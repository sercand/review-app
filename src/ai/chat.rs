//! The chat agent (spec §6.5, Task 7): the user's handle on a review run.
//!
//! One long-lived task serves every message of a run. The hub spawns it on
//! the first [`Command::AiUserMessage`](crate::runtime::Command::AiUserMessage)
//! and every later message goes through [`ChatHandle`]. A message is one
//! agent turn sequence: the model streams text, may call tools (including
//! the pipeline-steering ones in [`crate::ai::tools::steer`]), and stops when
//! it answers without a tool call. Then the task waits for the next message.
//!
//! The transcript lives in `ai_messages` under the [`CHAT_LABEL`] row, so a
//! chat task created after a restart reloads the conversation instead of
//! starting blank. Every turn begins with a compact status digest (stages,
//! groups, running agents, finding counts, open question ids, the brief's
//! intent) from [`SteerCtx::digest`] — never a transcript.
//!
//! The chat runs under the run's cancellation token but with its own
//! unpaused watch receiver: typing never pauses the review (spec §6.5), and
//! cancelling the run ends the chat with it. After the run, the read tools
//! keep working and the steering tools answer
//! [`crate::ai::tools::steer::FINISHED`]; `record_finding` still writes a new
//! draft.

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use review_llm::{ContentBlock, Message, MessageRole};

use crate::ai::agent::{AgentControl, AgentSpec, FinishCheck, run_agent};
use crate::ai::events::PipelineEvent;
use crate::ai::hub::ChatEvent;
use crate::ai::model::Llm;
use crate::ai::pipeline::PipelineControl;
use crate::ai::prompts::system_prompt;
use crate::ai::review::RunState;
use crate::ai::stages;
use crate::ai::store::Store;
use crate::ai::tools::steer::{self, SteerCtx};
use crate::ai::tools::{AgentRef, AgentRole, Registry, Tool, ToolCx};
use crate::ai::ui_interaction::UiInteraction;
use crate::runtime::{Event, EventSender, SessionId};

/// The transcript row and agent label every chat turn runs under.
pub const CHAT_LABEL: &str = "chat";

/// Model calls one user message may take before the turn is cut off. A
/// steering turn is short; a "review this file again" turn can use tools for
/// a while.
const MAX_TURNS: u32 = 24;

/// The hub's sender for a run's chat messages: cloned cheaply, one per run.
#[derive(Clone, Debug)]
pub struct ChatHandle {
    tx: mpsc::UnboundedSender<String>,
}

impl ChatHandle {
    /// A handle and the receiver [`run_chat`] consumes. The receiver lives in
    /// the [`crate::ai::hub::RunHandle`] until the first message spawns the
    /// task.
    pub fn channel() -> (ChatHandle, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ChatHandle { tx }, rx)
    }

    /// Queue one user message. `false` when the chat task is gone (the run
    /// was closed).
    pub fn send(&self, text: String) -> bool {
        self.tx.send(text).is_ok()
    }
}

/// Everything [`run_chat`] needs, captured once at [`crate::ai::hub::AiHub::start`].
/// A restart test can build one directly: it needs no live pipeline, only a
/// run row and its store.
pub struct ChatSetup {
    /// The chat model: the run's `models.default` (spec §6.5).
    pub llm: Arc<dyn Llm>,
    /// The run's findings, for the record tools and the status digest.
    pub run: Arc<RunState>,
    /// The run's store, shared with the transcript.
    pub store: Arc<Store>,
    /// The PR's session row.
    pub session_id: i64,
    /// The run whose transcript is the chat history.
    pub run_id: i64,
    /// The chat's `ToolCx` template: snapshot, sandbox, context,
    /// interaction. The chat agent label is set per turn.
    pub cx: ToolCx,
    /// The live pipeline, for the steering tools. `None` when the chat has
    /// no pipeline behind it (a restart test, or a run that has been
    /// finished); every steering tool then answers "finished".
    pub control: Option<PipelineControl>,
    /// The run's parked questions, for `answer_question`.
    pub interaction: Arc<UiInteraction>,
    /// Whether the pipeline is still running; cleared by the hub's forwarder.
    pub live: Arc<AtomicBool>,
    /// The run's current session, rebound by attach (spec §10.2).
    pub session: Arc<Mutex<SessionId>>,
    /// Where the chat events go.
    pub events: EventSender,
    /// Whether `bash` is unavailable (spec §8.2); drops it from the registry.
    pub bash_disabled: Option<String>,
    /// The run's context threshold, for the agent loop's elision.
    pub elide_threshold: f64,
    /// Cancels the chat with the run.
    pub cancel: CancellationToken,
}

