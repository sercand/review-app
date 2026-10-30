//! Tree walk: statements to [`Segment`]s.
//!
//! Every node kind the walker understands is listed in [`HANDLED`]; any other
//! kind is a [`Bail`]: the specific one from [`bail_for`] for compound
//! constructs, else [`Bail::Unsupported`]. [`emit`] flattens lists, pipelines,
//! negations and redirected statements into one segment per simple command.
//!
//! A `for NAME in word…; do …; done` loop over literal words is unrolled: its
//! body is emitted once per word with `$NAME` replaced, so every iteration is
//! classified as the command it really is. Other loops and conditionals bail.

use tree_sitter::Node;

use super::unsupported;
use super::vars::{Vars, is_name, literal_value};
use super::words::{continuation_only, is_word_kind, text, word_value};
use crate::model::{Bail, RedirOp, Redirect, Segment, Separator};

/// Maximum statement nesting before the walker gives up.
const MAX_DEPTH: usize = 64;

/// `for` loops inside `for` loops the walker unrolls.
const MAX_LOOP_NESTING: usize = 2;

/// Words one `for` loop may iterate over.
const MAX_LOOP_WORDS: usize = 32;

/// Segments a command may turn into once its loops are unrolled.
const MAX_SEGMENTS: usize = 1024;

/// Builtins that can assign any shell variable (`read N`, `eval`, `. f`).
/// After one runs, no variable is known. `printf` only with `-v`.
const ASSIGNING: &[&str] = &[
    "read",
    "mapfile",
    "readarray",
    "getopts",
    "declare",
    "typeset",
    "local",
    "export",
    "readonly",
    "unset",
    "eval",
    "source",
    ".",
    "let",
    "enable",
];

/// POSIX special builtins: in POSIX mode a `NAME=value` prefix on them
/// stays set after the command.
const SPECIAL_BUILTINS: &[&str] = &[
    ":", ".", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset", "source",
];

/// Variables bash sets or changes by itself (`$RANDOM`, `$PWD` after `cd`,
/// `$REPLY`, …), or that are read-only: an assignment to one does not make
/// its value known.
const SHELL_VARS: &[&str] = &[
    "RANDOM",
    "SRANDOM",
    "SECONDS",
    "LINENO",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "HISTCMD",
    "FUNCNAME",
    "PIPESTATUS",
    "OPTIND",
    "OPTARG",
    "OPTERR",
    "PWD",
    "OLDPWD",
    "REPLY",
    "COLUMNS",
    "LINES",
    "SHLVL",
    "UID",
    "EUID",
    "PPID",
    "GROUPS",
    "DIRSTACK",
    "HOSTNAME",
    "HOSTTYPE",
    "MACHTYPE",
    "OSTYPE",
    "COPROC",
    "MAPFILE",
    "_",
];

/// Walk state: the variables known so far and the `for` loop nesting.
struct Cx {
    vars: Vars,
    loops: usize,
}

impl Cx {
    /// A copy for a construct that may not run (or runs in a subshell).
    fn nested(&self) -> Cx {
        Cx {
            vars: self.vars.scope(),
            loops: self.loops,
        }
    }

    /// Back from a [`Cx::nested`] walk: what it assigned is unknown here.
    fn forget(&mut self, inner: Cx) {
        self.vars.forget_touched(inner.vars);
    }
}

/// Statement-level node kinds the walker understands. Word kinds are listed in
/// `words::WORD_KINDS`.
pub(super) const HANDLED: &[&str] = &[
    "program",
    "list",
    "pipeline",
    "negated_command",
    "redirected_statement",
    "command",
    "command_name",
    "variable_assignment",
    "variable_assignments",
    "file_redirect",
    "comment",
    "for_statement",
];

/// Words bash reserves at command position. tree-sitter only treats most of
/// them as keywords inside their own compound statement; after `!` or `time`
/// they come out as plain words, which must never become a simple command.
const RESERVED: &[&str] = &[
    "{", "}", "if", "then", "else", "elif", "fi", "while", "until", "do", "done", "for", "in",
    "case", "esac", "select", "function", "coproc", "[[", "]]", "!",
];

