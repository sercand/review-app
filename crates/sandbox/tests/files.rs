//! File ops, confinement, glob and grep of [`LocalHost`] against real temp directories.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use review_sandbox::{LocalHost, MAX_FILE_BYTES, MAX_GLOB, MAX_GREP, OpError, Root, SandboxOps};

fn host(dir: &Path) -> LocalHost {
    LocalHost::new(dir).expect("host")
}

fn set_mtime(path: &Path, secs_ago: u64) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .unwrap();
}

#[tokio::test]
async fn read_write_list_exists_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());

    assert_eq!(h.exists("a/b/c.txt").await.unwrap(), (false, false));
    assert!(
        h.write("a/b/c.txt", b"hello").await.unwrap(),
        "first write creates"
    );
    assert!(
        !h.write("a/b/c.txt", b"hello again").await.unwrap(),
        "second write overwrites"
    );
    assert_eq!(h.read("a/b/c.txt").await.unwrap(), b"hello again");
    assert_eq!(h.read("./a/b/../b/c.txt").await.unwrap(), b"hello again");
    assert_eq!(h.exists("a/b/c.txt").await.unwrap(), (true, false));
    assert_eq!(h.exists("a/b").await.unwrap(), (true, true));
    assert_eq!(h.exists("").await.unwrap(), (true, true));

    h.write("a/z.txt", b"zz").await.unwrap();
    let entries = h.list("a").await.unwrap();
    let names: Vec<_> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.is_dir))
        .collect();
    assert_eq!(names, vec![("b", true), ("z.txt", false)]);
    let z = entries.iter().find(|e| e.name == "z.txt").unwrap();
    assert_eq!(z.size, 2);
    assert!(z.modified.is_some());

    // The root itself lists, with "" and ".".
    assert_eq!(h.list("").await.unwrap().len(), 1);
    assert_eq!(h.list(".").await.unwrap().len(), 1);

    // Absolute paths inside the root work, through the non-canonical form too
    // (macOS: /var → /private/var).
    let abs = tmp.path().join("a/z.txt");
    assert_eq!(h.read(abs.to_str().unwrap()).await.unwrap(), b"zz");

    assert!(matches!(
        h.read("missing.txt").await,
        Err(OpError::NotFound(_))
    ));
    assert!(matches!(h.list("missing").await, Err(OpError::NotFound(_))));
    assert!(matches!(h.read("a").await, Err(OpError::Invalid(_))));
    assert!(matches!(h.list("a/z.txt").await, Err(OpError::Invalid(_))));
    assert!(matches!(h.write("a", b"x").await, Err(OpError::Invalid(_))));
}

