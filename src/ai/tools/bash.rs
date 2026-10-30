//! `bash`: one command in the sandbox's persistent shell, gated by the
//! approval engine (`review_approval`, spec §8.1): commands it can prove safe
//! or the repo explicitly allows run silently, commands touching secret paths
//! are blocked without asking, everything else is refused back to the agent
//! or (in `Ask` mode) waits for the user's approval, and a refused command
//! never reaches the shell.

use std::sync::Arc;
use std::time::Duration;

use review_approval::model_stage::{
    ClassifierNote, MODEL_SOURCE, ModelStage, decide as decide_command,
};
use review_approval::{ApprovalConfig, SegmentPolicy, Verdict};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::approval_setup::ApprovalSetup;
use crate::ai::events::CommandDecision;
use crate::ai::interaction::ApprovalDecision;
use crate::persist::settings::ApprovalMode;

/// The `bash` tool.
pub(super) struct Bash;

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    command: String,
    #[serde(default)]
    restart: bool,
}

#[async_trait::async_trait]
impl Tool for Bash {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "bash",
            "Run a bash command and return its combined output and exit code. It runs from the \
             checkout root; pass paths to the command rather than 'cd'-ing; a leading 'cd' is fine but \
             rarely needed. Use it for builds, tests, inspecting files and running tools.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to run in the persistent shell (pipes, \
                            redirection and globbing work); it runs from the checkout root: pass \
                            paths to the command rather than 'cd'-ing (a leading 'cd' is fine but \
                            rarely needed). Environment and shell \
                            variables persist across calls. Combined stdout+stderr is returned; \
                            large output is truncated."
                    },
                    "restart": {
                        "type": "boolean",
                        "description": "Reset the persistent shell before running, discarding \
                            accumulated state (cwd, env vars, background jobs). Send with an \
                            empty command to just restart."
                    }
                }
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: Input = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        if input.command.trim().is_empty() && !input.restart {
            return ToolOutput::error("command is required unless restart is true");
        }
        // A restart with no command has nothing to classify: it only resets
        // the shell, which writes nothing.
        if !input.command.trim().is_empty()
            && let Some(out) = approve(&input.command, cx).await
        {
            return out;
        }
        // A zero timeout is the sandbox's default (120 s).
        let res = match cx
            .sandbox
            .exec(&input.command, input.restart, Duration::ZERO)
            .await
        {
            Ok(res) => res,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        // A non-zero exit is information the model needs, not a tool failure.
        let mut text = res.output;
        if res.exit_code != 0 {
            text.push_str(&format!("\n(exit code {})", res.exit_code));
        }
        if res.timed_out {
            text.push_str("\n(timed out)");
        }
        if text.is_empty() {
            // Providers refuse an empty text block.
            text.push_str("(no output)");
        }
        ToolOutput::text(text)
    }
}

/// Settles `command` under the run's [`ApprovalSetup`]: full trust runs it,
/// a deny rule blocks it, what the rules or the classifier vouch for runs,
/// and the rest is refused back to the agent (`Auto`) or put to the user
/// (`Ask`). Every outcome is announced as a [`CommandDecision`].
///
/// Returns `None` when the command may run and `Some(error)` when it must
/// not.
///
/// "For this review" memory is the exact line, whitespace-normalized and
/// otherwise verbatim: approving `touch a` never grants `touch a b`, and
/// approving a line with a wrapper in it never grants that wrapper. It is
/// checked before the engine, so the approved line runs even if the engine
/// would ask again.
///
/// [`ApprovalSetup`]: crate::ai::approval_setup::ApprovalSetup
async fn approve(command: &str, cx: &ToolCx) -> Option<ToolOutput> {
    let setup = cx.approval.current();
    // Full trust short-circuits everything below: no rules, no model, no
    // timeout machinery.
    if setup.trust_all() {
        cx.run
            .command_decided(&cx.agent, CommandDecision::Trust, None);
        return None;
    }
    let line = normalized_line(command);
    if cx.for_review_contains(&line) {
        cx.run
            .command_decided(&cx.agent, CommandDecision::Remembered, None);
        return None;
    }
    let (verdict, note) = decide(command, cx, &setup).await;
    let reason = match verdict {
        Verdict::Run { source } => {
            tracing::debug!(source, command, "bash command auto-approved");
            let decision = if source == MODEL_SOURCE {
                CommandDecision::Classifier
            } else {
                CommandDecision::Rules { source }
            };
            cx.run.command_decided(&cx.agent, decision, note);
            return None;
        }
        Verdict::Deny { reason } => {
            let out = ToolOutput::error(format!("the command was blocked: {reason}"));
            cx.run
                .command_decided(&cx.agent, CommandDecision::Blocked { reason }, note);
            return Some(out);
        }
        Verdict::Ask { reason } => reason,
    };
    if setup.mode == ApprovalMode::Auto {
        let out = ToolOutput::error(auto_denied_text(&reason));
        cx.run
            .command_decided(&cx.agent, CommandDecision::AutoDenied { reason }, note);
        return Some(out);
    }
    let decision = cx
        .interaction
        .approve_command(&cx.agent, command, &reason)
        .await;
    let (decided, out) = match decision {
        ApprovalDecision::Once => (CommandDecision::User { for_review: false }, None),
        ApprovalDecision::ForReview => {
            cx.remember_for_review(line);
            (CommandDecision::User { for_review: true }, None)
        }
        ApprovalDecision::Deny { reason } => {
            let mut text = "the user declined this command".to_string();
            if let Some(reason) = reason.filter(|reason| !reason.trim().is_empty()) {
                text.push_str(": ");
                text.push_str(&reason);
            }
            (CommandDecision::UserDenied, Some(ToolOutput::error(text)))
        }
    };
    cx.run.command_decided(&cx.agent, decided, note);
    out
}