/// Whether `node` (a word about to join `seg`) would sit in command position,
/// possibly after `time [-p] [--]`, and is an unquoted reserved word.
fn reserved_at_command_position(seg: &Segment, node: Node, src: &str) -> Result<bool, Bail> {
    let leading = seg
        .argv
        .iter()
        .all(|a| matches!(a.as_str(), "time" | "-p" | "--"));
    Ok(leading && node.kind() == "word" && RESERVED.contains(&text(node, src)?))
}

/// The bail for a node kind with no explicit rule.
fn bail_for(node: Node) -> Bail {
    match node.kind() {
        "subshell" => Bail::Subshell,
        "compound_statement" => Bail::BraceGroup,
        "if_statement"
        | "while_statement"
        | "for_statement"
        | "c_style_for_statement"
        | "case_statement"
        | "select_statement" => Bail::CompoundCommand,
        "function_definition" => Bail::FunctionDef,
        "heredoc_redirect" => Bail::Heredoc,
        "herestring_redirect" => Bail::HereString,
        _ => unsupported(node),
    }
}

fn ensure_handled(node: Node) -> Result<(), Bail> {
    let kind = node.kind();
    if HANDLED.contains(&kind) || is_word_kind(kind) {
        Ok(())
    } else {
        Err(bail_for(node))
    }
}

pub(super) fn segments(root: Node, src: &str) -> Result<Vec<Segment>, Bail> {
    ensure_handled(root)?;
    let mut out: Vec<Segment> = Vec::new();
    let mut cx = Cx {
        vars: Vars::default(),
        loops: 0,
    };
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if !child.is_named() {
            // Separator tokens between top-level statements.
            match child.kind() {
                ";" | "\n" => continue,
                ";;" => return Err(Bail::Syntax),
                "&" => return Err(Bail::Background),
                _ => return Err(unsupported(child)),
            }
        }
        if child.kind() == "comment" {
            continue;
        }
        // Top-level statements are separated by `;` or a newline.
        if let Some(prev) = out.last_mut() {
            prev.follows = Separator::Semi;
        }
        // Top-level statements always run, so their assignments are known
        // to the statements after them.
        emit(child, src, 0, &mut out, &mut cx)?;
    }
    Ok(out)
}

/// Append the segments of statement `node`. The last one keeps
/// `Separator::End`; the caller sets the separator that follows it.
fn emit(
    node: Node,
    src: &str,
    depth: usize,
    out: &mut Vec<Segment>,
    cx: &mut Cx,
) -> Result<(), Bail> {
    if depth > MAX_DEPTH {
        return Err(Bail::Unsupported("depth"));
    }
    ensure_handled(node)?;
    match node.kind() {
        "command" => {
            let seg = command(node, src, &cx.vars)?;
            after_command(&seg, &mut cx.vars);
            out.push(seg);
        }
        "list" | "pipeline" => sequence(node, src, depth, out, cx)?,
        "negated_command" => {
            // `!` only flips the exit status; the command is classified as is.
            let mut inner = cx.nested();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match (child.is_named(), child.kind()) {
                    (false, "!") => {}
                    (true, _) => emit(child, src, depth + 1, out, &mut inner)?,
                    _ => return Err(unsupported(child)),
                }
            }
            cx.forget(inner);
        }
        "redirected_statement" => {
            let mut inner = cx.nested();
            redirected(node, src, depth, out, &mut inner)?;
            cx.forget(inner);
        }
        "variable_assignment" => {
            let mut seg = empty_segment();
            seg.assigns
                .push(standalone_assignment(node, src, &mut cx.vars)?);
            out.push(seg);
        }
        // Several bare assignments: `A=1 B=2`.
        "variable_assignments" => {
            let mut seg = empty_segment();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if !child.is_named() || child.kind() != "variable_assignment" {
                    return Err(unsupported(child));
                }
                seg.assigns
                    .push(standalone_assignment(child, src, &mut cx.vars)?);
            }
            out.push(seg);
        }
        "for_statement" => for_loop(node, src, depth, out, cx)?,
        _ => return Err(unsupported(node)),
    }
    if out.len() > MAX_SEGMENTS {
        return Err(Bail::Unsupported("for_statement"));
    }
    Ok(())
}

