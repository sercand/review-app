//! The [`SandboxOps`] trait, its result types and errors.

use std::time::{Duration, SystemTime};

/// At most this many paths come back from one `glob`.
pub const MAX_GLOB: usize = 300;
/// At most this many matches come back from one `grep`.
pub const MAX_GREP: usize = 200;
/// The largest file `read` returns or `write` accepts: it has to fit in one 16 MiB frame.
pub const MAX_FILE_BYTES: usize = 15 * 1024 * 1024;

/// How long one `exec` may run when the caller gives no timeout.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(120);
/// At most this many bytes of one command's output are kept, its tail.
pub const OUTPUT_CAP: usize = 10 * 1024 * 1024;

/// Why a sandbox op failed. [`OpError::code`] is the wire `ErrorResult` code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    /// The path (or directory) does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The path resolves outside the project root.
    #[error("path escapes the project root: {0}")]
    EscapesRoot(String),
    /// The file is too large for one frame; the message says what to do instead.
    #[error("{0}")]
    TooLarge(String),
    /// A bad argument: a malformed pattern, a directory where a file is expected, …
    #[error("invalid: {0}")]
    Invalid(String),
    /// Any other I/O failure.
    #[error("io: {0}")]
    Io(String),
    /// The op ran past its deadline.
    #[error("timed out")]
    Timeout,
    /// The host connection dropped while the op was in flight.
    #[error("sandbox disconnected")]
    Disconnected,
    /// The op was cancelled.
    #[error("cancelled")]
    Cancelled,
}

impl OpError {
    /// The stable code carried in `ErrorResult.code`.
    pub fn code(&self) -> &'static str {
        match self {
            OpError::NotFound(_) => "not_found",
            OpError::EscapesRoot(_) => "escapes_root",
            OpError::TooLarge(_) => "too_large",
            OpError::Invalid(_) => "invalid",
            OpError::Io(_) => "io",
            OpError::Timeout => "timeout",
            OpError::Disconnected => "disconnected",
            OpError::Cancelled => "cancelled",
        }
    }
}

/// The result of one shell command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// Merged stdout and stderr, ANSI-stripped, `\n` line ends.
    pub output: String,
    /// The exit code, `-1` when unknown (timed out, unparsable).
    pub exit_code: i32,
    /// The command hit its timeout and was killed.
    pub timed_out: bool,
    /// The output was capped and only its tail kept.
    pub truncated: bool,
}

/// One directory entry from `list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name, no directory part.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Whether it is a directory (an in-root symlink to one counts).
    pub is_dir: bool,
    /// Last modification time, when the platform has it.
    pub modified: Option<SystemTime>,
}

/// One `grep` hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// Root-relative, slash-separated path.
    pub path: String,
    /// 1-based line number.
    pub line: u64,
    /// The matching line, without its line terminator.
    pub text: String,
}

/// A capped list of results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing<T> {
    /// The results, at most the op's cap.
    pub items: Vec<T>,
    /// More results existed than the cap allowed.
    pub truncated: bool,
}

/// What a sandbox can do. Paths are relative to the project root (absolute paths
/// inside it are accepted); results carry root-relative, slash-separated paths.
///
/// Dyn-compatible: callers can drive any host as `Arc<dyn SandboxOps>`.
#[async_trait::async_trait]
pub trait SandboxOps: Send + Sync {
    /// Runs `command` in the persistent shell. `restart` restarts the shell first.
    async fn exec(
        &self,
        command: &str,
        restart: bool,
        timeout: Duration,
    ) -> Result<ExecOutput, OpError>;
    /// Reads a whole file, at most [`MAX_FILE_BYTES`].
    async fn read(&self, path: &str) -> Result<Vec<u8>, OpError>;
    /// Writes a whole file, creating parent directories. Returns whether it was created.
    async fn write(&self, path: &str, data: &[u8]) -> Result<bool, OpError>;
    /// Lists a directory, sorted by name.
    async fn list(&self, path: &str) -> Result<Vec<Entry>, OpError>;
    /// Files under `directory` matching `pattern` (relative to `directory`), newest first,
    /// capped at [`MAX_GLOB`].
    async fn glob(&self, pattern: &str, directory: &str) -> Result<Listing<String>, OpError>;
    /// Regex matches under `directory`, optionally only in files matching `glob`, capped
    /// at [`MAX_GREP`].
    async fn grep(
        &self,
        pattern: &str,
        directory: &str,
        glob: Option<&str>,
    ) -> Result<Listing<GrepMatch>, OpError>;
    /// Whether the path exists, and whether it is a directory.
    async fn exists(&self, path: &str) -> Result<(bool, bool), OpError>;
}
