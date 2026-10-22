use super::parse;
use crate::model::{Bail, RedirOp, Separator};

fn argv(src: &str) -> Vec<Vec<String>> {
    parse(src)
        .unwrap_or_else(|b| panic!("{src:?} bailed: {b:?}"))
        .into_iter()
        .map(|s| s.argv)
        .collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn simple_command() {
    let segs = parse("ls -la src").unwrap();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].argv, strs(&["ls", "-la", "src"]));
    assert_eq!(segs[0].dynamic, vec![false, false, false]);
}

#[test]
fn quotes_are_removed() {
    assert_eq!(
        argv("grep \"a | b\" 'c; d' e\\ f"),
        vec![strs(&["grep", "a | b", "c; d", "e f"])]
    );
}

#[test]
fn dynamic_words() {
    let segs = parse("echo $HOME \"$X\" \"a$Y\" {a,b} *.rs").unwrap();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].dynamic, vec![false, true, true, true, true, false]);
    // Numeric brace range, and braces next to a quoted piece.
    let segs = parse("echo x{1..3}y {a,\"b\"} {} a{b} \\{a,b\\}").unwrap();
    assert_eq!(
        segs[0].dynamic,
        vec![false, true, true, false, false, false]
    );
    assert_eq!(segs[0].argv[5], "{a,b}");
}

#[test]
fn dynamic_command_name_and_expansions() {
    let segs = parse("$CMD x ${A:-b}").unwrap();
    assert_eq!(segs[0].dynamic, vec![true, false, true]);
    assert_eq!(segs[0].argv[2], "${A:-b}");
}

#[test]
fn ansi_c_string_is_literal() {
    // Decoded, so `$'\x2d\x72'` is seen as the flag `-r`, not as opaque text.
    let segs = parse("echo $'a\\nb' $'\\x2d\\x72' $'it\\'s'").unwrap();
    assert_eq!(segs[0].argv, strs(&["echo", "a\nb", "-r", "it's"]));
    assert_eq!(segs[0].dynamic, vec![false; 4]);
    // A decoded NUL would truncate the word in bash: not literal.
    let segs = parse("echo $'a\\x00b'").unwrap();
    assert!(segs[0].dynamic[1]);
}

#[test]
fn assignments_collected() {
    let segs = parse("FOO=1 BAR=\"x y\" cmd a").unwrap();
    assert_eq!(
        segs[0].assigns,
        vec![("FOO".into(), "1".into()), ("BAR".into(), "x y".into())]
    );
    assert_eq!(segs[0].argv, strs(&["cmd", "a"]));
    let segs = parse("FOO=bar").unwrap();
    assert!(segs[0].argv.is_empty());
    assert_eq!(segs[0].assigns, vec![("FOO".into(), "bar".into())]);
    let segs = parse("FOO= ls").unwrap();
    assert_eq!(segs[0].assigns, vec![("FOO".into(), String::new())]);
    assert_eq!(segs[0].argv, strs(&["ls"]));
}

#[test]
fn escaped_separators_do_not_split() {
    assert_eq!(argv("echo a\\; ls"), vec![strs(&["echo", "a;", "ls"])]);
}

#[test]
fn unquoted_and_double_quote_escapes() {
    assert_eq!(
        argv("echo \"a\\\"b\\\\ \\$x \\n\" 'it'\"'\"'s' \"x\"y"),
        vec![strs(&["echo", "a\"b\\ $x \\n", "it's", "xy"])]
    );
}

#[test]
fn newlines_split_like_semicolons() {
    let segs = parse("ls\nrm -rf x; echo hi # note\r\npwd").unwrap();
    let got: Vec<_> = segs.iter().map(|s| s.argv.join(" ")).collect();
    assert_eq!(got, vec!["ls", "rm -rf x", "echo hi", "pwd"]);
    assert_eq!(segs[0].follows, Separator::Semi);
    assert_eq!(segs[3].follows, Separator::End);
}

