//! The agent loop (spec §9.2, §6.2).
//!
//! [`run_agent`] drives one agent: before every model call it waits out a
//! pause, delivers queued user notes, and elides old tool results once the
//! context estimate passes [`AgentSpec::elide_threshold`] of the model's
//! window. Each
//! turn streams the model (forwarding text deltas as
//! [`PipelineEvent::AgentText`]) and runs the reply's tool calls **in order**,
//! each raced against cancellation, answering unknown tools with an error
//! result. A turn without tool calls — or one where the finish tool succeeded
//! — consults [`AgentSpec::on_finish`]; the reminders it and the loop's
//! "call the finish tool" nudges share one allowance.
//!
//! Every assistant reply and every message the loop appends (notes, tool
//! results, reminders) is persisted through [`ToolCx::run`]'s store, so a
//! cancelled or failed run keeps its transcript. The system prompt and the
//! caller's `initial` messages are the caller's to persist.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use futures::{FutureExt as _, StreamExt as _};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use review_llm::{
    ContentBlock, GenerateRequest, Message, MessageRole, Reply, StreamEvent, ToolResultPart, Usage,
};

use crate::ai::events::PipelineEvent;
use crate::ai::model::Llm;
use crate::ai::tools::{AgentRef, Registry, ToolCx, ToolOutput};
use crate::persist::settings::ModelSettings;

/// The verdict an agent's finish check reaches (spec §9.2 rule 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishCheck {
    /// The agent is done; the loop stops [`Finished`](AgentStop::Finished).
    Accept,
    /// Not yet: push this text as a user-role reminder and keep going.
    Continue(String),
}

/// Everything one agent run needs: who it is, its prompt, its tools, and
/// when it may stop (spec §9.2).
pub struct AgentSpec {
    /// The agent these turns belong to.
    pub agent: AgentRef,
    /// The system prompt. Not persisted; the caller owns prompt text.
    pub system: String,
    /// The conversation the loop starts from (the task message, …). The loop
    /// persists only what it appends from here on.
    pub initial: Vec<Message>,
    /// The tools the agent may call.
    pub registry: Registry,
    /// Model calls this agent may make at most (spec §6.2).
    pub max_turns: u32,
    /// The tool that signals completion (e.g. `done`): a turn without tool
    /// calls while it is `Some` gets "Call {name} to finish." reminders
    /// instead of the finish check.
    pub finish_tool: Option<&'static str>,
    /// Asked when a turn ends without tool calls or the finish tool succeeded.
    pub on_finish: Arc<dyn Fn() -> FinishCheck + Send + Sync>,
    /// Combined allowance for `on_finish` reminders and finish-tool nudges.
    pub max_finish_reminders: u32,
    /// Fraction of the model's context window at which eliding starts
    /// (spec §6.2): the app passes `AiSettings::context_threshold`. Values
    /// outside `0.1..=0.95` are clamped before use.
    pub elide_threshold: f64,
}

/// The loop's control handles: cancel, pause, and the user's note queue.
#[derive(Debug)]
pub struct AgentControl {
    /// Cancels the run: checked between turns, and raced against the model
    /// stream and every tool call.
    pub cancel: CancellationToken,
    /// While `true`, the loop waits before each model call. A closed channel
    /// counts as unpaused.
    pub pause: watch::Receiver<bool>,
    /// Notes the user typed, drained into one message before each model call.
    /// A std mutex, never held across an await.
    pub notes: Arc<Mutex<VecDeque<String>>>,
}

/// Why an agent loop stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStop {
    /// The finish check accepted, or the reminder allowance ran out on it.
    Finished,
    /// `max_turns` model calls happened without finishing.
    MaxTurns,
    /// [`AgentControl::cancel`] fired.
    Cancelled,
    /// The model call failed after `review-llm`'s own retries.
    Failed(String),
    /// `finish_tool` was set and the model stopped calling it; the reminders
    /// ran out.
    NoFinish,
}

/// What a finished [`run_agent`] call reports.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentOutcome {
    /// Why the loop stopped.
    pub stop: AgentStop,
    /// Token usage summed over every turn.
    pub usage: Usage,
    /// The text of the last assistant reply that had any.
    pub last_text: String,
    /// Model calls made.
    pub turns: u32,
}