/// What the agent is told when `Auto` refuses a command: why, and how to get
/// the same answer in a form the rules can check.
fn auto_denied_text(reason: &str) -> String {
    format!(
        "the command was not run: {reason}. Commands run without asking only when they can be \
         checked as read-only, and nobody is asked about the rest. Rewrite it in a simpler form: \
         one plain command or pipeline, no $(...) or loops, explicit file paths; or use the \
         read_file, grep, glob and git_diff tools instead."
    )
}

/// The engine's verdict for `command` under `setup`, and what the local
/// classifier thought of it, if it was consulted.
///
/// Without a classifier the engine is pure and fast, so it runs inline. With
/// one, the model may block (a GPU call, a lock), so the whole classification
/// runs on a blocking thread under [`ApprovalSetup::timeout`]; on a timeout or
/// a panicked thread only cached model outcomes apply and no new model call
/// is made. The slow thread is detached, never awaited past the timeout, and
/// the in-flight guard keeps later commands off the model until it finishes.
///
/// [`ApprovalSetup`]: crate::ai::approval_setup::ApprovalSetup
/// [`ApprovalSetup::timeout`]: crate::ai::approval_setup::ApprovalSetup::timeout
async fn decide(
    command: &str,
    cx: &ToolCx,
    setup: &Arc<ApprovalSetup>,
) -> (Verdict, Option<ClassifierNote>) {
    let setup = setup.clone();
    let allowed = cx.allowed_snapshot();
    let deny_paths = setup.effective_deny_paths();
    let repo_root = cx.repo_root.clone();
    let model_setup = setup.clone();
    let run = move |command: &str, ask_new: bool| {
        classify_with(command, &setup, &allowed, &deny_paths, &repo_root, ask_new)
    };
    // At most one model classification runs at a time: a hung one keeps its
    // thread, and more of them would starve tokio's shared blocking pool.
    let guard = if model_setup.classifier.is_some() {
        model_setup.try_begin_model()
    } else {
        None
    };
    if let Some(guard) = guard {
        let owned = command.to_string();
        let timeout = model_setup.timeout;
        let blocking = {
            let run = run.clone();
            tokio::task::spawn_blocking(move || {
                // Released when the model call really ends, not at the timeout.
                let _guard = guard;
                run(&owned, true)
            })
        };
        match tokio::time::timeout(timeout, blocking).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                tracing::warn!(%error, "command classifier thread failed; deciding without it");
                run(command, false)
            }
            Err(_) => {
                tracing::warn!(
                    ?timeout,
                    "command classifier timed out; deciding without it"
                );
                run(command, false)
            }
        }
    } else {
        // Another model call is in flight or none is installed: cached
        // outcomes stay in play but no new call starts.
        run(command, false)
    }
}

