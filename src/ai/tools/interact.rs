//! The interaction tools (spec §6.4, §8.5): `ask_lookup`, `ask_user` and
//! `request_prepare`. They park the calling agent on the user (or the
//! pipeline) until an answer comes back; every other agent keeps running.
//!
//! `ask_lookup` reaches the pipeline through [`ToolCx::spawner`]; lookups
//! themselves never get this tool — their registry omits it (Task 15), so a
//! helper can never spawn another helper. `ask_user` posts a
//! [`Question`] through [`crate::ai::interaction::Interaction::ask_user`]
//! and stores the answer in the run's Q&A for reuse (spec §5.7).
//! `request_prepare` asks [`crate::ai::interaction::Interaction::approve_prepare`]
//! and, on approval, runs the stage's `prepare` closure — the repo's fixed
//! prepare commands, outside the sandbox — on the blocking pool.
//!
//! All three degrade to a tool error when their handle is missing
//! (`spawner` / `prepare` are `None`), so they are safe to register
//! everywhere [`all`] sweeps.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::interaction::Question;

/// `ask_lookup { question }` → [`Spawner::lookup`](crate::ai::stages::Spawner::lookup).
/// Lookups never get this tool — the pipeline leaves it out of their
/// registry (Task 15) — so a helper can never spawn another helper
/// (spec §5.4).
pub(super) struct AskLookup;
pub(super) struct AskUser;
pub(super) struct RequestPrepare;

/// The next [`Question::id`]: questions are numbered process-wide, which is
/// more unique than one run needs and never less.
static NEXT_QUESTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize)]
struct LookupInput {
    question: String,
}

#[derive(Deserialize)]
struct AskUserInput {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

#[derive(Deserialize)]
struct PrepareInput {
    reason: String,
}

#[async_trait::async_trait]
impl Tool for AskLookup {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "ask_lookup",
            "Ask a lookup helper a question about this repository, e.g. \
             \"how does this repo handle errors in HTTP handlers?\". The \
             helper reads with a fresh context and answers briefly with \
             file:line references, so you get the answer instead of twenty \
             file reads.",
            json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The repo question to answer." }
                },
                "required": ["question"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: LookupInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let Some(spawner) = cx.spawner.as_ref() else {
            return ToolOutput::error("no spawner attached");
        };
        ToolOutput::text(spawner.lookup(&cx.agent, &input.question).await)
    }
}

#[async_trait::async_trait]
impl Tool for AskUser {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "ask_user",
            "Ask the user a question that changes the review, e.g. \"is \
             empty `links` allowed for drafts?\". Never ask to confirm what \
             you can check yourself. `options` offers quick replies; without \
             it the user answers in free text. The answer is stored for the \
             rest of the session.",
            json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question, one sentence." },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Quick-reply choices; empty means free text." }
                },
                "required": ["question"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: AskUserInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let question = Question {
            id: NEXT_QUESTION_ID.fetch_add(1, Ordering::Relaxed),
            text: input.question,
            options: input.options,
        };
        let answer = cx.interaction.ask_user(&cx.agent, question.clone()).await;
        // The answer is the user's gift already; failing to persist it for
        // reuse (spec §5.7) must not swallow it.
        if let Err(error) = cx.run.add_qa(&question.text, &answer) {
            tracing::warn!(%error, "ask_user: storing the answer failed");
        }
        ToolOutput::text(answer)
    }
}

