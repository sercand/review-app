//! [`BashSession`]: one persistent `bash` on a PTY, driven a command at a time.
//!
//! A command never goes through the terminal: it is written to a private file in a
//! per-session temp dir outside the project root and sourced by a short fixed line,
//! so TABs (readline completion), control characters and long lines (the tty's
//! canonical-mode line limit) cannot mangle it.
//!
//! Each command is wrapped so that, when it finishes, the shell prints a per-command
//! sentinel (`__OQA_CMD_<16 hex>_DONE__`) followed by `$?`. The sentinel is written
//! in two quoted halves, so neither the PTY echo of the wrapper nor anything the
//! command prints can contain it without knowing the random nonce. A reader thread
//! copies the PTY into a buffer that keeps at most about [`OUTPUT_CAP`] bytes of the
//! current command's tail; [`BashSession::exec`] waits on a [`Notify`] until the
//! sentinel shows up or the timeout passes. A timeout kills the shell's whole process
//! group (background children included), and the session is then dead: the owner
//! starts a new one.

use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::{ExecOutput, OUTPUT_CAP, OpError};

/// Run once when the shell starts: no echo (stty needs the terminal, so no
/// `</dev/null`), no job control (background jobs stay in the shell's process group,
/// so a kill reaches them), no `!` history expansion, no history at all, and no
/// readline completion.
const SETUP: &str = "stty -echo 2>/dev/null; set +m; set +H; set +o history; \
                     bind 'set disable-completion on' 2>/dev/null";
/// The PTY is read in chunks of this size.
const CHUNK: usize = 4096;
const TRUNCATED: &str = "[output truncated]\n";
const TIMED_OUT: &str = "\n[timed out]";
/// How long to wait for a shell that hit EOF to report its exit status.
const REAP_WAIT: Duration = Duration::from_secs(1);
/// How often a waiting `exec` checks whether the shell itself has exited (a group
/// member that ignores SIGHUP can keep the PTY open, so EOF alone is not enough).
const POLL: Duration = Duration::from_millis(100);
/// After the shell is seen to exit, how long to keep collecting its last output.
const EXIT_GRACE: Duration = Duration::from_millis(100);

/// What the reader thread shares with [`BashSession::exec`].
#[derive(Default)]
struct Buf {
    /// The tail of everything read from the PTY since the buffer was last reset.
    bytes: Vec<u8>,
    /// Absolute offset (bytes ever read) of `bytes[0]`.
    base: u64,
    /// The PTY reached EOF or failed: every holder of the terminal is gone.
    eof: bool,
}

impl Buf {
    fn end(&self) -> u64 {
        self.base + self.bytes.len() as u64
    }

    /// `bytes` from absolute offset `from` (clamped to what is kept) to `to`.
    fn slice(&self, from: u64, to: u64) -> &[u8] {
        let lo = from.saturating_sub(self.base) as usize;
        let hi = (to.saturating_sub(self.base) as usize).min(self.bytes.len());
        &self.bytes[lo.min(hi)..hi]
    }
}

struct Shared {
    buf: Mutex<Buf>,
    notify: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Buf> {
        self.buf.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One persistent `bash --noprofile --norc` on a PTY. One command runs at a time; the
/// owner serialises calls and replaces the session once [`BashSession::is_dead`].
pub struct BashSession {
    child: Box<dyn Child + Send + Sync>,
    /// Kept so the PTY stays open while the session lives.
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    shared: Arc<Shared>,
    /// The private (0700) dir holding the command file; removed when the session drops.
    cmd_dir: tempfile::TempDir,
    /// The command file (0600) each `exec` writes and the shell sources.
    cmd_file: PathBuf,
    pid: Option<u32>,
    /// The marker the setup line prints; `exec` waits for it once, before the first command.
    ready_marker: Option<String>,
    /// The shell exited, was killed, or its PTY failed.
    dead: bool,
    /// The child was waited for: its pid may be reused, so it is never signalled again.
    reaped: bool,
}

impl std::fmt::Debug for BashSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashSession")
            .field("pid", &self.pid)
            .field("dead", &self.dead)
            .finish_non_exhaustive()
    }
}