#[cfg(unix)]
#[tokio::test]
async fn confinement_matrix() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "needle secret").unwrap();
    // A sibling sharing the root's name as a string prefix: whole segments only.
    fs::create_dir_all(tmp.path().join("rootx")).unwrap();
    fs::write(tmp.path().join("rootx/secret.txt"), "needle secret").unwrap();
    symlink("../rootx", root.join("prefix_link")).unwrap();
    fs::write(root.join("src/nested/ok.txt"), "needle ok").unwrap();
    // Symlinks inside the root pointing outside: to a file, to a dir, dangling.
    symlink(outside.join("secret.txt"), root.join("link_file")).unwrap();
    symlink(&outside, root.join("link_dir")).unwrap();
    symlink(outside.join("nope.txt"), root.join("dangling")).unwrap();
    symlink("../../outside/secret.txt", root.join("src/rel_link")).unwrap();
    // In-root symlinks, which keep working.
    symlink("src/nested", root.join("inside_dir")).unwrap();
    symlink("nested/ok.txt", root.join("src/inside_file")).unwrap();

    let h = host(&root);
    let secret = outside.join("secret.txt");
    let escapes = [
        "../outside/secret.txt",
        "src/../../outside/secret.txt",
        "/etc/passwd",
        secret.to_str().unwrap(),
        "link_file",
        "link_dir",
        "link_dir/secret.txt",
        "dangling",
        "src/rel_link",
        "inside_dir/../../../outside/secret.txt",
        "../rootx/secret.txt",
        "prefix_link/secret.txt",
    ];
    for p in escapes {
        assert!(
            matches!(h.read(p).await, Err(OpError::EscapesRoot(_))),
            "read {p}: {:?}",
            h.read(p).await
        );
        assert!(
            matches!(h.exists(p).await, Err(OpError::EscapesRoot(_))),
            "exists {p}"
        );
        assert!(
            matches!(h.write(p, b"x").await, Err(OpError::EscapesRoot(_))),
            "write {p}"
        );
        assert!(
            matches!(h.list(p).await, Err(OpError::EscapesRoot(_))),
            "list {p}"
        );
        assert!(
            matches!(h.glob("*", p).await, Err(OpError::EscapesRoot(_))),
            "glob {p}"
        );
        assert!(
            matches!(
                h.grep("needle", p, None).await,
                Err(OpError::EscapesRoot(_))
            ),
            "grep {p}"
        );
    }
    // Writes to new files under an escaping symlinked dir, and new dirs below it.
    for p in [
        "link_dir/new.txt",
        "link_dir/deep/new.txt",
        "../new.txt",
        "/tmp/oqa-new.txt",
    ] {
        assert!(
            matches!(h.write(p, b"x").await, Err(OpError::EscapesRoot(_))),
            "write {p}"
        );
    }
    assert!(!outside.join("new.txt").exists());
    assert!(!outside.join("deep").exists());
    // The dangling link was not created through.
    assert!(!outside.join("nope.txt").exists());
    // `..` after a missing component is refused, not resolved lexically.
    assert!(h.write("missing/../x.txt", b"x").await.is_err());
    assert!(!root.join("x.txt").exists());
    assert_eq!(fs::read_to_string(&secret).unwrap(), "needle secret");

    // Legit paths, nested and through in-root symlinks.
    assert_eq!(h.read("src/nested/ok.txt").await.unwrap(), b"needle ok");
    assert_eq!(h.read("inside_dir/ok.txt").await.unwrap(), b"needle ok");
    assert_eq!(h.read("src/inside_file").await.unwrap(), b"needle ok");
    assert_eq!(h.exists("inside_dir").await.unwrap(), (true, true));
    assert!(h.write("inside_dir/new.txt", b"n").await.unwrap());
    assert!(root.join("src/nested/new.txt").exists());
    assert_eq!(h.list("inside_dir").await.unwrap().len(), 2);

    // Walks never follow the escaping links.
    let all = h.glob("**/*", "").await.unwrap();
    assert!(
        all.items.iter().all(|p| !p.contains("secret")
            && p != "link_file"
            && p != "dangling"
            && p != "src/rel_link"),
        "{:?}",
        all.items
    );
    assert!(all.items.contains(&"src/nested/ok.txt".to_string()));
    let hits = h.grep("needle", "", None).await.unwrap();
    assert!(
        hits.items.iter().all(|m| !m.text.contains("secret")),
        "{:?}",
        hits.items
    );
    assert!(hits.items.iter().any(|m| m.path == "src/nested/ok.txt"));
}

#[test]
fn root_resolution_is_canonical() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Root::new(tmp.path()).unwrap();
    let canon = fs::canonicalize(tmp.path()).unwrap();
    assert_eq!(root.path(), canon.as_path());
    assert_eq!(root.resolve("").unwrap(), canon);
    assert_eq!(root.resolve_new("a/b").unwrap(), canon.join("a/b"));
    assert!(matches!(root.resolve("a"), Err(OpError::NotFound(_))));
    assert!(matches!(root.resolve(".."), Err(OpError::EscapesRoot(_))));
    assert_eq!(root.relative(&canon.join("a").join("b.rs")), "a/b.rs");
}

#[tokio::test]
async fn glob_honours_gitignore_and_caps() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let r = tmp.path();
    fs::write(r.join(".gitignore"), "target/\n*.gen.rs\n").unwrap();
    fs::create_dir_all(r.join("src/deep")).unwrap();
    fs::create_dir_all(r.join("target")).unwrap();
    fs::write(r.join("main.rs"), "").unwrap();
    fs::write(r.join("src/lib.rs"), "").unwrap();
    fs::write(r.join("src/deep/mod.rs"), "").unwrap();
    fs::write(r.join("src/notes.md"), "").unwrap();
    fs::write(r.join("src/x.gen.rs"), "").unwrap();
    fs::write(r.join("target/out.rs"), "").unwrap();
    set_mtime(&r.join("main.rs"), 300);
    set_mtime(&r.join("src/lib.rs"), 200);
    set_mtime(&r.join("src/deep/mod.rs"), 100);

    let got = h.glob("**/*.rs", "").await.unwrap();
    assert_eq!(got.items, vec!["src/deep/mod.rs", "src/lib.rs", "main.rs"]);
    assert!(!got.truncated);

    // `*` does not cross `/`.
    let top = h.glob("*.rs", "").await.unwrap();
    assert_eq!(top.items, vec!["main.rs"]);

    // The pattern is relative to `directory`; results stay root-relative.
    let sub = h.glob("*.rs", "src").await.unwrap();
    assert_eq!(sub.items, vec!["src/lib.rs"]);

    assert!(matches!(h.glob("[", "").await, Err(OpError::Invalid(_))));
    assert!(matches!(
        h.glob("*", "missing").await,
        Err(OpError::NotFound(_))
    ));

    let many = tempfile::tempdir().unwrap();
    for i in 0..=MAX_GLOB {
        let p = many.path().join(format!("f{i:03}.txt"));
        fs::write(&p, "").unwrap();
        set_mtime(&p, 10_000 - i as u64);
    }
    let h2 = host(many.path());
    let got = h2.glob("*.txt", "").await.unwrap();
    assert_eq!(got.items.len(), MAX_GLOB);
    assert!(got.truncated);
    assert_eq!(got.items[0], format!("f{MAX_GLOB:03}.txt"), "newest first");
    assert!(
        !got.items.contains(&"f000.txt".to_string()),
        "oldest dropped"
    );
}