/// Serve one run's chat until its channel closes or the run is cancelled.
pub async fn run_chat(mut messages: mpsc::UnboundedReceiver<String>, setup: ChatSetup) {
    while let Some(text) = messages.recv().await {
        if setup.cancel.is_cancelled() {
            return;
        }
        run_turn(&setup, text).await;
        emit(&setup, ChatEvent::TurnDone);
    }
}

/// One user message: digest, history, one agent turn sequence, echoes.
async fn run_turn(setup: &ChatSetup, text: String) {
    emit(setup, ChatEvent::User { text: text.clone() });
    let agent = chat_agent();
    let steer = Arc::new(SteerCtx::new(
        setup.control.clone(),
        setup.live.clone(),
        setup.interaction.clone(),
        setup.store.clone(),
        setup.session_id,
        setup.run_id,
    ));

    // The digest is the user-role preamble of every turn (spec §6.5).
    let prompt = format!("{}\n\n{}", steer.digest(), text);
    let mut history = load_history(&setup.store, setup.run_id);
    persist_user(&setup.store, setup.run_id, &prompt);
    history.push(Message::user_text(prompt));

    let spec = AgentSpec {
        agent: agent.clone(),
        system: system_prompt(AgentRole::Chat, CHAT_LABEL),
        initial: history,
        registry: Registry::new(chat_tools(setup.bash_disabled.as_deref(), steer)),
        max_turns: MAX_TURNS,
        finish_tool: None,
        on_finish: Arc::new(|| FinishCheck::Accept),
        max_finish_reminders: 0,
        elide_threshold: setup.elide_threshold,
    };

    // The loop emits pipeline events; this turn maps the ones the chat
    // stream shows. `tx` moves into `run_agent`, so the forwarder ends when
    // the turn does.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = setup.events.clone();
    let session = setup.session.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let chat = match event {
                PipelineEvent::AgentText { delta, .. } => ChatEvent::Text { delta },
                PipelineEvent::ToolCall { name, summary, .. } => {
                    ChatEvent::ToolCall { name, summary }
                }
                _ => continue,
            };
            let session = *session.lock().unwrap_or_else(|e| e.into_inner());
            events.send(Event::AiChat {
                session,
                event: chat,
            });
        }
    });

    // Typing never pauses the run (spec §6.5): the chat gets its own,
    // always-false pause receiver. Cancellation is the run's own token.
    let (_pause_tx, pause) = watch::channel(false);
    let control = AgentControl {
        cancel: setup.cancel.clone(),
        pause,
        notes: Arc::new(Mutex::new(VecDeque::new())),
    };
    let mut cx = setup.cx.clone();
    cx.agent = agent;
    let _ = run_agent(spec, cx, setup.llm.clone(), control, tx).await;
    let _ = forwarder.await;
}

/// The chat's stable agent reference: the transcript row and system prompt
/// label are [`CHAT_LABEL`]; id 0 because it is never one of the run's
/// numbered agents.
pub fn chat_agent() -> AgentRef {
    AgentRef {
        id: 0,
        role: AgentRole::Chat,
        label: CHAT_LABEL.to_string(),
    }
}

/// The chat's toolset (spec §6.5): the reviewer's read-only and record tools,
/// minus `done` and `request_prepare`, plus the pipeline-steering tools.
fn chat_tools(bash_disabled: Option<&str>, steer: Arc<SteerCtx>) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = stages::reviewer_tools(bash_disabled)
        .into_iter()
        .filter(|tool| !matches!(tool.definition().name.as_str(), "done" | "request_prepare"))
        .collect();
    tools.extend(steer::tools(steer));
    tools
}

/// The stored chat transcript as review-llm messages. A row that no longer
/// parses is skipped: a stale transcript must not wedge the next turn.
fn load_history(store: &Store, run_id: i64) -> Vec<Message> {
    store
        .messages(run_id, CHAT_LABEL)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(role, content_json)| {
            let content = serde_json::from_str::<Vec<ContentBlock>>(&content_json).ok()?;
            let role = match role.as_str() {
                "assistant" => MessageRole::Assistant,
                _ => MessageRole::User,
            };
            Some(Message {
                role,
                content,
                cache_breakpoint: false,
            })
        })
        .collect()
}

/// Persist one chat user turn. A failure is logged and skipped, exactly as
/// the agent loop treats its own appends.
fn persist_user(store: &Store, run_id: i64, text: &str) {
    let content = vec![ContentBlock::text(text)];
    let Ok(content_json) = serde_json::to_string(&content) else {
        tracing::warn!("ai chat: user message did not serialize");
        return;
    };
    if let Err(error) = store.message_append(run_id, CHAT_LABEL, "user", &content_json) {
        tracing::warn!(%error, "ai chat: transcript append failed");
    }
}