#[test]
fn line_continuation_joins_words() {
    assert_eq!(argv("echo a \\\n b"), vec![strs(&["echo", "a", "b"])]);
    assert_eq!(argv("ls\\\nx -l"), vec![strs(&["lsx", "-l"])]);
    assert_eq!(argv("echo \"c\\\nd\""), vec![strs(&["echo", "cd"])]);
    assert_eq!(argv("echo 'c\\\nd'"), vec![strs(&["echo", "c\\\nd"])]);
}

#[test]
fn substitutions_and_arithmetic_bail() {
    assert_eq!(parse("echo $(ls)"), Err(Bail::CommandSubstitution));
    assert_eq!(parse("echo `ls`"), Err(Bail::CommandSubstitution));
    assert_eq!(parse("echo \"a $(ls)\""), Err(Bail::CommandSubstitution));
    assert_eq!(parse("FOO=$(x) ls"), Err(Bail::CommandSubstitution));
    assert_eq!(parse("echo <(ls)"), Err(Bail::ProcessSubstitution));
    assert_eq!(parse("echo $((1+2))"), Err(Bail::Arithmetic));
    assert_eq!(parse("A=(1 2)"), Err(Bail::ArrayLiteral));
}

#[test]
fn syntax_errors_and_unsupported_kinds_bail() {
    assert_eq!(parse("echo \"a"), Err(Bail::Syntax));
    assert_eq!(parse("echo $("), Err(Bail::Syntax));
    assert_eq!(parse("ls;;"), Err(Bail::Syntax));
    assert_eq!(parse("echo a\0b"), Err(Bail::Syntax));
    assert_eq!(parse("ls &"), Err(Bail::Background));
    assert_eq!(parse("if x; then y; fi"), Err(Bail::CompoundCommand));
}

#[test]
fn empty_and_comment_only_have_no_segments() {
    assert_eq!(parse("").unwrap(), vec![]);
    assert_eq!(parse("# hi").unwrap(), vec![]);
}

#[test]
fn non_ascii_words() {
    assert_eq!(
        argv("echo héllo 日本 \"ü\""),
        vec![strs(&["echo", "héllo", "日本", "ü"])]
    );
}

#[test]
fn only_assignments_make_one_segment() {
    let segs = parse("A=1 B=2").unwrap();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].assigns.len(), 2);
    assert!(segs[0].argv.is_empty());
}

#[test]
fn substitutions_nested_in_expansions_bail() {
    for src in [
        "echo ${A:-$(touch pwned)}",
        "echo \"${A:-$(touch pwned)}\"",
        "echo ${A[$(touch pwned)]}",
        "echo ${A/x/$(ls)}",
        "echo ${A:-`touch pwned`}",
        "echo \"${A:-`touch pwned`}\"",
    ] {
        assert_eq!(parse(src), Err(Bail::CommandSubstitution), "{src}");
    }
    assert!(parse("echo ${A:-$((1+2))}").is_err());
    // Plain expansions stay fine.
    assert!(parse("echo ${A:-b} ${#A} \"${A}\"").is_ok());
}

#[test]
fn number_with_substitution_child_bails() {
    assert_eq!(
        parse("echo 10#$(touch pwned)"),
        Err(Bail::CommandSubstitution)
    );
}

#[test]
fn continuation_cannot_assemble_brace_expansion() {
    for src in ["find . {-delete\\\n,}", "ls {a\\\n,b}"] {
        assert_eq!(
            parse(src),
            Err(Bail::Unsupported("line_continuation")),
            "{src}"
        );
    }
}

#[test]
fn unsupported_kind_names_come_from_the_grammar() {
    assert_eq!(parse("[[ -f x ]]"), Err(Bail::Unsupported("test_command")));
    assert_eq!(
        parse("declare -a x"),
        Err(Bail::Unsupported("declaration_command"))
    );
}