/// Runs one agent until it stops. See the module docs and spec §9.2.
pub async fn run_agent(
    spec: AgentSpec,
    cx: ToolCx,
    llm: Arc<dyn Llm>,
    mut ctl: AgentControl,
    events: mpsc::UnboundedSender<PipelineEvent>,
) -> AgentOutcome {
    let agent = spec.agent.clone();
    let mut history = spec.initial.clone();
    let mut usage = Usage::default();
    let mut last_text = String::new();
    let mut turns: u32 = 0;
    let mut reminders: u32 = 0;

    loop {
        // spec §9.2: cancellation is checked between turns as well as raced
        // against the stream and every tool call below.
        if ctl.cancel.is_cancelled() {
            return outcome(AgentStop::Cancelled, usage, last_text, turns);
        }
        if turns >= spec.max_turns {
            return outcome(AgentStop::MaxTurns, usage, last_text, turns);
        }
        // A skip or rerun issued while paused fires only this token, so the
        // wait is raced against it and the cancel is re-checked after.
        tokio::select! {
            _ = wait_while_paused(&mut ctl.pause) => {}
            _ = ctl.cancel.cancelled() => {}
        }
        if ctl.cancel.is_cancelled() {
            return outcome(AgentStop::Cancelled, usage, last_text, turns);
        }
        if let Some(note) = drain_notes(&ctl.notes) {
            persist_message(&cx, &agent, &note);
            history.push(note);
        }
        elide_if_needed(
            llm.model(),
            spec.elide_threshold.clamp(0.1, 0.95),
            &spec.system,
            &mut history,
        );

        let request = GenerateRequest {
            system: Some(spec.system.clone()),
            messages: history.clone(),
            tools: spec.registry.definitions(),
            ..GenerateRequest::default()
        };
        turns += 1;

        // One streamed model turn, raced against cancellation. Dropping this
        // future on cancel drops the stream; nothing of it is persisted.
        let stream_turn = async {
            let mut stream = llm.stream(request).await?;
            let mut reply: Option<Reply> = None;
            while let Some(event) = stream.next().await {
                match event? {
                    StreamEvent::TextDelta { text, .. } => {
                        let _ = events.send(PipelineEvent::AgentText {
                            agent: agent.clone(),
                            delta: text,
                        });
                    }
                    StreamEvent::Done(done) => reply = Some(done),
                    _ => {}
                }
            }
            reply.ok_or_else(|| {
                review_llm::Error::Decode("the model stream ended without a reply".to_string())
            })
        };
        let reply = tokio::select! {
            biased;
            _ = ctl.cancel.cancelled() => {
                return outcome(AgentStop::Cancelled, usage, last_text, turns);
            }
            result = stream_turn.fuse() => match result {
                Ok(reply) => reply,
                // spec §9.2 rule 6: review-llm already retried what it can.
                Err(error) => {
                    return outcome(
                        AgentStop::Failed(error.to_string()),
                        usage,
                        last_text,
                        turns,
                    );
                }
            },
        };

        usage = add_usage(usage, reply.usage);
        let text = reply.text();
        if !text.is_empty() {
            last_text = text;
        }
        let reply_message = reply.to_message();
        persist_message(&cx, &agent, &reply_message);
        history.push(reply_message);

        // Tool calls run strictly in order (spec §9.2), each raced against
        // cancellation; an unknown tool is an error result, not a stop.
        let calls: Vec<(String, String, serde_json::Value)> = reply
            .tool_uses()
            .map(|call| {
                (
                    call.id.to_string(),
                    call.name.to_string(),
                    call.input.clone(),
                )
            })
            .collect();
        let mut finish_ok = false;
        let mut results = Vec::with_capacity(calls.len());
        for (id, name, input) in &calls {
            let summary = tool_call_summary(name, input);
            let _ = events.send(PipelineEvent::ToolCall {
                agent: agent.clone(),
                name: name.clone(),
                summary,
            });
            let output = match spec.registry.get(name) {
                Some(tool) => {
                    tokio::select! {
                        biased;
                        _ = ctl.cancel.cancelled() => {
                            return outcome(AgentStop::Cancelled, usage, last_text, turns);
                        }
                        output = tool.call(input.clone(), &cx) => output,
                    }
                }
                None => ToolOutput::error(format!("unknown tool: {name}")),
            };
            if Some(name.as_str()) == spec.finish_tool && !output.is_error {
                finish_ok = true;
            }
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: output.parts,
                is_error: output.is_error,
            });
        }
        if !results.is_empty() {
            let results_message = Message::user(results);
            persist_message(&cx, &agent, &results_message);
            history.push(results_message);
        }

        // spec §9.2 rule 3: the finish check.
        if !calls.is_empty() && !finish_ok {
            continue;
        }
        if calls.is_empty()
            && let Some(name) = spec.finish_tool
        {
            // The model stopped without calling its finish tool.
            if reminders >= spec.max_finish_reminders {
                return outcome(AgentStop::NoFinish, usage, last_text, turns);
            }
            reminders += 1;
            let reminder = Message::user_text(format!("Call {name} to finish."));
            persist_message(&cx, &agent, &reminder);
            history.push(reminder);
            continue;
        }
        match (spec.on_finish)() {
            FinishCheck::Accept => return outcome(AgentStop::Finished, usage, last_text, turns),
            FinishCheck::Continue(text) => {
                if reminders >= spec.max_finish_reminders {
                    // Out of reminders: done enough; the caller records skips.
                    return outcome(AgentStop::Finished, usage, last_text, turns);
                }
                reminders += 1;
                let reminder = Message::user_text(text);
                persist_message(&cx, &agent, &reminder);
                history.push(reminder);
            }
        }
    }
}

