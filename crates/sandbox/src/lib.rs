//! Local sandbox ops confined to one project root: the [`SandboxOps`] an agent
//! drives on this machine.
//!
//! [`LocalHost`] implements the file ops, `glob` and `grep` over a [`Root`]; every
//! path a caller passes is resolved against the canonical root and refused when it
//! would leave it ([`OpError::EscapesRoot`]). Only the file tools are confined: the
//! `exec` shell is unrestricted by design. On Unix, `exec` drives a persistent
//! [`BashSession`] on a PTY.
//!
//! On Unix, [`profile`] generates the OS-level sandbox policy (Seatbelt on
//! macOS, bubblewrap on Linux) that the `exec` shell runs under.

#[cfg(unix)]
mod bash;
mod env;
mod files;
mod ops;
mod paths;
#[cfg(unix)]
mod profile;
mod search;

#[cfg(unix)]
pub use bash::BashSession;
pub use env::{DENIED_ENV, EnvInput, Launch, agent_env};
pub use files::LocalHost;
pub use ops::{
    DEFAULT_EXEC_TIMEOUT, Entry, ExecOutput, GrepMatch, Listing, MAX_FILE_BYTES, MAX_GLOB,
    MAX_GREP, OUTPUT_CAP, OpError, SandboxOps,
};
pub use paths::Root;
#[cfg(unix)]
pub use profile::{
    SandboxPolicy, SandboxUnavailable, bwrap_args, default_deny_read, default_write_paths,
    sandboxed_launch, seatbelt_profile,
};