#[async_trait::async_trait]
impl Tool for RequestPrepare {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "request_prepare",
            "Ask the user to run the repo's prepare commands (e.g. \
             `cargo fetch`) outside the sandbox — the only way to fill \
             private dependency caches mid-run. You cannot choose what \
             runs; say why you need it.",
            json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "description": "Why the prepare commands are needed now." }
                },
                "required": ["reason"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: PrepareInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        if !cx
            .interaction
            .approve_prepare(&cx.agent, &input.reason)
            .await
        {
            return ToolOutput::error("the user declined");
        }
        let Some(prepare) = cx.prepare.clone() else {
            return ToolOutput::error("no prepare configured for this repo");
        };
        // The closure runs `sh -c` per command: blocking work, off the loop.
        let result = match tokio::task::spawn_blocking(move || prepare()).await {
            Ok(result) => result,
            Err(error) => {
                return ToolOutput::error(format!("the prepare run was cancelled: {error}"));
            }
        };
        match result {
            // `ToolOutput::text` cuts the combined output to MAX_TOOL_OUTPUT.
            Ok(output) => ToolOutput::text(output),
            Err(error) => ToolOutput::error(format!("the prepare commands failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use review_llm::ToolResultPart;
    use serde_json::json;

    use crate::ai::testing::{InteractionCall, ScriptedInteraction};
    use crate::ai::tools::{AgentRef, AgentRole, Tool, ToolCx, ToolOutput, all};

    fn tool(name: &str) -> Arc<dyn Tool> {
        all()
            .into_iter()
            .find(|t| t.definition().name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
    }

    fn text(out: &ToolOutput) -> &str {
        assert_eq!(out.parts.len(), 1, "{:?}", out.parts);
        match &out.parts[0] {
            ToolResultPart::Text { text } => text,
            other => panic!("not text: {other:?}"),
        }
    }

    struct Ctx {
        _dir: tempfile::TempDir,
        cx: ToolCx,
    }

    /// A reviewer's context over a temp-dir host with `interaction` attached
    /// and no spawner or prepare closure.
    fn ctx(interaction: Arc<ScriptedInteraction>) -> Ctx {
        let dir = crate::test_support::tempdir();
        let host = review_sandbox::LocalHost::new(dir.path()).unwrap();
        let cx = ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: AgentRef {
                id: 3,
                role: AgentRole::Reviewer,
                label: "reviewer:g1".to_string(),
            },
            interaction,
            allowed: Arc::new(std::sync::Mutex::new(Vec::new())),
            for_review: Arc::new(std::sync::Mutex::new(Vec::new())),
            snapshot: None,
            context: None,
            run: crate::test_support::ai_run_state(),
            assigned: Arc::new(Vec::new()),
            spawner: None,
            prepare: None,
            approval: Default::default(),
        };
        Ctx { _dir: dir, cx }
    }

    #[tokio::test]
    async fn ask_user_answer_is_stored_as_qa() {
        let user = Arc::new(ScriptedInteraction::new().answer("empty links are fine"));
        let ctx = ctx(user.clone());

        let out = tool("ask_user")
            .call(
                json!({"question": "is empty `links` allowed?", "options": ["yes", "no"]}),
                &ctx.cx,
            )
            .await;

        assert!(!out.is_error, "{:?}", out.parts);
        assert_eq!(text(&out), "empty links are fine");
        assert_eq!(
            ctx.cx.run.qa(),
            vec![(
                "is empty `links` allowed?".to_string(),
                "empty links are fine".to_string()
            )],
            "the answer is stored for reuse"
        );

        // The question carried the asking agent, the text and the options.
        let calls = user.calls();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            InteractionCall::Ask { agent, question } => {
                assert_eq!(agent.label, "reviewer:g1");
                assert_eq!(question.text, "is empty `links` allowed?");
                assert_eq!(question.options, vec!["yes".to_string(), "no".to_string()]);
                assert!(question.id > 0, "the question is numbered");
            }
            other => panic!("expected an Ask call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_without_options_asks_free_text() {
        let user = Arc::new(ScriptedInteraction::new().answer("the answer"));
        let ctx = ctx(user.clone());

        let out = tool("ask_user")
            .call(json!({"question": "which target?"}), &ctx.cx)
            .await;

        assert!(!out.is_error, "{:?}", out.parts);
        assert_eq!(text(&out), "the answer");
        match &user.calls()[0] {
            InteractionCall::Ask { question, .. } => assert!(question.options.is_empty()),
            other => panic!("expected an Ask call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_lookup_without_a_spawner_is_an_error() {
        let ctx = ctx(Arc::new(ScriptedInteraction::new()));

        let out = tool("ask_lookup")
            .call(json!({"question": "how are errors handled?"}), &ctx.cx)
            .await;

        assert!(out.is_error, "{:?}", out.parts);
        assert!(text(&out).contains("no spawner attached"), "{}", text(&out));
    }

    #[tokio::test]
    async fn ask_lookup_returns_the_spawner_answer() {
        struct StubSpawner;
        #[async_trait::async_trait]
        impl crate::ai::stages::Spawner for StubSpawner {
            async fn lookup(&self, from: &AgentRef, question: &str) -> String {
                format!("{} asked {question:?} -> use fmt_duration", from.label)
            }
        }

        let mut ctx = ctx(Arc::new(ScriptedInteraction::new()));
        ctx.cx.spawner = Some(Arc::new(StubSpawner));

        let out = tool("ask_lookup")
            .call(
                json!({"question": "is there a duration formatter?"}),
                &ctx.cx,
            )
            .await;

        assert!(!out.is_error, "{:?}", out.parts);
        assert!(
            text(&out).contains("reviewer:g1 asked \"is there a duration formatter?\""),
            "{}",
            text(&out)
        );
        assert!(text(&out).contains("use fmt_duration"), "{}", text(&out));
    }

    #[tokio::test]
    async fn request_prepare_declined_is_an_error_and_runs_nothing() {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = ran.clone();
        let user = Arc::new(ScriptedInteraction::new()); // queue empty → declines
        let mut ctx = ctx(user.clone());
        ctx.cx.prepare = Some(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("ran".to_string())
        }));

        let out = tool("request_prepare")
            .call(json!({"reason": "the build needs private deps"}), &ctx.cx)
            .await;

        assert!(out.is_error, "{:?}", out.parts);
        assert!(text(&out).contains("the user declined"), "{}", text(&out));
        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a declined prepare runs nothing"
        );
        assert!(
            matches!(&user.calls()[..], [InteractionCall::Prepare { agent, reason }]
                if agent.label == "reviewer:g1" && reason == "the build needs private deps"),
            "{:?}",
            user.calls()
        );
    }

    #[tokio::test]
    async fn request_prepare_approved_runs_closure() {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = ran.clone();
        let user = Arc::new(ScriptedInteraction::new().prepare(true));
        let mut ctx = ctx(user);
        ctx.cx.prepare = Some(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("ok: cargo fetch done".to_string())
        }));

        let out = tool("request_prepare")
            .call(json!({"reason": "cargo needs the registry"}), &ctx.cx)
            .await;

        assert!(!out.is_error, "{:?}", out.parts);
        assert!(
            text(&out).contains("ok: cargo fetch done"),
            "{}",
            text(&out)
        );
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn request_prepare_without_a_closure_is_an_error() {
        let user = Arc::new(ScriptedInteraction::new().prepare(true));
        let ctx = ctx(user);

        let out = tool("request_prepare")
            .call(json!({"reason": "deps"}), &ctx.cx)
            .await;

        assert!(out.is_error, "{:?}", out.parts);
        assert!(
            text(&out).contains("no prepare configured"),
            "{}",
            text(&out)
        );
    }
}
