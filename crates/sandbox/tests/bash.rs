//! The persistent bash session behind [`LocalHost::exec`]: state, exit codes, merged
//! stderr, restarts, and a command that floods, hangs or spoofs the sentinel.
#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use review_sandbox::{DEFAULT_EXEC_TIMEOUT, ExecOutput, LocalHost, OUTPUT_CAP, SandboxOps};

const T: Duration = Duration::from_secs(20);

fn host(dir: &Path) -> LocalHost {
    LocalHost::new(dir).expect("host")
}

async fn run(h: &LocalHost, cmd: &str) -> ExecOutput {
    h.exec(cmd, false, T).await.expect(cmd)
}

/// Whether a process with this pid is still alive (`kill -0`).
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn exec_echo_and_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());

    let out = run(&h, "echo hi").await;
    assert_eq!(out.output, "hi\n");
    assert_eq!(out.exit_code, 0);
    assert!(!out.timed_out && !out.truncated);

    assert_eq!(run(&h, "false").await.exit_code, 1);
    let out = run(&h, "printf 'no newline'").await;
    assert_eq!((out.output.as_str(), out.exit_code), ("no newline", 0));

    // `exit` ends the shell; its status comes back and the next command gets a new one.
    let out = run(&h, "exit 3").await;
    assert_eq!(out.exit_code, 3);
    let out = run(&h, "echo again").await;
    assert_eq!((out.output.as_str(), out.exit_code), ("again\n", 0));

    // Multi-line commands, heredocs, `!` and long lines all go through intact.
    let out = run(
        &h,
        "for i in 1 2; do\n  echo n$i\ndone\ncat <<'EOF'\nline!x\nEOF",
    )
    .await;
    assert_eq!(out.output, "n1\nn2\nline!x\n");
    let long = "a".repeat(5000);
    let out = run(&h, &format!("echo {long} | wc -c | tr -d ' '")).await;
    assert_eq!(out.output, "5001\n");

    // A syntax error fails fast instead of hanging until the timeout.
    let started = Instant::now();
    let out = run(&h, "echo )").await;
    assert!(started.elapsed() < Duration::from_secs(5), "{out:?}");
    assert_ne!(out.exit_code, 0);
    assert!(!out.timed_out);
    assert_eq!(run(&h, "echo fine").await.output, "fine\n");

    // An empty command without `restart` is refused, not sent to the shell.
    assert!(h.exec("  ", false, T).await.is_err());
    assert_eq!(DEFAULT_EXEC_TIMEOUT, Duration::from_secs(120));
}

#[tokio::test]
async fn state_persists_between_calls() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    let h = host(tmp.path());

    let out = run(&h, "pwd").await;
    assert_eq!(
        out.output.trim_end(),
        h.root().path().to_str().unwrap(),
        "starts in the root"
    );
    run(&h, "cd sub; export X=1").await;
    let out = run(&h, "pwd; echo $X").await;
    let lines: Vec<&str> = out.output.lines().collect();
    assert!(lines[0].ends_with("/sub"), "{out:?}");
    assert_eq!(lines[1], "1");
}

#[tokio::test]
async fn stderr_is_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let out = run(&h, "echo out; echo err >&2; ls /definitely/not/here").await;
    assert!(out.output.starts_with("out\nerr\n"), "{out:?}");
    assert!(out.output.contains("/definitely/not/here"), "{out:?}");
    assert_ne!(out.exit_code, 0);
    // Commands get /dev/null as stdin, so a reader does not hang.
    let out = run(&h, "cat; echo done").await;
    assert_eq!(out.output, "done\n");
    // ANSI escapes are stripped.
    let out = run(&h, r"printf '\033[31mred\033[0m\n'").await;
    assert_eq!(out.output, "red\n");
}

#[tokio::test]
async fn restart_resets_state() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    run(&h, "export Y=1").await;
    assert_eq!(run(&h, "echo ${Y:-unset}").await.output, "1\n");

    let out = h.exec("", true, T).await.unwrap();
    assert_eq!(
        (out.output.as_str(), out.exit_code),
        ("bash session restarted", 0)
    );
    assert_eq!(run(&h, "echo ${Y:-unset}").await.output, "unset\n");

    run(&h, "export Y=2").await;
    let out = h.exec("echo ${Y:-unset}", true, T).await.unwrap();
    assert_eq!(
        out.output, "unset\n",
        "restart with a command runs it in a new shell"
    );
}