/// One piece of a flattened list: a statement (with its nesting depth) or
/// the operator before the next one.
enum Item<'t> {
    Stmt(Node<'t>, usize),
    Sep(Separator),
}

/// The statements and operators of `node`, in order. tree-sitter nests
/// `a && b && c` as `((a && b) && c)`; inside a list the nested lists are
/// flattened so an assignment early in a `&&` chain reaches the end of it.
fn flatten<'t>(node: Node<'t>, depth: usize, items: &mut Vec<Item<'t>>) -> Result<(), Bail> {
    if depth > MAX_DEPTH {
        return Err(Bail::Unsupported("depth"));
    }
    let is_list = node.kind() == "list";
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            items.push(Item::Sep(match child.kind() {
                "&&" => Separator::And,
                "||" => Separator::Or,
                "|" => Separator::Pipe,
                "|&" => Separator::PipeBoth,
                ";" | "\n" => Separator::Semi,
                "&" => return Err(Bail::Background),
                _ => return Err(unsupported(child)),
            }));
        } else if child.kind() == "comment" {
            continue;
        } else if is_list && child.kind() == "list" {
            flatten(child, depth + 1, items)?;
        } else {
            items.push(Item::Stmt(child, depth + 1));
        }
    }
    Ok(())
}

/// `list` and `pipeline`: statements joined by `&&`, `||`, `|`, `|&`.
///
/// An assignment is known to the statements after it only while every
/// operator so far is `&&` (they run only if it ran). From the first `||` or
/// `;` on, the rest may run whether or not anything before did, so nothing
/// assigned in the list is known any more; every pipeline element runs in a
/// subshell, so nothing is known there either. Outside the list nothing it
/// assigned is known: an earlier `&&` may have failed.
fn sequence(
    node: Node,
    src: &str,
    depth: usize,
    out: &mut Vec<Segment>,
    cx: &mut Cx,
) -> Result<(), Bail> {
    let mut items = Vec::new();
    flatten(node, depth, &mut items)?;
    let mut local = cx.nested();
    let mut certain = node.kind() == "list";
    let mut pending: Option<Separator> = None;
    for item in items {
        let (child, child_depth) = match item {
            Item::Sep(sep) => {
                if certain && sep != Separator::And {
                    certain = false;
                    let before = std::mem::replace(&mut local, cx.nested());
                    local.forget(before);
                }
                pending = Some(sep);
                continue;
            }
            Item::Stmt(child, d) => (child, d),
        };
        if let (Some(sep), Some(prev)) = (pending.take(), out.last_mut()) {
            prev.follows = sep;
        }
        if certain {
            emit(child, src, child_depth, out, &mut local)?;
        } else {
            let mut inner = local.nested();
            emit(child, src, child_depth, out, &mut inner)?;
            local.forget(inner);
        }
    }
    cx.forget(local);
    Ok(())
}

/// `redirected_statement`: a body (usually a command) plus redirects. With no
/// body (`> f`) the segment is just the redirects.
fn redirected(
    node: Node,
    src: &str,
    depth: usize,
    out: &mut Vec<Segment>,
    cx: &mut Cx,
) -> Result<(), Bail> {
    let body = node.child_by_field_name("body");
    let start = out.len();
    let mut redirects = Vec::new();
    let mut trailing = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            return Err(unsupported(child));
        }
        if Some(child.id()) == body.map(|b| b.id()) {
            emit(child, src, depth + 1, out, cx)?;
        } else if child.kind() == "comment" {
            continue;
        } else {
            let (r, words) = redirect(child, src, &cx.vars)?;
            redirects.push(r);
            trailing.extend(words);
        }
    }
    if body.is_none() {
        out.push(empty_segment());
    }
    // When the last command of a list or pipeline has a redirect, tree-sitter
    // wraps the whole list as the body. Bash binds the redirect to the last
    // simple command, so that is where it (and any trailing words) goes. A
    // redirect on a whole `for` loop lands on its last segment too: it is
    // checked once, and a write it is refused for refuses the command.
    if out.len() <= start {
        return Err(Bail::Unsupported("redirected_statement"));
    }
    let seg = out.last_mut().ok_or(Bail::Syntax)?;
    seg.redirects.extend(redirects);
    for (v, d) in trailing {
        seg.argv.push(v);
        seg.dynamic.push(d);
    }
    Ok(())
}

