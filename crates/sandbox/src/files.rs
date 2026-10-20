//! [`LocalHost`]: the sandbox ops on this machine, confined to a [`Root`].

use std::io::{ErrorKind, Read};
use std::path::Path;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use crate::bash::BashSession;

use crate::{
    Entry, ExecOutput, GrepMatch, Listing, MAX_FILE_BYTES, OpError, Root, SandboxOps, search,
};

/// The sandbox ops on this machine, confined to one project root. File ops run on
/// tokio's blocking pool; `exec` drives one persistent shell (Unix only), shared by
/// clones, started lazily and one command at a time.
#[derive(Debug, Clone)]
pub struct LocalHost {
    root: Root,
    #[cfg(unix)]
    shell: Arc<tokio::sync::Mutex<Option<BashSession>>>,
}

impl LocalHost {
    /// A host over `dir`, canonicalized once.
    pub fn new(dir: &Path) -> std::io::Result<LocalHost> {
        Ok(LocalHost {
            root: Root::new(dir)?,
            #[cfg(unix)]
            shell: Arc::default(),
        })
    }

    /// The project root.
    pub fn root(&self) -> &Root {
        &self.root
    }
}

/// Maps an I/O error on the caller's path `p`.
pub(crate) fn io_err(p: &str, e: std::io::Error) -> OpError {
    match e.kind() {
        ErrorKind::NotFound => OpError::NotFound(p.to_owned()),
        _ => OpError::Io(format!("{p}: {e}")),
    }
}

fn too_large(n: u64) -> OpError {
    OpError::TooLarge(format!(
        "file too large for one frame ({n} bytes); use bash to split or inspect it"
    ))
}

/// Runs blocking file work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, OpError> + Send + 'static,
) -> Result<T, OpError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| OpError::Io(format!("blocking task failed: {e}")))?
}

fn read(root: &Root, p: &str) -> Result<Vec<u8>, OpError> {
    let path = root.resolve(p)?;
    let meta = std::fs::metadata(&path).map_err(|e| io_err(p, e))?;
    if meta.is_dir() {
        return Err(OpError::Invalid(format!("is a directory: {p}")));
    }
    // Only regular files: opening a FIFO would block until a writer appears, and a
    // device or socket has no meaningful contents.
    if !meta.is_file() {
        return Err(OpError::Invalid(format!("not a regular file: {p}")));
    }
    if meta.len() > MAX_FILE_BYTES as u64 {
        return Err(too_large(meta.len()));
    }
    let file = std::fs::File::open(&path).map_err(|e| io_err(p, e))?;
    // It may have been swapped between the stat and the open.
    let meta = file.metadata().map_err(|e| io_err(p, e))?;
    if !meta.is_file() {
        return Err(OpError::Invalid(format!("not a regular file: {p}")));
    }
    // It may also grow: read one byte past the cap, never more.
    let mut data = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| io_err(p, e))?;
    if data.len() > MAX_FILE_BYTES {
        return Err(too_large(meta.len().max(data.len() as u64)));
    }
    Ok(data)
}

fn write(root: &Root, p: &str, data: &[u8]) -> Result<bool, OpError> {
    if data.len() > MAX_FILE_BYTES {
        return Err(too_large(data.len() as u64));
    }
    // `dir/` names a directory; a file write to it would drop the slash.
    if p.ends_with('/') || p.ends_with(std::path::MAIN_SEPARATOR) {
        return Err(OpError::Invalid(format!("is a directory: {p}")));
    }
    let path = root.resolve_new(p)?;
    if path == root.path() {
        return Err(OpError::Invalid(format!("is a directory: {p}")));
    }
    let created = match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => return Err(OpError::Invalid(format!("is a directory: {p}"))),
        Ok(_) => false,
        Err(e) if e.kind() == ErrorKind::NotFound => true,
        Err(e) => return Err(io_err(p, e)),
    };
    if let Some(parent) = path.parent() {
        // Every missing component is a plain name under a resolved, in-root directory.
        std::fs::create_dir_all(parent).map_err(|e| io_err(p, e))?;
    }
    std::fs::write(&path, data).map_err(|e| io_err(p, e))?;
    Ok(created)
}