/// One synchronous decision: the rules, the opt-in capabilities, then (when
/// a classifier is installed) the model on the whole command. `ask_new` is
/// false once the command's time is up: only a cached model outcome applies.
fn classify_with(
    command: &str,
    setup: &ApprovalSetup,
    allowed: &[String],
    deny_paths: &[String],
    repo_root: &std::path::Path,
    ask_new: bool,
) -> (Verdict, Option<ClassifierNote>) {
    let extra: Vec<&dyn SegmentPolicy> = setup
        .capabilities
        .iter()
        .map(|policy| *policy as &dyn SegmentPolicy)
        .collect();
    let model = setup.classifier.as_ref().map(|classifier| ModelStage {
        classifier: classifier.as_ref(),
        cache: &setup.cache,
        ask_new,
        run_unsure: setup.mode == ApprovalMode::Lenient,
    });
    decide_command(
        command,
        &ApprovalConfig {
            allowed,
            deny_paths,
            cwd: Some(repo_root),
            extra: &extra,
        },
        model.as_ref(),
    )
}

/// The line "for this review" remembers.
///
/// Outside quotes whitespace collapses by class: a run of spaces and tabs
/// becomes one space, and a run that contains a newline (`\n` or `\r`) becomes
/// one `\n`, because outside quotes a newline separates commands and must not
/// turn into argument whitespace. Leading and trailing whitespace is dropped.
/// Inside quotes — and after a backslash escape, which applies outside quotes
/// and inside double quotes (inside single quotes it is literal) — characters
/// are copied verbatim. Quote and backslash characters are kept, so the result
/// is still the line the user approved.
///
/// Whitespace inside quotes is command text, not formatting: collapsing it
/// would make `sh -c 'echo  hi'` and `sh -c 'echo hi'` one approved line, and
/// would turn a newline inside `sh -c '…'` into a plain space.
fn normalized_line(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut pending: Option<char> = None;
    for c in command.chars() {
        if let Some(open) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' && open == '"' {
                escaped = true;
            } else if c == open {
                quote = None;
            }
            continue;
        }
        if escaped {
            // A backslash escapes exactly one character: keep both verbatim and
            // do not read the character as a separator or a quote.
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            c if c.is_whitespace() => {
                // A newline anywhere in the run wins: it separates commands
                // outside quotes, so it must not collapse into a space.
                if c == '\n' || c == '\r' {
                    pending = Some('\n');
                } else if pending.is_none() {
                    pending = Some(' ');
                }
            }
            '\'' | '"' | '\\' => {
                flush_separator(&mut out, &mut pending);
                out.push(c);
                if c == '\\' {
                    escaped = true;
                } else {
                    quote = Some(c);
                }
            }
            c => {
                flush_separator(&mut out, &mut pending);
                out.push(c);
            }
        }
    }
    out
}

/// Writes the pending separator, unless the output is empty (which trims
/// leading whitespace; a pending separator at the end is never written).
fn flush_separator(out: &mut String, pending: &mut Option<char>) {
    if let Some(separator) = pending.take()
        && !out.is_empty()
    {
        out.push(separator);
    }
}

#[cfg(test)]
mod tests {
    use super::normalized_line;

    #[test]
    fn normalization_collapses_only_unquoted_whitespace() {
        assert_eq!(
            normalized_line("  a   'b  c'\n\"d  e\""),
            "a 'b  c'\n\"d  e\""
        );
        assert_eq!(normalized_line("touch  a"), "touch a");
        assert_eq!(normalized_line("  touch a  "), "touch a");
        assert_eq!(normalized_line("  a\tb  "), "a b");
        // Quoted whitespace is command text: these are different lines.
        assert_eq!(normalized_line("sh -c 'echo  hi'"), "sh -c 'echo  hi'");
        assert_ne!(
            normalized_line("sh -c 'echo  hi'"),
            normalized_line("sh -c 'echo hi'")
        );
        assert_ne!(
            normalized_line("sh -c 'echo\na'"),
            normalized_line("sh -c 'echo a'")
        );
        // An unquoted newline separates commands; it is not a space.
        assert_eq!(normalized_line("a\nb"), "a\nb");
        assert_eq!(normalized_line("a \n\n b"), "a\nb");
        assert_eq!(normalized_line("a\r\n\nb"), "a\nb");
        assert_ne!(
            normalized_line("touch a rm x"),
            normalized_line("touch a\nrm x")
        );
        // Escaped whitespace is literal too, inside and outside quotes.
        assert_eq!(normalized_line("echo a\\  b"), "echo a\\  b");
        assert_ne!(normalized_line("echo a\\ b"), normalized_line("echo a b"));
        assert_eq!(normalized_line("echo \"a  b\""), "echo \"a  b\"");
        // An escaped quote is not a quote: the following text stays unquoted.
        assert_eq!(normalized_line("echo \\' a   b"), "echo \\' a b");
    }
}