/// `for NAME in w1 w2 …; do BODY; done`, unrolled: for each word, a bare
/// `NAME=word` segment (so the engine screens the name like any assignment:
/// `for PATH in …` is refused) and then BODY with `$NAME` known to be that
/// word. The words must be literal (no expansion, glob or tilde); at most
/// [`MAX_LOOP_WORDS`] of them and [`MAX_LOOP_NESTING`] loops deep. `select`
/// (also a `for_statement` to tree-sitter), a loop with no `in` list and
/// anything else bail. After the loop, whatever its body assigned (and
/// NAME) is unknown.
fn for_loop(
    node: Node,
    src: &str,
    depth: usize,
    out: &mut Vec<Segment>,
    cx: &mut Cx,
) -> Result<(), Bail> {
    if cx.loops >= MAX_LOOP_NESTING || node.child(0).map(|c| c.kind()) != Some("for") {
        return Err(Bail::CompoundCommand);
    }
    let mut name: Option<&str> = None;
    let mut words: Vec<String> = Vec::new();
    let mut body = None;
    let mut seen_in = false;
    let mut cursor = node.walk();
    for (i, child) in node.children(&mut cursor).enumerate() {
        match (child.is_named(), child.kind()) {
            (false, "for") if i == 0 => {}
            (false, "in") if name.is_some() && !seen_in => seen_in = true,
            (false, ";" | "\n") if body.is_none() => {}
            (true, "variable_name") if name.is_none() => name = Some(text(child, src)?),
            (true, "do_group") if seen_in && body.is_none() => body = Some(child),
            (true, "comment") => {}
            (true, kind) if is_word_kind(kind) && seen_in && body.is_none() => {
                words.push(loop_word(child, src)?);
            }
            _ => return Err(Bail::CompoundCommand),
        }
    }
    let (Some(name), Some(body)) = (name, body) else {
        return Err(Bail::CompoundCommand);
    };
    if !is_name(name) || words.is_empty() || words.len() > MAX_LOOP_WORDS {
        return Err(Bail::CompoundCommand);
    }
    let start = out.len();
    let mut after = cx.nested();
    for word in &words {
        let mut iteration = Cx {
            vars: cx.vars.scope(),
            loops: cx.loops + 1,
        };
        record(&mut iteration.vars, name, Some(word.clone()));
        if out.len() > start
            && let Some(prev) = out.last_mut()
        {
            prev.follows = Separator::Semi;
        }
        let mut seg = empty_segment();
        seg.assigns.push((name.to_string(), word.clone()));
        out.push(seg);
        do_group(body, src, depth, out, &mut iteration)?;
        after.forget(iteration);
    }
    after.vars.set(name, None);
    cx.forget(after);
    Ok(())
}

/// One word of a `for` list: its literal value, or a bail when bash would
/// expand it (`$x`, `*`, `~`, `{a,b}`).
fn loop_word(node: Node, src: &str) -> Result<String, Bail> {
    let (value, dynamic) = word_value(node, src, &Vars::default())?;
    if dynamic || value.contains(['$', '`', '*', '?', '[', '~']) {
        return Err(Bail::CompoundCommand);
    }
    Ok(value)
}

/// The statements of a loop's `do … done`, in order.
fn do_group(
    node: Node,
    src: &str,
    depth: usize,
    out: &mut Vec<Segment>,
    cx: &mut Cx,
) -> Result<(), Bail> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            match child.kind() {
                "do" | "done" | ";" | "\n" => continue,
                "&" => return Err(Bail::Background),
                _ => return Err(unsupported(child)),
            }
        }
        if child.kind() == "comment" {
            continue;
        }
        if let Some(prev) = out.last_mut() {
            prev.follows = Separator::Semi;
        }
        emit(child, src, depth + 1, out, cx)?;
    }
    Ok(())
}

/// Set `name` to `value` in `vars`, unless bash manages that variable
/// itself; then it is unknown.
fn record(vars: &mut Vars, name: &str, value: Option<String>) {
    let managed = SHELL_VARS.contains(&name) || name.starts_with("BASH");
    vars.set(name, value.filter(|_| !managed));
}