#[test]
fn process_substitution_and_legacy_arithmetic_in_expansions_bail() {
    assert_eq!(
        parse("echo ${A:-<(touch pwned)}"),
        Err(Bail::ProcessSubstitution)
    );
    assert_eq!(
        parse("echo ${A:->(touch pwned)}"),
        Err(Bail::ProcessSubstitution)
    );
    assert_eq!(
        parse("echo \"${A:-<(touch pwned)}\""),
        Err(Bail::ProcessSubstitution)
    );
    assert_eq!(parse("echo ${A:-$[1+2]}"), Err(Bail::Arithmetic));
}

fn follows(src: &str) -> Vec<Separator> {
    parse(src).unwrap().iter().map(|s| s.follows).collect()
}

fn redirs(src: &str) -> Vec<(Option<u32>, RedirOp, String)> {
    let segs = parse(src).unwrap_or_else(|b| panic!("{src:?} bailed: {b:?}"));
    segs.into_iter()
        .flat_map(|s| s.redirects)
        .map(|r| (r.fd, r.op, r.target))
        .collect()
}

#[test]
fn list_and_pipeline_segments() {
    assert_eq!(
        follows("a && b | c; d"),
        vec![
            Separator::And,
            Separator::Pipe,
            Separator::Semi,
            Separator::End
        ]
    );
    assert_eq!(
        follows("a || b |& c"),
        vec![Separator::Or, Separator::PipeBoth, Separator::End]
    );
    assert_eq!(
        follows("a &&\nb |\nc"),
        vec![Separator::And, Separator::Pipe, Separator::End]
    );
    assert_eq!(
        argv("! ls | a && ! b"),
        vec![strs(&["ls"]), strs(&["a"]), strs(&["b"])]
    );
}

#[test]
fn newline_splits_and_trailing_separators_ok() {
    assert_eq!(parse("ls\nrm x\n").unwrap().len(), 2);
    assert_eq!(parse("ls;").unwrap().len(), 1);
    assert_eq!(parse("").unwrap(), vec![]);
    assert_eq!(parse("  ").unwrap(), vec![]);
}

#[test]
fn crlf_and_line_continuation() {
    assert_eq!(argv("ls\r\nrm x"), vec![strs(&["ls"]), strs(&["rm", "x"])]);
    assert_eq!(argv("ls \\\n-la"), vec![strs(&["ls", "-la"])]);
}

#[test]
fn redirect_forms() {
    use RedirOp::*;
    let r = |fd, op, t: &str| (fd, op, t.to_string());
    assert_eq!(redirs("cat x > o"), vec![r(None, OutTrunc, "o")]);
    assert_eq!(redirs("cat x 2>&1"), vec![r(Some(2), OutDup, "1")]);
    assert_eq!(redirs("cat &> f"), vec![r(None, OutAll, "f")]);
    assert_eq!(redirs("cat &>> f"), vec![r(None, OutAllAppend, "f")]);
    assert_eq!(redirs("cat >> f"), vec![r(None, OutAppend, "f")]);
    assert_eq!(redirs("cat < f"), vec![r(None, In, "f")]);
    assert_eq!(redirs("cat >| f"), vec![r(None, OutClobber, "f")]);
    assert_eq!(redirs("cat <&0"), vec![r(None, InDup, "0")]);
    assert_eq!(redirs("cat >&-"), vec![r(None, OutDup, "-")]);
    assert_eq!(
        redirs("cat 2> /dev/null"),
        vec![r(Some(2), OutTrunc, "/dev/null")]
    );
    assert_eq!(
        redirs("cat x > 2> /dev/null"),
        vec![r(None, OutTrunc, "2"), r(None, OutTrunc, "/dev/null")]
    );
    assert_eq!(redirs("> f cmd"), vec![r(None, OutTrunc, "f")]);
    assert_eq!(redirs("cat x > o | b"), vec![r(None, OutTrunc, "o")]);
    // A dynamic target keeps its source text.
    assert_eq!(redirs("echo > $F"), vec![r(None, OutTrunc, "$F")]);
    // Redirects attach to their own command, words after a redirect still count.
    let segs = parse("cat > o x $Y").unwrap();
    assert_eq!(segs[0].argv, strs(&["cat", "x", "$Y"]));
    assert_eq!(segs[0].dynamic, vec![false, false, true]);
    assert_eq!(segs[0].redirects[0].target, "o");
    assert_eq!(
        argv("cat a > o x > p y"),
        vec![strs(&["cat", "a", "x", "y"])]
    );
    let segs = parse("> f").unwrap();
    assert!(segs[0].argv.is_empty());
    assert_eq!(segs[0].redirects.len(), 1);
}