#[tokio::test]
async fn flood_hang_and_spoof() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());

    // Flood: 50 MiB comes back as its last 10 MiB with a marker, quickly. The `cat`
    // matters on macOS only: BSD `head` writes to a terminal a line at a time, which
    // takes ~13 s on its own (GNU `head` does not).
    let started = Instant::now();
    let out = h
        .exec(
            "yes | head -c 52428800 | cat",
            false,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let took = started.elapsed();
    eprintln!("flood took {took:?}, {} bytes", out.output.len());
    assert!(took < Duration::from_secs(10), "flood took {took:?}");
    assert!(out.truncated);
    assert_eq!(out.exit_code, 0);
    let body = out
        .output
        .strip_prefix("[output truncated]\n")
        .expect("marker");
    assert!(body.len() <= OUTPUT_CAP, "{}", body.len());
    assert!(body.len() > OUTPUT_CAP / 2, "{}", body.len());
    assert!(body.lines().all(|l| l == "y"), "only y lines");

    // Hang: a 1 s timeout returns within about 2 s and kills the whole process group.
    let started = Instant::now();
    let out = h
        .exec(
            "sleep 1000 & echo $! > bg.pid; echo $$ > sh.pid; echo started; sleep 1000",
            false,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_millis(2500), "hang took {took:?}");
    assert!(out.timed_out, "{out:?}");
    assert_eq!(out.exit_code, -1);
    // An interactive bash announces the background job (`[1] <pid>`) before it.
    assert!(out.output.ends_with("started\n\n[timed out]"), "{out:?}");
    let bg = std::fs::read_to_string(tmp.path().join("bg.pid")).unwrap();
    let sh = std::fs::read_to_string(tmp.path().join("sh.pid")).unwrap();
    let (bg, sh) = (bg.trim(), sh.trim());
    let deadline = Instant::now() + Duration::from_secs(3);
    while (alive(bg) || alive(sh)) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(bg), "background child {bg} survived");
    assert!(!alive(sh), "shell {sh} survived");

    // The next command gets a fresh shell.
    let out = run(&h, "echo ok").await;
    assert_eq!((out.output.as_str(), out.exit_code), ("ok\n", 0));

    // Printing sentinel-looking text does not end the command early.
    let out = run(
        &h,
        "echo __OQA_CMD_; echo __OQA_CMD_0123456789abcdef_DONE__0; sleep 0.2; echo after; (exit 4)",
    )
    .await;
    assert_eq!(
        out.output,
        "__OQA_CMD_\n__OQA_CMD_0123456789abcdef_DONE__0\nafter\n"
    );
    assert_eq!(out.exit_code, 4);
}

#[tokio::test]
async fn dropping_an_exec_kills_its_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let cancelled = tokio::time::timeout(
        Duration::from_secs(1),
        h.exec("echo $$ > sh.pid; sleep 1000", false, T),
    )
    .await;
    assert!(cancelled.is_err(), "the caller gave up first");
    let sh = std::fs::read_to_string(tmp.path().join("sh.pid")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(sh.trim()) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(sh.trim()), "shell survived a dropped exec");
    assert_eq!(run(&h, "echo ok").await.output, "ok\n");
}

#[tokio::test]
async fn long_commands_back_to_back() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let started = Instant::now();
    for i in 0..20 {
        run(&h, "true").await;
        let long = "b".repeat(6000);
        let out = h
            .exec(
                &format!("echo {long} | wc -c | tr -d ' '"),
                false,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(
            (out.output.as_str(), out.timed_out),
            ("6001\n", false),
            "round {i}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn tabs_control_chars_and_bangs_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let out = run(&h, "cat <<EOF\n\tindented\n\t\ttwice\nEOF").await;
    assert_eq!(out.output, "\tindented\n\t\ttwice\n");
    let out = run(&h, "printf \"a\\tb\\n\"").await;
    assert_eq!(out.output, "a\tb\n");
    let out = run(&h, "printf 'a\tb\\n'").await;
    assert_eq!(out.output, "a\tb\n", "a literal TAB in the command");
    // A literal ^C and `!` in the command reach bash as data.
    let out = run(&h, "x='a\u{3}b!c'; echo ${#x}; echo \"!x !! hi!\"").await;
    assert_eq!(out.output, "5\n!x !! hi!\n");
    assert_eq!(out.exit_code, 0);
}

#[tokio::test]
async fn exit_with_a_hup_ignoring_child_returns_promptly() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let started = Instant::now();
    let out = h
        .exec(
            // The child ignores SIGHUP and keeps the terminal open as its stdout.
            "(trap '' HUP; exec sleep 100) & echo $! > bg.pid; exit 3",
            false,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!((out.exit_code, out.timed_out), (3, false), "{out:?}");
    let bg = std::fs::read_to_string(tmp.path().join("bg.pid")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(bg.trim()) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(bg.trim()), "the nohup child survived");
    assert_eq!(run(&h, "echo ok").await.output, "ok\n");
}

#[tokio::test]
async fn zero_timeout_means_default_and_signal_death_is_minus_one() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let out = h
        .exec("sleep 0.3; echo slow", false, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!((out.output.as_str(), out.timed_out), ("slow\n", false));

    let out = run(&h, "kill -9 $$").await;
    assert_eq!((out.exit_code, out.timed_out), (-1, false), "{out:?}");
    assert_eq!(run(&h, "echo ok").await.output, "ok\n");
}

/// Runs in a child process (umask is process-wide): re-executes this test binary with
/// only this test and `OQA_UMASK_CHILD=1`, which sets umask 000 and checks the modes.
#[tokio::test]
async fn command_dir_is_private_under_umask_000() {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("OQA_UMASK_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "command_dir_is_private_under_umask_000",
                "--nocapture",
            ])
            .env("OQA_UMASK_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success(), "the umask child failed");
        return;
    }
    nix::sys::stat::umask(nix::sys::stat::Mode::empty());
    let tmp = tempfile::tempdir().unwrap();
    let mut s = review_sandbox::BashSession::start(tmp.path()).unwrap();
    let out = s.exec("echo hi", T).await.unwrap();
    assert_eq!(out.output, "hi\n");
    let file = s.command_file().to_owned();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(file.parent().unwrap()), 0o700, "dir");
    assert_eq!(mode(&file), 0o600, "file");
    assert!(!file.starts_with(tmp.path()), "outside the project root");

    // A symlink planted in place of the file is not followed.
    std::fs::remove_file(&file).unwrap();
    let target = tmp.path().join("target");
    std::os::unix::fs::symlink(&target, &file).unwrap();
    assert!(s.exec("echo no", T).await.is_err());
    assert!(!target.exists(), "wrote through the symlink");
}