/// Send one chat event under the run's current session.
fn emit(setup: &ChatSetup, event: ChatEvent) {
    let session = *setup.session.lock().unwrap_or_else(|e| e.into_inner());
    setup.events.send(Event::AiChat { session, event });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::{ChatHandle, ChatSetup, run_chat};
    use crate::ai::events::PipelineEvent;
    use crate::ai::hub::ChatEvent;
    use crate::ai::model::Llm;
    use crate::ai::review::RunState;
    use crate::ai::snapshot::PrSnapshot;
    use crate::ai::store::{PrKey, RunKind, Store};
    use crate::ai::testing::{ScriptStep, ScriptedLlm};
    use crate::ai::tools::ToolCx;
    use crate::ai::ui_interaction::UiInteraction;
    use crate::persist::settings::ModelSettings;
    use crate::runtime::{Event, EventReceiver, EventSender, SessionId, event_channel};

    fn model() -> ModelSettings {
        ModelSettings {
            provider: "work".into(),
            model: "claude-test".into(),
            context_window: 100_000,
            max_tokens: 4096,
            reasoning: None,
        }
    }

    /// A setup over `run`'s store, as a restart would rebuild it: no
    /// pipeline control, no live run.
    fn setup(
        run: &Arc<RunState>,
        dir: &tempfile::TempDir,
        llm: Arc<ScriptedLlm>,
        events: &EventSender,
    ) -> ChatSetup {
        let host = review_sandbox::LocalHost::new(dir.path()).expect("local host");
        let store: Arc<Store> = run.store().clone();
        let session = Arc::new(Mutex::new(SessionId(1)));
        let interaction = UiInteraction::new(events.clone(), session.clone(), Vec::new());
        let cx = ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: super::chat_agent(),
            interaction: interaction.clone(),
            allowed: Arc::new(Mutex::new(Vec::new())),
            for_review: Arc::new(Mutex::new(Vec::new())),
            snapshot: None,
            context: None,
            run: run.clone(),
            assigned: Arc::new(Vec::new()),
            spawner: None,
            prepare: None,
        };
        ChatSetup {
            llm: llm as Arc<dyn Llm>,
            run: run.clone(),
            store,
            session_id: run.session_id(),
            run_id: run.run_id(),
            cx,
            control: None,
            interaction,
            live: Arc::new(AtomicBool::new(false)),
            session,
            events: events.clone(),
            bash_disabled: None,
            elide_threshold: 0.8,
            cancel: CancellationToken::new(),
        }
    }

    /// Collect this turn's chat events up to and including `TurnDone`,
    /// discarding everything else on the stream.
    fn wait_chat_turn(rx: &EventReceiver) -> Vec<ChatEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let event = rx
                .recv_timeout(remaining)
                .expect("a chat turn finished in time");
            if let Event::AiChat { event, .. } = event {
                let done = matches!(event, ChatEvent::TurnDone);
                events.push(event);
                if done {
                    return events;
                }
            }
        }
    }

    /// The text of a message whose content is one text block.
    fn text_of(message: &review_llm::Message) -> &str {
        match message.content.as_slice() {
            [review_llm::ContentBlock::Text { text }] => text,
            other => panic!("not one text block: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chat_history_survives_restart() {
        let run = crate::test_support::ai_run_state();
        let dir = crate::test_support::tempdir();
        let mut scripts = BTreeMap::new();
        scripts.insert(
            "chat".to_string(),
            vec![
                ScriptStep::Text("the helper is new".into()),
                ScriptStep::Text("all good".into()),
            ],
        );
        let llm = Arc::new(ScriptedLlm::per_agent(model(), scripts));
        let (tx, rx) = event_channel();

        // First chat task: one answer.
        let (handle, task) = {
            let (handle, messages) = ChatHandle::channel();
            let setup = setup(&run, &dir, llm.clone(), &tx);
            (handle, tokio::spawn(run_chat(messages, setup)))
        };
        assert!(handle.send("what changed?".to_string()));
        wait_chat_turn(&rx);
        drop(handle);
        task.await.expect("the first chat task ends");

        // The app restarts: a new task, the same store and run row.
        let (handle, task) = {
            let (handle, messages) = ChatHandle::channel();
            let setup = setup(&run, &dir, llm.clone(), &tx);
            (handle, tokio::spawn(run_chat(messages, setup)))
        };
        assert!(handle.send("and the tests?".to_string()));
        wait_chat_turn(&rx);
        drop(handle);
        task.await.expect("the restarted chat task ends");

        // The second turn carried the first turn's user message and reply.
        let requests = llm.requests_for("chat");
        assert_eq!(requests.len(), 2, "one model call per turn");
        let second = &requests[1].messages;
        assert!(
            second
                .iter()
                .any(|message| text_of(message).contains("what changed?")),
            "the first user message rode along: {second:?}"
        );
        assert!(
            second
                .iter()
                .any(|message| text_of(message) == "the helper is new"),
            "the first reply rode along: {second:?}"
        );
        assert!(
            second
                .iter()
                .any(|message| text_of(message).contains("and the tests?")),
            "the new message is the last turn: {second:?}"
        );
    }

    /// The run has finished (`live` is false, no pipeline control): the
    /// chat's `record_finding` still writes a draft into the run's state —
    /// app state only, never GitHub — persists it, and reports it through
    /// the run's event channel so the tab shows the new draft.
    #[tokio::test(flavor = "multi_thread")]
    async fn chat_after_run_can_record_a_new_draft() {
        let dir = crate::test_support::tempdir();
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let session_id = store
            .session_id(&PrKey {
                host: "github.com".to_string(),
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 7,
            })
            .expect("session row");
        let run_id = store
            .run_begin(session_id, RunKind::Full, None, "sha_head")
            .expect("run row");
        // The run's pipeline channel: after the run the hub's forwarder
        // still reads it, so the test does.
        let (pipeline_tx, mut pipeline_rx) = tokio::sync::mpsc::unbounded_channel();
        let run = Arc::new(RunState::new(
            store.clone(),
            session_id,
            run_id,
            pipeline_tx,
        ));

        let mut scripts = BTreeMap::new();
        scripts.insert(
            "chat".to_string(),
            vec![
                ScriptStep::ToolCalls(vec![(
                    "record_finding".to_string(),
                    serde_json::json!({
                        "path": "a.rs",
                        "line": 12,
                        "severity": "bug",
                        "title": "nil config after the split",
                        "body": "cfg can be nil on this path",
                        "evidence": [{
                            "path": "a.rs", "line": 12,
                            "note": "the guard was removed in this hunk",
                        }],
                    }),
                )]),
                ScriptStep::Text("recorded a new draft".into()),
            ],
        );
        let llm = Arc::new(ScriptedLlm::per_agent(model(), scripts));
        let (tx, rx) = event_channel();

        let (handle, messages) = ChatHandle::channel();
        let mut setup = setup(&run, &dir, llm.clone(), &tx);
        // The chat reads the PR through the run's frozen snapshot.
        setup.cx.snapshot = Some(Arc::new(PrSnapshot {
            repo: crate::test_support::repo(),
            number: 7,
            metadata: crate::test_support::meta("sha_head"),
            files: vec![crate::test_support::loaded(
                "a.rs",
                "@@ -10,5 +10,5 @@\n c1\n c2\n c3\n c4\n c5\n",
            )],
            threads: Vec::new(),
            conversation: Vec::new(),
            checks: Vec::new(),
        }));
        let task = tokio::spawn(run_chat(messages, setup));

        assert!(handle.send("there is a nil path we missed".to_string()));
        let turn = wait_chat_turn(&rx);
        let tools: Vec<&str> = turn
            .iter()
            .filter_map(|event| match event {
                ChatEvent::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tools, vec!["record_finding"], "{turn:?}");
        assert!(
            turn.contains(&ChatEvent::Text {
                delta: "recorded a new draft".into()
            }),
            "{turn:?}"
        );
        drop(handle);
        task.await.expect("the chat task ends");

        // The draft exists as a finding of the run: in state, in the store,
        // and reported through the run's channel as FindingAdded.
        let findings = run.findings();
        assert_eq!(findings.len(), 1, "the chat recorded a new draft");
        assert_eq!(findings[0].title, "nil config after the split");
        assert_eq!(findings[0].agent.label, super::CHAT_LABEL);
        assert_eq!(
            store
                .finding_list(session_id)
                .expect("findings")
                .iter()
                .filter(|finding| finding.id == findings[0].id)
                .count(),
            1,
            "the draft is persisted under the session's latest run"
        );
        assert!(
            matches!(
                pipeline_rx.try_recv(),
                Ok(PipelineEvent::FindingAdded { id }) if id == findings[0].id
            ),
            "FindingAdded rides the run's channel for the UI to fold in"
        );
    }
}