fn io(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// A fresh sentinel as its two halves; the sentinel itself is `a + b`.
fn sentinel() -> (String, String) {
    let nonce = format!("{:016x}", rand::random::<u64>());
    (
        format!("__OQA_CMD_{}", &nonce[..8]),
        format!("{}_DONE__", &nonce[8..]),
    )
}

/// How a wait for a marker ended.
enum Outcome {
    /// The marker arrived; the text after it up to its newline.
    Found(Vec<u8>),
    /// The PTY closed first: the shell exited, status not yet collected.
    Eof,
    /// The shell exited (and was reaped) with this code.
    Exited(i32),
    /// The deadline passed.
    Timeout,
}

/// What a wait saw: how it ended, the raw bytes from the start up to the marker (or
/// to the end so far), and whether the head of those bytes was dropped.
struct Seen {
    outcome: Outcome,
    raw: Vec<u8>,
    truncated: bool,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let first = *needle.first()?;
    let mut i = 0;
    while let Some(j) = hay[i..].iter().position(|&c| c == first) {
        let at = i + j;
        if hay.len() - at < needle.len() {
            return None;
        }
        if &hay[at..at + needle.len()] == needle {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// The `exec` code for a shell's exit status: `-1` when a signal killed it.
fn status_code(status: &portable_pty::ExitStatus) -> i32 {
    if status.signal().is_some() {
        -1
    } else {
        i32::try_from(status.exit_code()).unwrap_or(-1)
    }
}

/// A new private (0700, whatever the umask) dir for the command file, outside the
/// project root.
fn command_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("otsimoqa-bash-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
}

/// Writes `command` to `path`: 0600, truncated, never through a symlink.
fn write_command(path: &Path, command: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)?;
    f.write_all(command.as_bytes())
}

/// Whether the env var `key` is kept from the shell: every `OTSIMOQA_*` and `TMOUT`.
fn is_scrubbed(key: &std::ffi::OsStr) -> bool {
    let key = key.as_encoded_bytes();
    key.starts_with(b"OTSIMOQA_") || key == b"TMOUT"
}

/// `p` as a single-quoted shell word.
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// Strips ANSI CSI sequences (`\x1b\[[0-9;?]*[ -/]*[@-~]`) and every CR.
fn clean(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if c == 0x1b && raw.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < raw.len() && (raw[j].is_ascii_digit() || raw[j] == b';' || raw[j] == b'?') {
                j += 1;
            }
            while j < raw.len() && (0x20..=0x2f).contains(&raw[j]) {
                j += 1;
            }
            if j < raw.len() && (0x40..=0x7e).contains(&raw[j]) {
                i = j + 1;
                continue;
            }
        }
        if c != b'\r' {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// The text an `exec` returns, and whether it was truncated: cleaned, leading newlines
/// trimmed, at most [`OUTPUT_CAP`] bytes of rendered text (its tail, cut on a char
/// boundary), with the truncation marker first when the head was dropped.
///
/// The cap applies to the rendered `String`, not the raw bytes: each invalid byte becomes
/// a 3-byte U+FFFD, so capping the raw bytes alone could still overflow a frame.
fn render(raw: &[u8], truncated: bool) -> (String, bool) {
    render_capped(raw, truncated, OUTPUT_CAP)
}

fn render_capped(raw: &[u8], mut truncated: bool, cap: usize) -> (String, bool) {
    let cleaned = clean(raw);
    let start = cleaned
        .iter()
        .position(|&c| c != b'\n')
        .unwrap_or(cleaned.len());
    let mut text = String::from_utf8_lossy(&cleaned[start..]).into_owned();
    if text.len() > cap {
        let mut cut = text.len() - cap;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text.drain(..cut);
        truncated = true;
    }
    if truncated {
        text.insert_str(0, TRUNCATED);
    }
    (text, truncated)
}

/// Disarmed when an `exec` finishes normally; dropped mid-command (the caller's future
/// was cancelled), it kills the shell so nothing keeps running unobserved.
struct InFlight<'a> {
    session: &'a mut BashSession,
    armed: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.kill();
        }
    }
}

impl BashSession {
    /// Starts `bash --noprofile --norc` on a new PTY in `cwd`, with the host env plus
    /// `PS1= PS2= TERM=dumb`, and sends the setup line.
    pub fn start(cwd: &Path) -> std::io::Result<BashSession> {
        // Before the shell exists, so a failure here leaks nothing.
        let cmd_dir = command_dir()?;
        let cmd_file = cmd_dir.path().join("cmd");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io)?;
        let mut cmd = CommandBuilder::new("bash");
        cmd.args(["--noprofile", "--norc"]);
        // The agent's shell must not see the host's credentials (`OTSIMOQA_TOKEN`, …),
        // nor a `TMOUT` that would end an idle shell behind the session's back.
        for (key, _) in std::env::vars_os() {
            if is_scrubbed(&key) {
                cmd.env_remove(&key);
            }
        }
        cmd.env("PS1", "");
        cmd.env("PS2", "");
        cmd.env("TERM", "dumb");
        cmd.cwd(cwd);
        // portable-pty runs the child under `setsid`, so its process group id is its pid.
        let mut child = pair.slave.spawn_command(cmd).map_err(io)?;
        // Our copy of the slave must close, or the reader never sees EOF.
        drop(pair.slave);
        let pid = child.process_id();
        let handles = pair
            .master
            .try_clone_reader()
            .and_then(|r| Ok((r, pair.master.take_writer()?)));
        let (mut reader, writer) = match handles {
            Ok(h) => h,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io(e));
            }
        };

        let shared = Arc::new(Shared {
            buf: Mutex::new(Buf::default()),
            notify: Notify::new(),
        });
        let feed = Arc::clone(&shared);
        let mut s = BashSession {
            child,
            _master: pair.master,
            writer,
            shared,
            cmd_dir,
            cmd_file,
            pid,
            ready_marker: None,
            dead: false,
            reaped: false,
        };
        let spawned = std::thread::Builder::new()
            .name("review-sandbox-pty".into())
            .spawn(move || {
                let mut chunk = [0u8; CHUNK];
                loop {
                    let n = match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut b = feed.lock();
                    b.bytes.extend_from_slice(&chunk[..n]);
                    // Keep the tail; drop the head in big steps so the cost stays linear.
                    if b.bytes.len() > 2 * OUTPUT_CAP {
                        let drop_n = b.bytes.len() - OUTPUT_CAP;
                        b.bytes.drain(..drop_n);
                        b.base += drop_n as u64;
                    }
                    drop(b);
                    feed.notify.notify_one();
                }
                feed.lock().eof = true;
                feed.notify.notify_one();
            });
        if let Err(e) = spawned {
            s.kill();
            return Err(e);
        }
        let (a, b) = sentinel();
        if let Err(e) = s.send(&format!("{SETUP}; printf '{a}''{b}\\n'\n")) {
            s.kill();
            return Err(e);
        }
        s.ready_marker = Some(format!("{a}{b}"));
        Ok(s)
    }

    /// The file each command is written to and sourced from (in a private temp dir).
    pub fn command_file(&self) -> &Path {
        &self.cmd_file
    }

    /// Whether the shell is gone (it exited, timed out or failed) and must be replaced.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Checks whether the shell is still there before a command is written to it: a shell
    /// that died while idle (killed, or exited on its own) has closed its PTY or exited.
    /// Such a session is marked dead (and its group killed), so the owner starts a fresh
    /// one instead of reporting the old shell's end as the next command's result.
    pub fn check_alive(&mut self) -> bool {
        if self.dead {
            return false;
        }
        let eof = self.shared.lock().eof;
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.reaped = true;
                self.dead = true;
                // The group may outlive the shell; its id is not reused while it has members.
                self.killpg();
            }
            Ok(None) if !eof => {}
            // EOF with the shell still running (it is exiting), or `try_wait` failed.
            _ => self.kill(),
        }
        !self.dead
    }

    /// Runs one command and waits for it, at most `timeout`.
    ///
    /// A timeout is not an error: it returns the output so far plus `[timed out]`,
    /// exit `-1`, and kills the session. A command that exits the shell returns the
    /// shell's exit status and leaves the session dead. Errors (the PTY failed, the
    /// shell never started) also leave it dead. Dropping the future mid-command kills
    /// the session too.
    pub async fn exec(&mut self, command: &str, timeout: Duration) -> Result<ExecOutput, OpError> {
        if self.dead {
            return Err(OpError::Io("bash session is gone".into()));
        }
        if command.contains('\0') {
            // bash would silently cut the command at the NUL.
            return Err(OpError::Invalid("command contains a NUL byte".into()));
        }
        let deadline = Instant::now() + timeout;
        let mut guard = InFlight {
            session: self,
            armed: true,
        };
        let s = &mut *guard.session;
        let shared = Arc::clone(&s.shared);

        if let Some(marker) = s.ready_marker.take() {
            let start = s.shared.lock().base;
            let seen =
                Self::wait_for(&shared, &mut *s.child, marker.as_bytes(), start, deadline).await;
            if !matches!(seen.outcome, Outcome::Found(_)) {
                return Err(OpError::Io(format!(
                    "bash did not start: {}",
                    render(&seen.raw, seen.truncated).0.trim_end()
                )));
            }
        }

        // Drop whatever arrived between commands; this command's output starts here.
        let start = {
            let mut b = s.shared.lock();
            b.base = b.end();
            b.bytes.clear();
            b.base
        };
        let mut written = write_command(&s.cmd_file, command);
        if matches!(&written, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            // A command removed the dir: make a new one rather than lose the session.
            written = command_dir().and_then(|d| {
                s.cmd_dir = d;
                s.cmd_file = s.cmd_dir.path().join("cmd");
                write_command(&s.cmd_file, command)
            });
        }
        written.map_err(|e| OpError::Io(format!("writing the command file: {e}")))?;
        let (a, b) = sentinel();
        let wrapped = format!(
            "{{ . {}\n}} </dev/null 2>&1\nprintf '\\n{a}''{b}%d\\n' $?\n",
            shell_quote(&s.cmd_file)
        );
        s.send(&wrapped)
            .map_err(|e| OpError::Io(format!("writing to bash: {e}")))?;
        let marker = format!("{a}{b}");
        let seen = Self::wait_for(&shared, &mut *s.child, marker.as_bytes(), start, deadline).await;

        let out = match seen.outcome {
            Outcome::Found(rest) => {
                let mut raw = seen.raw;
                // The wrapper's printf starts with a newline of its own.
                if raw.ends_with(b"\r\n") {
                    raw.truncate(raw.len() - 2);
                } else if raw.ends_with(b"\n") {
                    raw.truncate(raw.len() - 1);
                }
                let code = std::str::from_utf8(&rest)
                    .ok()
                    .and_then(|t| t.trim().parse().ok())
                    .unwrap_or(-1);
                let (output, truncated) = render(&raw, seen.truncated);
                ExecOutput {
                    output,
                    exit_code: code,
                    timed_out: false,
                    truncated,
                }
            }
            Outcome::Eof => {
                let code = s.reap().await;
                let (output, truncated) = render(&seen.raw, seen.truncated);
                ExecOutput {
                    output,
                    exit_code: code,
                    timed_out: false,
                    truncated,
                }
            }
            Outcome::Exited(code) => {
                // `try_wait` reaped the shell; a group member may outlive it (a SIGHUP-
                // ignoring background job). POSIX does not reuse a process group id while
                // the group has members, so this reaches only them.
                s.reaped = true;
                s.dead = true;
                s.killpg();
                let (output, truncated) = render(&seen.raw, seen.truncated);
                ExecOutput {
                    output,
                    exit_code: code,
                    timed_out: false,
                    truncated,
                }
            }
            Outcome::Timeout => {
                s.kill();
                let (output, truncated) = render(&seen.raw, seen.truncated);
                ExecOutput {
                    output: format!("{output}{TIMED_OUT}"),
                    exit_code: -1,
                    timed_out: true,
                    truncated,
                }
            }
        };
        guard.armed = false;
        Ok(out)
    }

    /// SIGKILLs the shell's whole process group (background children included), then
    /// reaps the shell. Idempotent; the session is dead afterwards.
    pub fn kill(&mut self) {
        self.dead = true;
        if self.reaped {
            return;
        }
        self.killpg();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }

    /// SIGKILL to the shell's process group.
    fn killpg(&self) {
        if let Some(pid) = self.pid.and_then(|p| i32::try_from(p).ok()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }

    fn send(&mut self, text: &str) -> std::io::Result<()> {
        let r = self
            .writer
            .write_all(text.as_bytes())
            .and_then(|()| self.writer.flush());
        if r.is_err() {
            self.dead = true;
        }
        r
    }

    /// After EOF: the shell's exit status, waiting briefly for it to exit; `-1` (and a
    /// kill) when it does not.
    async fn reap(&mut self) -> i32 {
        self.dead = true;
        // The shell is exiting (or a zombie): signal its group while the pid is still
        // ours, before `try_wait` reaps it and the id can be reused. A SIGKILL to a
        // zombie leaves its exit status as it was.
        self.killpg();
        let until = Instant::now() + REAP_WAIT;
        while Instant::now() < until {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    return status_code(&status);
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(_) => break,
            }
        }
        self.kill();
        -1
    }

    /// Waits until `marker` (and the line after it) appears after absolute offset
    /// `start`, the PTY closes, the shell exits, or `deadline` passes; on the deadline
    /// it checks the buffer one last time.
    async fn wait_for(
        shared: &Shared,
        child: &mut (dyn Child + Send + Sync),
        marker: &[u8],
        start: u64,
        deadline: Instant,
    ) -> Seen {
        let mut scan = start;
        let mut last = false;
        // The shell's exit code, and until when its last output is still collected.
        let mut exited: Option<(i32, Instant)> = None;
        loop {
            {
                let b = shared.lock();
                let from = scan.max(b.base);
                let end = b.end();
                if let Some(i) = find(b.slice(from, end), marker) {
                    let pos = from + i as u64;
                    let rest = b.slice(pos + marker.len() as u64, end);
                    if let Some(nl) = rest.iter().position(|&c| c == b'\n') {
                        let rest = rest[..nl].to_vec();
                        return seen(&b, start, pos, Outcome::Found(rest));
                    }
                    scan = pos;
                } else {
                    scan = end.saturating_sub(marker.len() as u64 - 1).max(from);
                }
                match exited {
                    Some((code, until)) if b.eof || Instant::now() >= until => {
                        return seen(&b, start, end, Outcome::Exited(code));
                    }
                    None if b.eof => return seen(&b, start, end, Outcome::Eof),
                    _ => {}
                }
                if last {
                    return seen(&b, start, end, Outcome::Timeout);
                }
            }
            if exited.is_none()
                && let Ok(Some(status)) = child.try_wait()
            {
                exited = Some((status_code(&status), Instant::now() + EXIT_GRACE));
            }
            let wake = match exited {
                Some((_, until)) => until,
                None => Instant::now() + POLL,
            }
            .min(deadline);
            let _ = tokio::time::timeout_at(wake, shared.notify.notified()).await;
            if Instant::now() >= deadline {
                last = true;
            }
        }
    }
}