fn outcome(stop: AgentStop, usage: Usage, last_text: String, turns: u32) -> AgentOutcome {
    AgentOutcome {
        stop,
        usage,
        last_text,
        turns,
    }
}

/// Sums one turn's usage into the running total, field by field.
fn add_usage(total: Usage, turn: Usage) -> Usage {
    Usage {
        input: total.input + turn.input,
        output: total.output + turn.output,
        cache_read: total.cache_read + turn.cache_read,
        cache_write: total.cache_write + turn.cache_write,
        reasoning: total.reasoning + turn.reasoning,
    }
}

/// Waits while `pause` holds `true`; a closed channel counts as unpaused.
async fn wait_while_paused(pause: &mut watch::Receiver<bool>) {
    loop {
        if !*pause.borrow_and_update() {
            return;
        }
        if pause.changed().await.is_err() {
            return;
        }
    }
}

/// Drains the note queue into one user message, or `None` when it is empty.
/// The std mutex is released before the message is returned, never held
/// across an await.
fn drain_notes(notes: &Arc<Mutex<VecDeque<String>>>) -> Option<Message> {
    let drained: Vec<String> = {
        let mut queue = notes.lock().unwrap_or_else(|e| e.into_inner());
        queue.drain(..).collect()
    };
    if drained.is_empty() {
        return None;
    }
    let text = drained
        .iter()
        .map(|note| format!("[note from the user] {note}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(Message::user_text(text))
}

/// Appends one transcript message through the run's store. A failure is
/// logged and skipped: losing the transcript must not abort the review.
fn persist_message(cx: &ToolCx, agent: &AgentRef, message: &Message) {
    let role = match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    };
    let Ok(content_json) = serde_json::to_string(&message.content) else {
        tracing::warn!("ai agent: transcript content did not serialize");
        return;
    };
    if let Err(error) =
        cx.run
            .store()
            .message_append(cx.run.run_id(), &agent.label, role, &content_json)
    {
        tracing::warn!(%error, agent = %agent.label, "ai agent: transcript append failed");
    }
}

/// The loop's token estimate: serialized bytes of the system prompt and all
/// message content, over four (spec §6.2 rule 4).
fn estimate_tokens(system: &str, messages: &[Message]) -> usize {
    let mut bytes = system.len();
    for message in messages {
        if let Ok(json) = serde_json::to_string(&message.content) {
            bytes += json.len();
        }
    }
    bytes / 4
}

/// Whether `content` is already an elision stub.
fn is_elided(content: &[ToolResultPart]) -> bool {
    matches!(content, [ToolResultPart::Text { text }] if text.starts_with("[elided: "))
}

/// The `tool_use` ids of the last two assistant messages: their results are
/// the "latest two turns" and are never elided.
fn protected_ids(messages: &[Message]) -> HashSet<&str> {
    let mut ids = HashSet::new();
    let mut assistants = 0;
    for message in messages.iter().rev() {
        if message.role != MessageRole::Assistant {
            continue;
        }
        assistants += 1;
        if assistants > 2 {
            break;
        }
        for block in &message.content {
            if let ContentBlock::ToolUse { id, .. } = block {
                ids.insert(id.as_str());
            }
        }
    }
    ids
}

/// The name and compact input JSON of the `tool_use` with id `id`.
fn find_tool_use<'a>(messages: &'a [Message], id: &str) -> Option<(&'a str, String)> {
    messages.iter().find_map(|message| {
        message.content.iter().find_map(|block| match block {
            ContentBlock::ToolUse {
                id: use_id,
                name,
                input,
            } if use_id == id => Some((name.as_str(), input.to_string())),
            _ => None,
        })
    })
}