#[tokio::test]
async fn grep_matches_and_skips_binary_and_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let r = tmp.path();
    fs::write(r.join(".gitignore"), "ignored/\n").unwrap();
    fs::create_dir_all(r.join("src")).unwrap();
    fs::create_dir_all(r.join("ignored")).unwrap();
    fs::write(r.join("src/a.rs"), "fn main() {}\nlet needle = 1;\n").unwrap();
    fs::write(r.join("src/b.md"), "a needle in docs\n").unwrap();
    fs::write(r.join("ignored/c.rs"), "needle\n").unwrap();
    fs::write(r.join("bin.dat"), b"needle\0\x01\x02binary").unwrap();

    let got = h.grep("needle", "", None).await.unwrap();
    let mut hits: Vec<_> = got
        .items
        .iter()
        .map(|m| (m.path.as_str(), m.line, m.text.as_str()))
        .collect();
    hits.sort();
    assert_eq!(
        hits,
        vec![
            ("src/a.rs", 2, "let needle = 1;"),
            ("src/b.md", 1, "a needle in docs"),
        ]
    );
    assert!(!got.truncated);

    let rs = h.grep("needle", "", Some("*.rs")).await.unwrap();
    assert_eq!(rs.items.len(), 1);
    assert_eq!(rs.items[0].path, "src/a.rs");
    let sub = h.grep("ne+dle", "src", Some("**/*.md")).await.unwrap();
    assert_eq!(sub.items.len(), 1);
    assert_eq!(sub.items[0].path, "src/b.md");

    // A single file as the directory searches just that file.
    let one = h.grep("needle", "src/a.rs", None).await.unwrap();
    assert_eq!(one.items.len(), 1);

    assert!(matches!(
        h.grep("(", "", None).await,
        Err(OpError::Invalid(_))
    ));
    assert!(matches!(
        h.grep("x", "", Some("[")).await,
        Err(OpError::Invalid(_))
    ));

    let many = tempfile::tempdir().unwrap();
    let body: String = (0..MAX_GREP + 50).map(|i| format!("hit {i}\n")).collect();
    fs::write(many.path().join("big.txt"), body).unwrap();
    let got = host(many.path()).grep("hit", "", None).await.unwrap();
    assert_eq!(got.items.len(), MAX_GREP);
    assert!(got.truncated);
    assert_eq!(got.items[0].line, 1);
}

#[tokio::test]
async fn too_large_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let big = vec![b'a'; MAX_FILE_BYTES + 1];
    fs::write(tmp.path().join("big.bin"), &big).unwrap();
    let err = h.read("big.bin").await.unwrap_err();
    assert!(matches!(err, OpError::TooLarge(_)), "{err:?}");
    assert_eq!(err.code(), "too_large");
    assert!(
        err.to_string().contains("file too large for one frame"),
        "{err}"
    );
    assert!(err.to_string().contains("use bash"), "{err}");

    let err = h.write("new.bin", &big).await.unwrap_err();
    assert!(matches!(err, OpError::TooLarge(_)), "{err:?}");
    assert!(!tmp.path().join("new.bin").exists());

    // Exactly at the limit is fine.
    fs::write(tmp.path().join("edge.bin"), &big[..MAX_FILE_BYTES]).unwrap();
    assert_eq!(h.read("edge.bin").await.unwrap().len(), MAX_FILE_BYTES);
}

#[test]
fn error_codes() {
    assert_eq!(OpError::NotFound("x".into()).code(), "not_found");
    assert_eq!(OpError::EscapesRoot("x".into()).code(), "escapes_root");
    assert_eq!(OpError::Invalid("x".into()).code(), "invalid");
    assert_eq!(OpError::Io("x".into()).code(), "io");
    assert_eq!(OpError::Timeout.code(), "timeout");
    assert_eq!(OpError::Disconnected.code(), "disconnected");
    assert_eq!(OpError::Cancelled.code(), "cancelled");
    assert_eq!(
        OpError::EscapesRoot("../x".into()).to_string(),
        "path escapes the project root: ../x"
    );
}