/// A `NAME=value` statement: the segment's assignment, and what it makes
/// known. Only `NAME=literal` is remembered; `NAME+=x`, `NAME[i]=x` and any
/// run-time value make the name unknown.
fn standalone_assignment(node: Node, src: &str, vars: &mut Vars) -> Result<(String, String), Bail> {
    let (name, value, dynamic) = assignment(node, src, vars)?;
    let name_node = node.child_by_field_name("name").ok_or(Bail::Syntax)?;
    let plain = name_node.kind() == "variable_name"
        && src
            .get(name_node.end_byte()..)
            .is_some_and(|rest| rest.starts_with('=') && !rest.starts_with("=("));
    let base = name.split('[').next().unwrap_or(&name);
    let known = (plain && !dynamic && literal_value(&value)).then(|| value.clone());
    record(vars, base, known);
    Ok((name, value))
}

/// What a simple command does to the known variables: a builtin that can
/// assign any variable (`read N`, `eval`, `printf -v`) or a command whose
/// name is only known at run time forgets them all; `cd` changes `$PWD`
/// (never remembered); a `NAME=value` prefix on a POSIX special builtin
/// stays set in POSIX mode, so that name is unknown.
fn after_command(seg: &Segment, vars: &mut Vars) {
    // `time`, `command`, `builtin` and `exec` run the next word in this shell.
    let mut i = 0;
    while i < seg.argv.len()
        && !seg.is_dynamic(i)
        && matches!(
            seg.argv[i].as_str(),
            "time" | "command" | "builtin" | "exec" | "-p" | "--"
        )
    {
        i += 1;
    }
    if let Some(word) = seg.argv.get(i) {
        let printf_v = word == "printf"
            && seg
                .argv
                .iter()
                .enumerate()
                .skip(i + 1)
                .any(|(k, a)| seg.is_dynamic(k) || a.starts_with("-v"));
        if seg.is_dynamic(i) || ASSIGNING.contains(&word.as_str()) || printf_v {
            vars.clear();
        }
    }
    if seg
        .argv
        .first()
        .is_some_and(|w| SPECIAL_BUILTINS.contains(&w.as_str()))
    {
        for (name, _) in &seg.assigns {
            vars.set(name.split('[').next().unwrap_or(name), None);
        }
    }
}

/// Words that tree-sitter files under a redirect (`cat > o x` puts `x` inside
/// the `file_redirect`): `(value, dynamic)`, in order. They are command args.
type TrailingWords = Vec<(String, bool)>;

/// One redirect child: `file_redirect`, or the bails for here-docs/strings.
fn redirect(node: Node, src: &str, vars: &Vars) -> Result<(Redirect, TrailingWords), Bail> {
    if node.kind() != "file_redirect" {
        return Err(bail_for(node));
    }
    let mut fd = None;
    let mut op = None;
    let mut target = None;
    let mut trailing = Vec::new();
    let mut prev_end = 0;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            op = Some(match child.kind() {
                ">" => (RedirOp::OutTrunc, None),
                ">>" => (RedirOp::OutAppend, None),
                "<" => (RedirOp::In, None),
                ">&" => (RedirOp::OutDup, None),
                "<&" => (RedirOp::InDup, None),
                "&>" => (RedirOp::OutAll, None),
                "&>>" => (RedirOp::OutAllAppend, None),
                ">|" => (RedirOp::OutClobber, None),
                ">&-" => (RedirOp::OutDup, Some("-")),
                "<&-" => (RedirOp::InDup, Some("-")),
                _ => return Err(unsupported(child)),
            });
        } else if child.kind() == "file_descriptor" {
            fd = Some(text(child, src)?.parse::<u32>().map_err(|_| Bail::Syntax)?);
        } else if is_word_kind(child.kind()) {
            let fixed_target = op.is_some_and(|(_, fixed)| fixed.is_some());
            if target.is_none() && !fixed_target {
                let (value, dynamic) = word_value(child, src, vars)?;
                // A dynamic target keeps its source text; the engine rejects it.
                // A literal one must not look like an expansion (`'$TMPDIR'/x`),
                // or it would be indistinguishable from the dynamic spelling.
                if !dynamic && value.contains(['$', '`']) {
                    return Err(Bail::Unsupported("redirect_target"));
                }
                target = Some(value);
            } else {
                // A word glued to the previous one by `\<newline>` would need
                // joining; anything else is a separate argument.
                let gap = src.get(prev_end..child.start_byte()).ok_or(Bail::Syntax)?;
                if continuation_only(gap) {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                trailing.push(word_value(child, src, vars)?);
            }
        } else {
            return Err(bail_for(child));
        }
        prev_end = child.end_byte();
    }
    let (op, fixed) = op.ok_or(Bail::Syntax)?;
    let target = match (target, fixed) {
        (Some(t), None) => t,
        (None, Some(t)) => t.to_string(),
        _ => return Err(Bail::Syntax),
    };
    Ok((Redirect { fd, op, target }, trailing))
}