/// One elision stub: `[elided: <tool> <first 80 chars of the input>]`.
fn elided_text(name: &str, input_json: &str) -> String {
    let head: String = input_json.chars().take(80).collect();
    format!("[elided: {name} {head}]")
}

/// Replaces the text of the oldest `tool_result`s, oldest first, until the
/// estimate fits `threshold × context_window` or nothing is left that may be
/// elided. `tool_use` blocks, ids and the result↔call pairing are
/// never removed or reordered (spec §6.2, trim_history semantics).
fn elide_if_needed(model: &ModelSettings, threshold: f64, system: &str, messages: &mut [Message]) {
    let budget = threshold * f64::from(model.context_window);
    while estimate_tokens(system, messages) as f64 > budget {
        let protected = protected_ids(messages);
        let mut target: Option<(usize, usize, String)> = None;
        'results: for (mi, message) in messages.iter().enumerate() {
            for (bi, block) in message.content.iter().enumerate() {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                else {
                    continue;
                };
                if protected.contains(tool_use_id.as_str()) || is_elided(content) {
                    continue;
                }
                let stub = match find_tool_use(messages, tool_use_id) {
                    Some((name, input)) => elided_text(name, &input),
                    None => elided_text("unknown", ""),
                };
                target = Some((mi, bi, stub));
                break 'results;
            }
        }
        let Some((mi, bi, stub)) = target else {
            break; // nothing left to elide: over budget is the best we can do
        };
        if let ContentBlock::ToolResult { content, .. } = &mut messages[mi].content[bi] {
            *content = vec![ToolResultPart::Text { text: stub }];
        }
    }
}

/// Tools whose input is an agent's result (its summary, verdicts, review
/// draft, re-checked states): the UI shows them as cards, so they travel
/// whole, up to [`MAX_RESULT_SUMMARY`] chars.
const RESULT_TOOLS: &[&str] = &[
    "done",
    "submit_verdicts",
    "submit_review_draft",
    "submit_prior_states",
];

/// The longest result-tool input a [`PipelineEvent::ToolCall`] carries.
const MAX_RESULT_SUMMARY: usize = 16_000;