/// The bytes from `start` to `to`, capped at [`OUTPUT_CAP`] (keeping the tail).
fn seen(b: &Buf, start: u64, to: u64, outcome: Outcome) -> Seen {
    let mut raw = b.slice(start, to);
    let mut truncated = start < b.base;
    if raw.len() > OUTPUT_CAP {
        raw = &raw[raw.len() - OUTPUT_CAP..];
        truncated = true;
    }
    Seen {
        outcome,
        raw: raw.to_vec(),
        truncated,
    }
}

impl Drop for BashSession {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_ansi_and_cr() {
        assert_eq!(clean(b"\x1b[31mred\x1b[0m\r\n\x1b[?2004hx\ry"), b"red\nxy");
        assert_eq!(
            clean(b"\x1b]0;title"),
            b"\x1b]0;title",
            "only CSI sequences"
        );
    }

    #[test]
    fn render_trims_leading_newlines_and_marks_truncation() {
        assert_eq!(render(b"\r\n\nhi\n", false), ("hi\n".into(), false));
        assert_eq!(
            render(b"tail", true),
            ("[output truncated]\ntail".into(), true)
        );
    }

    #[test]
    fn render_caps_the_rendered_text_on_a_char_boundary() {
        // Each invalid byte becomes a 3-byte U+FFFD: 7 raw bytes render as 13.
        let (text, truncated) = render_capped(b"ab\xff\xff\xffcd", false, 8);
        assert!(truncated);
        let body = text.strip_prefix(TRUNCATED).unwrap();
        assert!(body.len() <= 8, "{body:?}");
        assert_eq!(body, "\u{fffd}\u{fffd}cd");
        let (text, truncated) = render_capped(b"abc", false, 8);
        assert_eq!((text.as_str(), truncated), ("abc", false));
    }

    #[test]
    fn find_locates_needles() {
        assert_eq!(find(b"aab_xx_DONE", b"_DONE"), Some(6));
        assert_eq!(find(b"_DON", b"_DONE"), None);
        assert_eq!(find(b"", b"x"), None);
    }
}