#[test]
fn redirect_target_bails_like_any_word() {
    assert_eq!(parse("echo > $(x)"), Err(Bail::CommandSubstitution));
    assert_eq!(parse("echo > <(x)"), Err(Bail::ProcessSubstitution));
}

#[test]
fn continuation_rules_survive_redirects() {
    assert_eq!(argv("echo a\\\nb > f"), vec![strs(&["echo", "ab"])]);
    assert_eq!(
        parse("echo {a\\\n,b} > f"),
        Err(Bail::Unsupported("line_continuation"))
    );
    assert_eq!(
        parse("echo > f\\\ng"),
        Err(Bail::Unsupported("line_continuation"))
    );
}

#[test]
fn bail_table() {
    let cases: &[(&str, Bail)] = &[
        ("$(x)", Bail::CommandSubstitution),
        ("`x`", Bail::CommandSubstitution),
        ("\"$(x)\"", Bail::CommandSubstitution),
        ("<(x)", Bail::ProcessSubstitution),
        ("cat <<EOF\nx\nEOF", Bail::Heredoc),
        ("cat <<-EOF\nx\nEOF", Bail::Heredoc),
        ("cat <<<x", Bail::HereString),
        ("echo $((1+1))", Bail::Arithmetic),
        ("(a)", Bail::Subshell),
        ("a && (b)", Bail::Subshell),
        ("{ a; }", Bail::BraceGroup),
        ("if a; then b; fi", Bail::CompoundCommand),
        ("for i in a; do b; done", Bail::CompoundCommand),
        ("while a; do b; done", Bail::CompoundCommand),
        ("until a; do b; done", Bail::CompoundCommand),
        ("case a in b) c;; esac", Bail::CompoundCommand),
        ("f() { a; }", Bail::FunctionDef),
        ("A=(1 2)", Bail::ArrayLiteral),
        ("a &", Bail::Background),
        ("a & b", Bail::Background),
        ("a && b &", Bail::Background),
        ("a | b &", Bail::Background),
        ("echo 'x", Bail::Syntax),
        ("ls;;", Bail::Syntax),
        ("[[ -f x ]]", Bail::Unsupported("test_command")),
    ];
    for (src, want) in cases {
        assert_eq!(parse(src).as_ref(), Err(want), "{src:?}");
    }
}

#[test]
fn depth_is_capped() {
    // A chain of N `&&` nests N lists; commands sit at depth N, the cap is 64.
    assert!(parse(&format!("{}a", "a && ".repeat(63))).is_ok());
    assert!(parse(&format!("{}a", "a && ".repeat(64))).is_ok());
    assert_eq!(
        parse(&format!("{}a", "a && ".repeat(65))),
        Err(Bail::Unsupported("depth"))
    );
}

