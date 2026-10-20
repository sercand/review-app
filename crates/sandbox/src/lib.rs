//! Local sandbox ops confined to one project root: the [`SandboxOps`] an agent
//! drives on this machine.
//!
//! [`LocalHost`] implements the file ops, `glob` and `grep` over a [`Root`]; every
//! path a caller passes is resolved against the canonical root and refused when it
//! would leave it ([`OpError::EscapesRoot`]). Only the file tools are confined: the
//! `exec` shell is unrestricted by design. On Unix, `exec` drives a persistent
//! [`BashSession`] on a PTY.
//!
//! An OS-level sandbox (seatbelt etc.) is not part of this crate yet; it arrives
//! with the agent runtime.

#[cfg(unix)]
mod bash;
mod files;
mod ops;
mod paths;
mod search;

#[cfg(unix)]
pub use bash::BashSession;
pub use files::LocalHost;
pub use ops::{
    DEFAULT_EXEC_TIMEOUT, Entry, ExecOutput, GrepMatch, Listing, MAX_FILE_BYTES, MAX_GLOB,
    MAX_GREP, OUTPUT_CAP, OpError, SandboxOps,
};
pub use paths::Root;
