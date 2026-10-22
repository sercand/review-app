use super::parse;
use crate::model::{Bail, Separator};

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
    assert!(matches!(
        parse("if x; then y; fi"),
        Err(Bail::Unsupported("if_statement"))
    ));
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
    assert_eq!(
        parse("for i in a; do b; done"),
        Err(Bail::Unsupported("for_statement"))
    );
    assert_eq!(
        parse("echo hi > f"),
        Err(Bail::Unsupported("redirected_statement"))
    );
}