#[test]
fn redirect_on_last_command_of_list_or_pipeline() {
    let segs = parse("a && b > f").unwrap();
    assert_eq!(segs.len(), 2);
    assert!(segs[0].redirects.is_empty());
    assert_eq!(segs[1].redirects.len(), 1);
    assert_eq!(segs[1].redirects[0].target, "f");
    assert_eq!(segs[0].follows, Separator::And);

    let segs = parse("a | b 2>/dev/null").unwrap();
    assert!(segs[0].redirects.is_empty());
    assert_eq!(segs[1].redirects.len(), 1);
    assert_eq!(segs[1].redirects[0].fd, Some(2));
    assert_eq!(segs[0].follows, Separator::Pipe);

    // Trailing words after the target are args of the last command.
    let segs = parse("a | b > f x").unwrap();
    assert_eq!(segs[0].argv, strs(&["a"]));
    assert_eq!(segs[1].argv, strs(&["b", "x"]));

    for src in [
        "git log | head -5 2>/dev/null",
        "a | b 2>&1 | c",
        "a && b | c > f",
        "! a | b > f",
    ] {
        assert!(parse(src).is_ok(), "{src}");
    }
    let segs = parse("a | b 2>&1 | c").unwrap();
    assert!(segs[0].redirects.is_empty() && segs[2].redirects.is_empty());
    assert_eq!(segs[1].redirects.len(), 1);
    let segs = parse("a && b | c > f").unwrap();
    assert_eq!(
        segs.iter().map(|s| s.redirects.len()).collect::<Vec<_>>(),
        vec![0, 0, 1]
    );
}

#[test]
fn literal_redirect_target_with_dollar_bails() {
    assert_eq!(
        parse("echo > '$TMPDIR'/x"),
        Err(Bail::Unsupported("redirect_target"))
    );
    assert_eq!(
        parse("echo > \"a\\$b\""),
        Err(Bail::Unsupported("redirect_target"))
    );
    let segs = parse("echo > $TMPDIR/x").unwrap();
    assert_eq!(segs[0].redirects[0].target, "$TMPDIR/x");
}

#[test]
fn fixed_target_redirect_takes_following_words_as_args() {
    let segs = parse("cat >&- x").unwrap();
    assert_eq!(segs[0].argv, strs(&["cat", "x"]));
    assert_eq!(segs[0].redirects[0].op, RedirOp::OutDup);
    assert_eq!(segs[0].redirects[0].target, "-");
    let segs = parse("cat <&- x").unwrap();
    assert_eq!(segs[0].argv, strs(&["cat", "x"]));
    // tree-sitter reports a syntax error for two words after either form.
    assert_eq!(parse("cat >&- x y"), Err(Bail::Syntax));
    assert_eq!(parse("cat <&- x y"), Err(Bail::Syntax));
}

/// tree-sitter reads `{`, `if`, ... after `!` or `time` as plain words; a
/// reserved word in command position must never become a simple command.
#[test]
fn reserved_word_in_command_position_bails() {
    for src in [
        "! { echo hello; }",
        "! if true; then false; fi",
        "! while false; do echo x; done",
        "! for i in 1; do false; done",
        "time { echo hello; }",
        "time -p { echo hello; }",
        "coproc { cmd |& cat; }",
        "ls; ! { rm x; }",
    ] {
        assert!(parse(src).is_err(), "{src:?} must bail");
    }
    // Quoted, escaped, or in argument position they are ordinary words.
    for src in ["echo { } if fi", "'if' x", "i\\f x", "time ls", "! true"] {
        assert!(parse(src).is_ok(), "{src:?} must parse");
    }
}

// I4: `${x@P}` / `${x@E}` transformation operators change the value of a word.
#[test]
fn parameter_transformation_operators_bail() {
    for src in [
        "echo ${x@P}",
        "echo ${x@E}",
        "echo \"${x@A}\"",
        "echo a${x@Q}b",
        "A=${x@P} ls",
        "ls ${x@a}",
    ] {
        assert_eq!(
            parse(src),
            Err(Bail::Unsupported("parameter_transformation")),
            "{src:?}"
        );
    }
    // Ordinary expansions are untouched.
    for src in [
        "echo ${x}",
        "echo ${x:-a}",
        "echo \"${x}\" a@b",
        "echo ${#x}",
    ] {
        assert!(parse(src).is_ok(), "{src:?}");
    }
}