fn list(root: &Root, p: &str) -> Result<Vec<Entry>, OpError> {
    let path = root.resolve(p)?;
    let meta = std::fs::metadata(&path).map_err(|e| io_err(p, e))?;
    if !meta.is_dir() {
        return Err(OpError::Invalid(format!("not a directory: {p}")));
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&path).map_err(|e| io_err(p, e))? {
        let entry = entry.map_err(|e| io_err(p, e))?;
        let Ok(lmeta) = entry.metadata() else {
            continue;
        };
        // A symlink reports its target only when the target is inside the root;
        // otherwise the link itself, so nothing outside leaks.
        let meta = if lmeta.file_type().is_symlink() {
            root.resolve_path(&entry.path())
                .and_then(|t| std::fs::metadata(t).ok())
                .unwrap_or(lmeta)
        } else {
            lmeta
        };
        out.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            size: meta.len(),
            is_dir: meta.is_dir(),
            modified: meta.modified().ok(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn exists(root: &Root, p: &str) -> Result<(bool, bool), OpError> {
    match root.resolve(p) {
        Ok(path) => Ok((true, path.is_dir())),
        Err(OpError::NotFound(_)) => Ok((false, false)),
        Err(e) => Err(e),
    }
}

#[async_trait::async_trait]
impl SandboxOps for LocalHost {
    #[cfg(unix)]
    async fn exec(
        &self,
        command: &str,
        restart: bool,
        timeout: Duration,
    ) -> Result<ExecOutput, OpError> {
        // A zero timeout (a proto default) means "the default", not "fail at once".
        let timeout = if timeout.is_zero() {
            crate::DEFAULT_EXEC_TIMEOUT
        } else {
            timeout
        };
        let mut shell = self.shell.lock().await;
        if restart {
            // Dropping a session kills its process group.
            shell.take();
            if command.trim().is_empty() {
                return Ok(ExecOutput {
                    output: "bash session restarted".into(),
                    ..ExecOutput::default()
                });
            }
        }
        if command.trim().is_empty() {
            return Err(OpError::Invalid("empty command".into()));
        }
        if command.contains('\0') {
            // Refused before the shell sees it, so the session survives.
            return Err(OpError::Invalid("command contains a NUL byte".into()));
        }
        // A shell that died while idle is replaced before the command is sent to it.
        if shell.as_mut().is_some_and(|s| !s.check_alive()) {
            shell.take();
        }
        let session = match &mut *shell {
            Some(s) => s,
            None => shell.insert(
                BashSession::start(self.root.path())
                    .map_err(|e| OpError::Io(format!("starting bash: {e}")))?,
            ),
        };
        let out = session.exec(command, timeout).await;
        // A timeout, an exited shell or any error: the next command gets a new shell.
        if out.is_err() || session.is_dead() {
            shell.take();
        }
        out
    }

    #[cfg(not(unix))]
    async fn exec(
        &self,
        _command: &str,
        _restart: bool,
        _timeout: Duration,
    ) -> Result<ExecOutput, OpError> {
        Err(OpError::Invalid("exec needs a Unix host".into()))
    }

    async fn read(&self, path: &str) -> Result<Vec<u8>, OpError> {
        let (root, p) = (self.root.clone(), path.to_owned());
        blocking(move || read(&root, &p)).await
    }

    async fn write(&self, path: &str, data: &[u8]) -> Result<bool, OpError> {
        if data.len() > MAX_FILE_BYTES {
            return Err(too_large(data.len() as u64));
        }
        let (root, p, d) = (self.root.clone(), path.to_owned(), data.to_vec());
        blocking(move || write(&root, &p, &d)).await
    }

    async fn list(&self, path: &str) -> Result<Vec<Entry>, OpError> {
        let (root, p) = (self.root.clone(), path.to_owned());
        blocking(move || list(&root, &p)).await
    }

    async fn glob(&self, pattern: &str, directory: &str) -> Result<Listing<String>, OpError> {
        let (root, pat, dir) = (self.root.clone(), pattern.to_owned(), directory.to_owned());
        blocking(move || search::glob(&root, &pat, &dir)).await
    }

    async fn grep(
        &self,
        pattern: &str,
        directory: &str,
        glob: Option<&str>,
    ) -> Result<Listing<GrepMatch>, OpError> {
        let (root, pat, dir) = (self.root.clone(), pattern.to_owned(), directory.to_owned());
        let glob = glob.map(str::to_owned);
        blocking(move || search::grep(&root, &pat, &dir, glob.as_deref())).await
    }

    async fn exists(&self, path: &str) -> Result<(bool, bool), OpError> {
        let (root, p) = (self.root.clone(), path.to_owned());
        blocking(move || exists(&root, &p)).await
    }
}