#[tokio::test]
async fn only_in_root_ignore_files_apply() {
    let tmp = tempfile::tempdir().unwrap();
    // A parent-of-root `.gitignore` that would hide everything.
    fs::write(tmp.path().join(".gitignore"), "*\n").unwrap();
    fs::write(tmp.path().join(".ignore"), "*\n").unwrap();
    let r = tmp.path().join("proj");
    fs::create_dir_all(r.join("src/deep")).unwrap();
    fs::write(r.join(".gitignore"), "*.gen.rs\n").unwrap();
    fs::write(r.join("src/.gitignore"), "skip/\n").unwrap();
    fs::create_dir_all(r.join("src/deep/skip")).unwrap();
    fs::write(r.join("src/a.rs"), "needle\n").unwrap();
    fs::write(r.join("src/x.gen.rs"), "needle\n").unwrap();
    fs::write(r.join("src/deep/b.rs"), "needle\n").unwrap();
    fs::write(r.join("src/deep/skip/c.rs"), "needle\n").unwrap();
    let h = host(&r);

    let mut all = h.glob("**/*.rs", "").await.unwrap().items;
    all.sort();
    assert_eq!(all, vec!["src/a.rs", "src/deep/b.rs"]);
    // Walks starting below the root still apply the in-root ancestors' rules.
    let mut sub = h.glob("**/*.rs", "src/deep").await.unwrap().items;
    sub.sort();
    assert_eq!(sub, vec!["src/deep/b.rs"]);
    let mut sub = h.glob("*.rs", "src").await.unwrap().items;
    sub.sort();
    assert_eq!(sub, vec!["src/a.rs"]);

    let paths = |l: review_sandbox::Listing<review_sandbox::GrepMatch>| {
        let mut v: Vec<_> = l.items.into_iter().map(|m| m.path).collect();
        v.sort();
        v
    };
    assert_eq!(
        paths(h.grep("needle", "", None).await.unwrap()),
        vec!["src/a.rs", "src/deep/b.rs"]
    );
    assert_eq!(
        paths(h.grep("needle", "src/deep", None).await.unwrap()),
        vec!["src/deep/b.rs"]
    );
}

#[tokio::test]
async fn hidden_files_are_walked_but_vcs_dirs_are_not() {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    for d in [".github", ".git", ".jj", ".hg", ".svn"] {
        fs::create_dir_all(r.join(d)).unwrap();
    }
    fs::write(r.join(".github/x.yml"), "needle\n").unwrap();
    fs::write(r.join(".env.example"), "needle\n").unwrap();
    fs::write(r.join(".gitignore"), "# needle\n").unwrap();
    fs::write(r.join(".git/HEAD"), "needle\n").unwrap();
    fs::write(r.join(".jj/x"), "needle\n").unwrap();
    fs::write(r.join(".hg/x"), "needle\n").unwrap();
    fs::write(r.join(".svn/x"), "needle\n").unwrap();
    let h = host(r);

    let want = vec![".env.example", ".github/x.yml", ".gitignore"];
    let mut got = h.glob("**/*", "").await.unwrap().items;
    got.sort();
    assert_eq!(got, want);
    let mut got: Vec<_> = h
        .grep("needle", "", None)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|m| m.path)
        .collect();
    got.sort();
    assert_eq!(got, want);
}

#[tokio::test]
async fn write_to_a_trailing_slash_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    assert!(matches!(
        h.write("newdir/", b"x").await,
        Err(OpError::Invalid(_))
    ));
    assert!(matches!(
        h.write("a/b/", b"x").await,
        Err(OpError::Invalid(_))
    ));
    assert!(!tmp.path().join("newdir").exists());
    assert!(!tmp.path().join("a").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn read_of_a_fifo_is_refused_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let fifo = tmp.path().join("pipe");
    let ok = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success();
    assert!(ok, "mkfifo");
    let h = host(tmp.path());
    let res = tokio::time::timeout(Duration::from_secs(5), h.read("pipe"))
        .await
        .expect("read of a FIFO must not block");
    assert!(matches!(res, Err(OpError::Invalid(_))), "{res:?}");
    // Walks skip it too.
    let g = tokio::time::timeout(Duration::from_secs(5), h.grep("x", "", None))
        .await
        .expect("grep must not block");
    assert!(g.unwrap().items.is_empty());
    let g = tokio::time::timeout(Duration::from_secs(5), h.grep("x", "pipe", None))
        .await
        .expect("grep must not block");
    assert!(g.unwrap().items.is_empty());
}
