//! `git_show`, `git_log` and `git_diff`: read-only git without a shell.
//!
//! Each tool runs `git` as a child process in the checkout root with
//! `--no-pager -c core.pager=cat`, so nothing can block on a pager. Revs and
//! ranges are validated against a fixed character set first (no leading `-`, no
//! whitespace, no control or shell characters), and paths are refused when they
//! start with `-` or resolve outside the checkout. There is no shell in between:
//! the arguments go straight to the child (spec §6.3).

use std::process::Stdio;

use review_sandbox::{OpError, Root};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};

/// Commits `git_log` lists when the caller gives no `n`.
const DEFAULT_LOG: usize = 20;
/// The most commits `git_log` lists.
const MAX_LOG: usize = 200;

/// The `git_show` tool.
pub(super) struct GitShow;

#[derive(Deserialize)]
struct ShowInput {
    rev: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait::async_trait]
impl Tool for GitShow {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "git_show",
            "Show one commit: its header and patch. With a path, show only that path's part \
             of the commit. To read a file as it was at a rev, pass '<rev>:<path>' as the rev \
             (e.g. 'HEAD~3:src/main.rs'). Use git_diff for a range of commits and git_log to \
             find a rev. Read-only; paths resolve against the checkout root.",
            json!({
                "type": "object",
                "properties": {
                    "rev": {
                        "type": "string",
                        "description": "The revision to show (e.g. 'HEAD', 'HEAD~2', \
                            'origin/main', a commit sha or a tag)."
                    },
                    "path": {
                        "type": "string",
                        "description": "Limit the output to this path. Relative paths resolve \
                            against the checkout root."
                    }
                },
                "required": ["rev"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: ShowInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        let rev = match rev_arg(&input.rev) {
            Ok(rev) => rev,
            Err(out) => return out,
        };
        let mut args = vec![
            "show".to_owned(),
            "--no-color".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-textconv".to_owned(),
            "--no-show-signature".to_owned(),
        ];
        args.push(rev);
        if let Some(path) = non_empty(input.path) {
            match repo_path(cx, &path) {
                Ok(path) => {
                    args.push("--".to_owned());
                    args.push(path);
                }
                Err(out) => return out,
            }
        }
        run(cx, args).await
    }
}

/// The `git_log` tool.
pub(super) struct GitLog;

#[derive(Deserialize)]
struct LogInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    n: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for GitLog {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "git_log",
            "List recent commits, newest first, as '<short sha> <date> <subject>'. With a path, \
             only the commits that touched that path. Use it to find a rev for git_show or \
             git_diff. Read-only; paths resolve against the checkout root.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Only list commits that touched this path. Relative \
                            paths resolve against the checkout root."
                    },
                    "n": {
                        "type": "integer",
                        "description": "How many commits to list. Defaults to 20, at most 200."
                    }
                }
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: LogInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        let n = match input.n {
            None => DEFAULT_LOG,
            Some(0) => return ToolOutput::error("n must be 1 or more"),
            Some(n) => n.min(MAX_LOG),
        };
        let mut args = vec![
            "log".to_owned(),
            "--no-color".to_owned(),
            "--no-show-signature".to_owned(),
            "--date=short".to_owned(),
            "--format=%h %ad %s".to_owned(),
            "-n".to_owned(),
            n.to_string(),
        ];
        if let Some(path) = non_empty(input.path) {
            match repo_path(cx, &path) {
                Ok(path) => {
                    args.push("--".to_owned());
                    args.push(path);
                }
                Err(out) => return out,
            }
        }
        run(cx, args).await
    }
}

/// The `git_diff` tool.
pub(super) struct GitDiff;

#[derive(Deserialize)]
struct DiffInput {
    range: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait::async_trait]