fn empty_segment() -> Segment {
    Segment {
        assigns: Vec::new(),
        argv: Vec::new(),
        dynamic: Vec::new(),
        redirects: Vec::new(),
        follows: Separator::End,
    }
}

/// `(name, value, dynamic)` of one assignment.
fn assignment(node: Node, src: &str, vars: &Vars) -> Result<(String, String, bool), Bail> {
    let name = match node.child_by_field_name("name") {
        Some(n) => text(n, src)?.to_string(),
        None => return Err(Bail::Syntax),
    };
    let (value, dynamic) = match node.child_by_field_name("value") {
        None => (String::new(), false),
        Some(v) if v.kind() == "array" => return Err(Bail::ArrayLiteral),
        // A dynamic value keeps its source text, so `$` stays visible.
        Some(v) => word_value(v, src, vars)?,
    };
    Ok((name, value, dynamic))
}

fn command(node: Node, src: &str, vars: &Vars) -> Result<Segment, Bail> {
    let mut seg = empty_segment();
    let mut prev: Option<(usize, bool)> = None; // (end byte, was a word)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            return Err(unsupported(child));
        }
        let joins = match prev {
            Some((end, _)) => {
                continuation_only(src.get(end..child.start_byte()).ok_or(Bail::Syntax)?)
            }
            None => false,
        };
        match child.kind() {
            "variable_assignment" => {
                if joins {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                let (name, value, _) = assignment(child, src, vars)?;
                seg.assigns.push((name, value));
                prev = Some((child.end_byte(), false));
            }
            "command_name" => {
                if joins {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                let inner = child.named_child(0).ok_or(Bail::Syntax)?;
                if reserved_at_command_position(&seg, inner, src)? {
                    return Err(Bail::Unsupported("reserved_word"));
                }
                let (v, d) = word_value(inner, src, vars)?;
                seg.argv.push(v);
                seg.dynamic.push(d);
                prev = Some((child.end_byte(), true));
            }
            kind if is_word_kind(kind) => {
                if reserved_at_command_position(&seg, child, src)? {
                    return Err(Bail::Unsupported("reserved_word"));
                }
                let (v, d) = word_value(child, src, vars)?;
                match (joins, prev) {
                    // `a\<newline>b` is the single word `ab`.
                    (true, Some((_, true))) => {
                        if let (Some(last), Some(dynamic)) =
                            (seg.argv.last_mut(), seg.dynamic.last_mut())
                        {
                            // Brace expansion could now span the join
                            // (`{-delete\<nl>,}`), so refuse braces.
                            let braces = |t: &str| t.contains(['{', '}']);
                            if braces(last) || braces(&v) {
                                return Err(Bail::Unsupported("line_continuation"));
                            }
                            last.push_str(&v);
                            *dynamic |= d;
                        }
                    }
                    (true, _) => return Err(Bail::Unsupported("line_continuation")),
                    _ => {
                        seg.argv.push(v);
                        seg.dynamic.push(d);
                    }
                }
                prev = Some((child.end_byte(), true));
            }
            "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                if joins {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                let (r, words) = redirect(child, src, vars)?;
                seg.redirects.push(r);
                let trailing = !words.is_empty();
                for (v, d) in words {
                    seg.argv.push(v);
                    seg.dynamic.push(d);
                }
                prev = Some((child.end_byte(), trailing));
            }
            _ => return Err(bail_for(child)),
        }
    }
    Ok(seg)
}