#[tokio::test]
async fn removed_command_dir_is_recreated_and_nul_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    run(&h, "export KEEP=1").await;
    let out = run(&h, "rm -rf \"$(dirname \"$BASH_SOURCE\")\"; echo gone").await;
    assert_eq!(out.output, "gone\n");
    let out = run(&h, "echo $KEEP").await;
    assert_eq!(out.output, "1\n", "same session, new dir");

    let err = h.exec("echo a\0b", false, T).await.unwrap_err();
    assert!(
        matches!(err, review_sandbox::OpError::Invalid(_)),
        "{err:?}"
    );
    assert_eq!(
        run(&h, "echo $KEEP").await.output,
        "1\n",
        "the session survives"
    );
}

/// Invalid UTF-8 renders as 3-byte U+FFFD per byte: the rendered output, not the raw bytes,
/// is what stays within [`OUTPUT_CAP`], so an `ExecResult` always fits one frame.
#[tokio::test]
async fn random_bytes_stay_within_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let out = h
        .exec(
            "head -c 12000000 /dev/urandom | cat",
            false,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(out.truncated, "exit {}", out.exit_code);
    let body = out
        .output
        .strip_prefix("[output truncated]\n")
        .expect("marker");
    assert!(body.len() <= OUTPUT_CAP, "{} bytes", body.len());
    assert!(
        out.output.len() <= OUTPUT_CAP + "[output truncated]\n".len(),
        "{} bytes",
        out.output.len()
    );
    // The shell still works afterwards.
    let out = run(&h, "echo ok").await;
    assert_eq!(out.output, "ok\n");
}

/// Set in the child process of [`credentials_are_not_in_the_shell_env`].
const ENV_CHILD: &str = "QA_SANDBOX_TEST_ENV_CHILD";

/// The host's `OTSIMOQA_*` variables (the token above all) never reach the agent's shell.
/// `set_var` is unsafe, so the test re-runs itself as a child process with the variables
/// set, and the child checks the shell's env.
#[tokio::test]
async fn credentials_are_not_in_the_shell_env() {
    if std::env::var_os(ENV_CHILD).is_some() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(tmp.path());
        let out = run(
            &h,
            "env | grep -E '^(OTSIMOQA|TMOUT)' ; echo \"seen=$OTHER_VAR\"",
        )
        .await;
        assert_eq!(out.output, "seen=kept\n", "{out:?}");
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "credentials_are_not_in_the_shell_env",
            "--nocapture",
        ])
        .env(ENV_CHILD, "1")
        .env("OTSIMOQA_TOKEN", "oqa_secret_marker")
        .env("OTSIMOQA_SERVER", "http://example")
        .env("TMOUT", "5")
        .env("OTHER_VAR", "kept")
        .status()
        .unwrap();
    assert!(status.success(), "the child test failed: {status}");
}

/// A shell that dies while idle is replaced before the next command: that command runs
/// and reports its own output, not the dead shell's end.
#[tokio::test]
async fn shell_that_died_idle_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let h = host(tmp.path());
    let out = run(
        &h,
        "echo $$ > sh.pid; (sleep 0.3; kill -9 $$) >/dev/null 2>&1 &",
    )
    .await;
    assert_eq!(out.exit_code, 0, "{out:?}");
    let sh = std::fs::read_to_string(tmp.path().join("sh.pid")).unwrap();
    let sh = sh.trim().to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(&sh) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Give the reader a moment past the kill as well.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out = run(&h, "echo ok; echo $$").await;
    let mut lines = out.output.lines();
    assert_eq!(lines.next(), Some("ok"), "{out:?}");
    assert_eq!(out.exit_code, 0, "{out:?}");
    assert_ne!(lines.next(), Some(sh.as_str()), "the same shell");
}