impl Tool for GitDiff {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "git_diff",
            "Show the diff of a range, e.g. 'main...HEAD', 'HEAD~3..HEAD' or a single rev \
             compared with the working tree. With a path, only that path's changes. Use it to \
             see what a branch changed; the PR's own patch is usually the better source. \
             Read-only; paths resolve against the checkout root.",
            json!({
                "type": "object",
                "properties": {
                    "range": {
                        "type": "string",
                        "description": "The range to diff: '<from>..<to>' or '<from>...<to>' \
                            (merge base), or a single rev to compare with the working tree."
                    },
                    "path": {
                        "type": "string",
                        "description": "Limit the diff to this path. Relative paths resolve \
                            against the checkout root."
                    }
                },
                "required": ["range"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: DiffInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        let range = match range_arg(&input.range) {
            Ok(range) => range,
            Err(out) => return out,
        };
        let mut args = vec![
            "diff".to_owned(),
            "--no-color".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-textconv".to_owned(),
        ];
        args.push(range);
        if let Some(path) = non_empty(input.path) {
            match repo_path(cx, &path) {
                Ok(path) => {
                    args.push("--".to_owned());
                    args.push(path);
                }
                Err(out) => return out,
            }
        }
        run(cx, args).await
    }
}

/// Whether `s` is a safe revision argument: non-empty, not an option, and made only
/// of characters git revs use (`[A-Za-z0-9._/~^@{}:-]`).
///
/// This is the barrier against option injection (`--output=/tmp/x`), shell
/// metacharacters (`HEAD;rm`) and whitespace splitting, not a check that the rev
/// exists: git itself rejects an unknown rev.
pub fn validate_rev(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("rev must not be empty".to_string());
    }
    if s.starts_with('-') {
        return Err(format!("rev must not start with '-': {s}"));
    }
    for c in s.chars() {
        if c.is_whitespace() || c.is_control() {
            return Err(format!(
                "rev contains whitespace or a control character: {s}"
            ));
        }
        if !(c.is_ascii_alphanumeric() || "._/~^@{}:-".contains(c)) {
            return Err(format!("rev contains an unsupported character {c:?}: {s}"));
        }
    }
    Ok(())
}

/// A validated `git_diff` range: a single rev, or one `..`/`...` with a safe rev on
/// each side.
fn validate_range(s: &str) -> Result<(), String> {
    let Some((from, rest)) = s.split_once("..") else {
        return validate_rev(s);
    };
    if rest.contains("..") {
        return Err(format!("range has more than one '..': {s}"));
    }
    let to = rest.strip_prefix('.').unwrap_or(rest);
    validate_rev(from)?;
    validate_rev(to)
}

/// `rev` after [`validate_rev`], or the tool error to return.
fn rev_arg(rev: &str) -> Result<String, ToolOutput> {
    validate_rev(rev).map_err(ToolOutput::error)?;
    Ok(rev.to_owned())
}

/// `range` after [`validate_range`], or the tool error to return.
fn range_arg(range: &str) -> Result<String, ToolOutput> {
    validate_range(range).map_err(ToolOutput::error)?;
    Ok(range.to_owned())
}

/// A path that may be empty (the model sending `""` means "no path").
fn non_empty(path: Option<String>) -> Option<String> {
    path.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty())
}

/// The checkout-relative, slash-separated form of a tool input path.
///
/// A leading `-` is refused so a path can never be read as an option, and every
/// path is resolved through [`Root`] (symlinks followed, `..` applied, whole-segment
/// containment) so it cannot leave the checkout. A path a newer revision deleted is
/// still accepted: [`Root::resolve_new`] applies the same containment without
/// requiring the file to exist.
fn repo_path(cx: &ToolCx, path: &str) -> Result<String, ToolOutput> {
    if path.starts_with('-') {
        return Err(ToolOutput::error(format!(
            "path must not start with '-': {path}"
        )));
    }
    let root = Root::new(&cx.repo_root)
        .map_err(|e| ToolOutput::error(format!("cannot open the checkout root: {e}")))?;
    let resolved = match root.resolve(path) {
        Ok(p) => p,
        Err(OpError::NotFound(_)) => match root.resolve_new(path) {
            Ok(p) => p,
            Err(e) => return Err(ToolOutput::error(e.to_string())),
        },
        Err(e) => return Err(ToolOutput::error(e.to_string())),
    };
    Ok(root.relative(&resolved))
}

/// Runs `git` in the checkout root with `args` and returns its stdout.
///
/// The child sees no shell, an empty stdin and no terminal prompt; a non-zero exit
/// is a tool error carrying git's stderr, because a bad rev or path is the model's
/// to fix.
async fn run(cx: &ToolCx, args: Vec<String>) -> ToolOutput {
    let dir = cx.repo_root.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .current_dir(&dir)
            .args([
                "--no-pager",
                "-c",
                "core.pager=cat",
                "-c",
                "core.quotePath=false",
            ])
            .args(&args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
    })
    .await;
    let output = match output {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return ToolOutput::error(format!("cannot run git: {e}")),
        Err(e) => return ToolOutput::error(format!("git task failed: {e}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return ToolOutput::error(if stderr.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            format!("git: {stderr}")
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        // Providers refuse an empty text block.
        return ToolOutput::text("(no output)");
    }
    ToolOutput::text(stdout)
}

#[cfg(test)]
mod tests {
    use super::{validate_range, validate_rev};

    #[test]
    fn range_allows_one_double_dot_on_each_side() {
        // A single rev is a diff against the working tree, so it is a valid range.
        for ok in [
            "HEAD~2..HEAD",
            "main...HEAD",
            "a1b2c3..HEAD",
            "HEAD",
            "HEAD~2",
        ] {
            assert!(validate_range(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "..",
            "..HEAD",
            "HEAD..",
            "a..b..c",
            "a..b...c",
            "-p..HEAD",
            "HEAD..main;rm",
        ] {
            assert!(validate_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn revs_reject_control_characters_and_odd_scripts() {
        for bad in ["HEAD\n", "HEAD\t1", "HEAD\0", "héad", "HEAD|cat", "HEAD$X"] {
            assert!(validate_rev(bad).is_err(), "{bad:?}");
        }
    }
}