/// What a [`PipelineEvent::ToolCall`] carries of `input`: the first 120
/// chars, or a result tool's whole input.
fn tool_call_summary(name: &str, input: &serde_json::Value) -> String {
    let cap = if RESULT_TOOLS.contains(&name) {
        MAX_RESULT_SUMMARY
    } else {
        120
    };
    input.to_string().chars().take(cap).collect()
}
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use review_llm::{ContentBlock, ToolResultPart};

    use super::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
    use crate::ai::events::PipelineEvent;
    use crate::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
    use crate::ai::tools::{AgentRef, AgentRole, Registry, Tool, ToolCx, ToolOutput};
    use crate::persist::settings::ModelSettings;

    fn model() -> ModelSettings {
        ModelSettings {
            provider: "work".into(),
            model: "claude-test".into(),
            context_window: 100_000,
            max_tokens: 4096,
            reasoning: None,
        }
    }

    fn agent() -> AgentRef {
        AgentRef {
            id: 1,
            role: AgentRole::Reviewer,
            label: "reviewer 1".to_string(),
        }
    }

    /// A `ToolCx` over a temp-dir [`review_sandbox::LocalHost`] and a fresh
    /// run state, as in the tool tests.
    fn cx_in(dir: &tempfile::TempDir) -> ToolCx {
        let host = review_sandbox::LocalHost::new(dir.path()).expect("local host");
        ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: agent(),
            interaction: Arc::new(ScriptedInteraction::new()),
            allowed: Arc::new(std::sync::Mutex::new(Vec::new())),
            for_review: Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot: None,
            context: None,
            run: crate::test_support::ai_run_state(),
            assigned: Arc::new(Vec::new()),
            spawner: None,
            prepare: None,
            approval: Default::default(),
        }
    }

    /// A spec with the defaults every test shares: one task message, ten
    /// turns, no finish tool, an accepting finish check, and the pre-setting
    /// elision threshold of 0.8.
    fn base_spec(registry: Registry) -> AgentSpec {
        AgentSpec {
            agent: agent(),
            system: "# agent: reviewer 1\nReview the PR.".to_string(),
            initial: vec![review_llm::Message::user_text("review the PR")],
            registry,
            max_turns: 10,
            finish_tool: None,
            on_finish: Arc::new(|| FinishCheck::Accept),
            max_finish_reminders: 2,
            elide_threshold: 0.8,
        }
    }

    /// An unpaused control plus the pieces the test keeps: the pause sender
    /// and the note queue.
    fn control() -> (
        AgentControl,
        watch::Sender<bool>,
        Arc<std::sync::Mutex<VecDeque<String>>>,
    ) {
        control_paused(false)
    }

    /// [`control`] starting in state `paused`.
    fn control_paused(
        paused: bool,
    ) -> (
        AgentControl,
        watch::Sender<bool>,
        Arc<std::sync::Mutex<VecDeque<String>>>,
    ) {
        let notes = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let (tx, rx) = watch::channel(paused);
        (
            AgentControl {
                cancel: CancellationToken::new(),
                pause: rx,
                notes: notes.clone(),
            },
            tx,
            notes,
        )
    }

    /// Echoes its `msg` back and records the call order.
    struct Echo {
        calls: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for Echo {
        fn definition(&self) -> review_llm::Tool {
            review_llm::Tool::new(
                "echo",
                "echo the message back",
                json!({
                    "type": "object",
                    "properties": { "msg": { "type": "string" } },
                    "required": ["msg"],
                }),
            )
            .expect("valid schema")
        }

        async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
            let msg = input
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(msg.clone());
            ToolOutput::text(msg)
        }
    }

    /// Sleeps an hour: only cancellation ends it.
    struct Slow;

    #[async_trait]
    impl Tool for Slow {
        fn definition(&self) -> review_llm::Tool {
            review_llm::Tool::new(
                "slow",
                "sleeps forever",
                json!({ "type": "object", "properties": {} }),
            )
            .expect("valid schema")
        }

        async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolOutput {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            ToolOutput::text("woke up")
        }
    }

    /// Returns a 4000-byte text result, to blow a tiny context window up.
    struct Big;

    #[async_trait]
    impl Tool for Big {
        fn definition(&self) -> review_llm::Tool {
            review_llm::Tool::new(
                "big",
                "returns a huge result",
                json!({
                    "type": "object",
                    "properties": { "n": { "type": "integer" } },
                    "required": ["n"],
                }),
            )
            .expect("valid schema")
        }

        async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolOutput {
            ToolOutput::text("x".repeat(4000))
        }
    }

    fn echo() -> (Arc<dyn Tool>, Arc<std::sync::Mutex<Vec<String>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Arc::new(Echo {
                calls: calls.clone(),
            }),
            calls,
        )
    }

    /// Drains the event channel.
    fn drained(rx: &mut mpsc::UnboundedReceiver<PipelineEvent>) -> Vec<PipelineEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// The persisted transcript of the run's `reviewer 1`, as roles.
    fn transcript_roles(run: &Arc<crate::ai::review::RunState>) -> Vec<String> {
        run.store()
            .messages(run.run_id(), "reviewer 1")
            .expect("transcript")
            .into_iter()
            .map(|(role, _)| role)
            .collect()
    }

    /// The text of a message's single text block.
    fn text_of(message: &review_llm::Message) -> &str {
        match message.content.as_slice() {
            [ContentBlock::Text { text }] => text,
            other => panic!("not one text block: {other:?}"),
        }
    }

    #[tokio::test]
    async fn runs_tools_in_order_and_finishes_on_text() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let run = cx.run.clone();
        let (echo_tool, calls) = echo();
        let spec = base_spec(Registry::new(vec![echo_tool]));
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::ToolCalls(vec![
                    ("echo".into(), json!({"msg": "one"})),
                    ("echo".into(), json!({"msg": "two"})),
                ]),
                ScriptStep::Text("all done".into()),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(out.stop, AgentStop::Finished);
        assert_eq!(out.turns, 2);
        assert_eq!(out.last_text, "all done");
        assert_eq!(
            *calls.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["one", "two"],
            "tools ran in call order"
        );

        // The second request carries both results, in order, paired by id.
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        let results = &requests[1]
            .messages
            .last()
            .expect("results message")
            .content;
        assert_eq!(
            results,
            &vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: vec![ToolResultPart::Text { text: "one".into() }],
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tu_2".into(),
                    content: vec![ToolResultPart::Text { text: "two".into() }],
                    is_error: false,
                },
            ]
        );

        // The UI saw both tool calls, then the text.
        assert_eq!(
            drained(&mut rx),
            vec![
                PipelineEvent::ToolCall {
                    agent: agent(),
                    name: "echo".into(),
                    summary: "{\"msg\":\"one\"}".into(),
                },
                PipelineEvent::ToolCall {
                    agent: agent(),
                    name: "echo".into(),
                    summary: "{\"msg\":\"two\"}".into(),
                },
                PipelineEvent::AgentText {
                    agent: agent(),
                    delta: "all done".into(),
                },
            ]
        );

        // The transcript holds everything the loop exchanged, in order; the
        // caller's initial message and the system prompt are the caller's.
        assert_eq!(
            transcript_roles(&run),
            vec!["assistant", "user", "assistant"]
        );
    }

    #[tokio::test]
    async fn finish_check_continue_reprompts_then_accepts() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let checks = Arc::new(AtomicU32::new(0));
        let counter = checks.clone();
        let mut spec = base_spec(Registry::new(vec![]));
        spec.on_finish = Arc::new(move || {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                FinishCheck::Continue("keep going".to_string())
            } else {
                FinishCheck::Accept
            }
        });
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("hmm".into()),
                ScriptStep::Text("done now".into()),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(out.stop, AgentStop::Finished);
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert_eq!(out.last_text, "done now");
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            text_of(requests[1].messages.last().expect("reminder")),
            "keep going",
            "the Continue text is the next turn's last message"
        );
    }

    #[tokio::test]
    async fn finish_check_continue_exhausts_the_allowance_then_finishes() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let run = cx.run.clone();
        let checks = Arc::new(AtomicU32::new(0));
        let counter = checks.clone();
        let mut spec = base_spec(Registry::new(vec![]));
        spec.on_finish = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            FinishCheck::Continue("keep going".to_string())
        });
        spec.max_finish_reminders = 2;
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("still thinking".into()),
                ScriptStep::Text("almost there".into()),
                ScriptStep::Text("stubborn as ever".into()),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(
            out.stop,
            AgentStop::Finished,
            "an exhausted Continue allowance finishes; the caller records skips"
        );
        assert_eq!(out.turns, 3);
        assert_eq!(
            checks.load(Ordering::SeqCst),
            3,
            "on_finish is asked on every tool-free turn"
        );

        let requests = llm.requests();
        assert_eq!(requests.len(), 3, "the third Continue stops the loop");
        // The shared counter let exactly two reminders through; the third
        // Continue found the allowance spent and was never pushed.
        let reminders = requests[2]
            .messages
            .iter()
            .filter(
                |m| matches!(&m.content[..], [ContentBlock::Text { text }] if text == "keep going"),
            )
            .count();
        assert_eq!(reminders, 2, "two reminder messages ride along, no more");
        assert_eq!(
            transcript_roles(&run),
            vec!["assistant", "user", "assistant", "user", "assistant"],
            "the two pushed reminders are persisted; the third Continue is not"
        );
    }

    #[tokio::test]
    async fn missing_finish_tool_gets_one_reminder_per_allowance_then_no_finish() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let mut spec = base_spec(Registry::new(vec![]));
        spec.finish_tool = Some("done");
        spec.max_finish_reminders = 2;
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("I think it is fine".into()),
                ScriptStep::Text("really, it is fine".into()),
                ScriptStep::Text("last word: fine".into()),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(out.stop, AgentStop::NoFinish);
        assert_eq!(out.turns, 3, "one reminder per allowance, then stop");
        let requests = llm.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            assert_eq!(
                text_of(request.messages.last().expect("reminder")),
                "Call done to finish."
            );
        }
    }

    #[tokio::test]
    async fn max_turns_stops() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let (echo_tool, _calls) = echo();
        let mut spec = base_spec(Registry::new(vec![echo_tool]));
        spec.max_turns = 2;
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::ToolCalls(vec![("echo".into(), json!({"msg": "a"}))]),
                ScriptStep::ToolCalls(vec![("echo".into(), json!({"msg": "b"}))]),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(out.stop, AgentStop::MaxTurns);
        assert_eq!(out.turns, 2);
        assert_eq!(llm.requests().len(), 2, "exactly max_turns model calls");
    }

    #[tokio::test]
    async fn cancel_during_a_tool_stops_promptly() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let spec = base_spec(Registry::new(vec![Arc::new(Slow)]));
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![ScriptStep::ToolCalls(vec![("slow".into(), json!({}))])],
        ));
        let (ctl, _pause, _notes) = control();
        let cancel = ctl.cancel.clone();
        let (tx, _rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(run_agent(
            spec,
            cx,
            llm as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();

        let out = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cancelled within 2 s")
            .expect("the task did not panic");
        assert_eq!(out.stop, AgentStop::Cancelled);
        assert_eq!(out.turns, 1);
    }

    #[tokio::test]
    async fn pause_holds_before_the_next_model_call() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let spec = base_spec(Registry::new(vec![]));
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![ScriptStep::Text("hi".into())],
        ));
        let (ctl, pause, _notes) = control_paused(true);
        let (tx, _rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            llm.requests().is_empty(),
            "paused: the model has not been called"
        );

        pause.send(false).expect("the loop still watches pause");
        let out = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("resumed within 2 s")
            .expect("the task did not panic");
        assert_eq!(out.stop, AgentStop::Finished);
        assert_eq!(llm.requests().len(), 1);
    }

    #[tokio::test]
    async fn notes_are_delivered_on_the_next_turn() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let run = cx.run.clone();
        let spec = base_spec(Registry::new(vec![]));
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![ScriptStep::Text("ok".into())],
        ));
        let (ctl, _pause, notes) = control();
        notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back("x".to_string());
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(out.stop, AgentStop::Finished);
        let requests = llm.requests();
        assert_eq!(requests.len(), 1);
        let messages = &requests[0].messages;
        assert_eq!(messages.len(), 2, "the note rides along before the call");
        assert_eq!(
            text_of(messages.last().expect("note message")),
            "[note from the user] x"
        );
        assert_eq!(transcript_roles(&run), vec!["user", "assistant"]);
    }

    #[tokio::test]
    async fn eliding_keeps_tool_use_result_pairs() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let spec = base_spec(Registry::new(vec![Arc::new(Big)]));
        let mut tiny = model();
        tiny.context_window = 200;
        let window = tiny.context_window;
        let llm = Arc::new(ScriptedLlm::new(
            tiny,
            vec![
                ScriptStep::ToolCalls(vec![("big".into(), json!({"n": 1}))]),
                ScriptStep::ToolCalls(vec![("big".into(), json!({"n": 2}))]),
                ScriptStep::ToolCalls(vec![("big".into(), json!({"n": 3}))]),
                ScriptStep::Text("done".into()),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;
        assert_eq!(out.stop, AgentStop::Finished);

        let last = llm.requests().pop().expect("four requests");
        assert!(
            super::estimate_tokens(&last.system.clone().unwrap_or_default(), &last.messages)
                > (0.8 * f64::from(window)) as usize,
            "the run is over budget even after eliding: eliding had to do something"
        );

        let mut seen_tool_uses: Vec<&str> = Vec::new();
        let mut elided = Vec::new();
        for message in &last.messages {
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { id, .. } => seen_tool_uses.push(id),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        assert!(
                            seen_tool_uses.contains(&tool_use_id.as_str()),
                            "tool_result {tool_use_id} lost its tool_use"
                        );
                        if let [ToolResultPart::Text { text }] = content.as_slice()
                            && text.starts_with("[elided: ")
                        {
                            elided.push(text.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(
            elided,
            vec!["[elided: big {\"n\":1}]".to_string()],
            "the oldest result, and only it (the latest two turns are protected), is stubbed"
        );
    }

    #[tokio::test]
    async fn a_low_elide_threshold_elides_where_the_default_would_not() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let mut spec = base_spec(Registry::new(vec![Arc::new(Big)]));
        // Below the 0.1 clamp floor, so this also pins the clamp: the budget
        // is 0.1 × 8000 = 800 tokens, not 0.05 × 8000.
        spec.elide_threshold = 0.05;
        let mut roomy = model();
        roomy.context_window = 8_000;
        // Three 4000-byte results ≈ 3k tokens: under the default 0.8 budget
        // (6400) the history would ride unelided.
        let llm = Arc::new(ScriptedLlm::new(
            roomy,
            vec![
                ScriptStep::ToolCalls(vec![("big".into(), json!({"n": 1}))]),
                ScriptStep::ToolCalls(vec![("big".into(), json!({"n": 2}))]),
                ScriptStep::ToolCalls(vec![("big".into(), json!({"n": 3}))]),
                ScriptStep::Text("done".into()),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;
        assert_eq!(out.stop, AgentStop::Finished);

        let last = llm.requests().pop().expect("four requests");
        let elided = last.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { content, .. }
                        if matches!(
                            content.as_slice(),
                            [ToolResultPart::Text { text }] if text.starts_with("[elided: ")
                        )
                )
            })
        });
        assert!(
            elided,
            "the configured low threshold elides old tool results the default would keep"
        );
    }

    #[tokio::test]
    async fn unknown_tool_answers_an_error_and_the_loop_continues() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let spec = base_spec(Registry::new(vec![]));
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::ToolCalls(vec![("nope".into(), json!({}))]),
                ScriptStep::Text("fine then".into()),
            ],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(
            out.stop,
            AgentStop::Finished,
            "an unknown tool is not fatal"
        );
        let requests = llm.requests();
        let results = &requests[1].messages.last().expect("results").content;
        match &results[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "tu_1", "the result references its call");
                assert!(is_error, "unknown tools answer with an error");
                let [ToolResultPart::Text { text }] = content.as_slice() else {
                    panic!("one text part: {content:?}")
                };
                assert!(text.contains("nope"), "{text}");
            }
            other => panic!("not a tool result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_error_fails_the_run() {
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let spec = base_spec(Registry::new(vec![]));
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![ScriptStep::Fail(review_llm::Error::Timeout)],
        ));
        let (ctl, _pause, _notes) = control();
        let (tx, _rx) = mpsc::unbounded_channel();

        let out = run_agent(
            spec,
            cx,
            llm.clone() as Arc<dyn crate::ai::model::Llm>,
            ctl,
            tx,
        )
        .await;

        assert_eq!(
            out.stop,
            AgentStop::Failed("request timed out".to_string()),
            "the error's Display text is the message"
        );
        assert_eq!(out.turns, 1);
        assert!(out.last_text.is_empty());
    }
}
