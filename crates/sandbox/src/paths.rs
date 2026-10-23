//! Project-root confinement: the security core of the file ops.
//!
//! A caller's path is resolved the way the kernel would, one component at a time,
//! following every symlink (relative or absolute, dangling or not) and applying `..`
//! to the already-resolved real directory. The result is then checked for
//! whole-segment containment in the canonical root. Because symlinks are followed
//! even when their target does not exist, a dangling link pointing outside is refused
//! as an escape rather than reported missing, and a write through it cannot create a
//! file outside the root. Once a component is missing, the rest of the path is taken
//! literally and a later `..` is refused, since its meaning would depend on
//! directories that do not exist yet.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use crate::OpError;

/// How many symlinks one resolution may follow (Linux's `MAXSYMLINKS`).
const MAX_LINKS: usize = 40;

/// The canonical project root every sandbox path is resolved against.
#[derive(Debug, Clone)]
pub struct Root {
    path: PathBuf,
}

/// One component still to resolve.
enum Part {
    Root(OsString),
    Parent,
    Name(OsString),
}

fn parts(p: &Path) -> impl Iterator<Item = Part> + '_ {
    p.components().filter_map(|c| match c {
        Component::Prefix(pre) => Some(Part::Root(pre.as_os_str().to_owned())),
        Component::RootDir => Some(Part::Root(std::path::MAIN_SEPARATOR_STR.into())),
        Component::CurDir => None,
        Component::ParentDir => Some(Part::Parent),
        Component::Normal(n) => Some(Part::Name(n.to_owned())),
    })
}

impl Root {
    /// Canonicalizes `dir` once (so `/var/…` on macOS becomes `/private/var/…`).
    pub fn new(dir: &Path) -> std::io::Result<Root> {
        Ok(Root {
            path: std::fs::canonicalize(dir)?,
        })
    }

    /// The canonical root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves a path that must exist. `""` and `"."` are the root.
    ///
    /// Errors: [`OpError::EscapesRoot`] when it resolves outside the root (checked
    /// first, so a dangling link to outside is an escape), [`OpError::NotFound`] when
    /// it does not exist.
    pub fn resolve(&self, p: &str) -> Result<PathBuf, OpError> {
        let (path, exists) = self.resolve_any(p)?;
        if exists {
            Ok(path)
        } else {
            Err(OpError::NotFound(p.to_owned()))
        }
    }

    /// Resolves a path that may not exist yet, for writes. The returned path has
    /// every symlink resolved, so writing to it (and creating its missing parents)
    /// stays inside the root.
    pub fn resolve_new(&self, p: &str) -> Result<PathBuf, OpError> {
        self.resolve_any(p).map(|(path, _)| path)
    }

    /// Resolves an entry found by a walk under the root; `None` when it escapes or is
    /// gone.
    pub(crate) fn resolve_path(&self, p: &Path) -> Option<PathBuf> {
        match self.resolve_parts(p, &p.to_string_lossy()) {
            Ok((path, true)) => Some(path),
            _ => None,
        }
    }

    /// Root-relative, slash-separated form of a path inside the root (`""` for the
    /// root itself).
    pub fn relative(&self, p: &Path) -> String {
        let rel = p.strip_prefix(&self.path).unwrap_or(p);
        let segs: Vec<_> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_string_lossy()),
                _ => None,
            })
            .collect();
        segs.join("/")
    }

    /// The resolved real path and whether it exists.
    fn resolve_any(&self, p: &str) -> Result<(PathBuf, bool), OpError> {
        if p.contains('\0') {
            return Err(OpError::Invalid(format!("path contains a NUL byte: {p:?}")));
        }
        self.resolve_parts(Path::new(p), p)
    }

    fn resolve_parts(&self, p: &Path, shown: &str) -> Result<(PathBuf, bool), OpError> {
        let mut pending: VecDeque<Part> = parts(p).collect();
        // A relative path starts at the root; an absolute one resets `current`.
        let mut current = self.path.clone();
        let mut missing = false;
        let mut links = 0usize;
        while let Some(part) = pending.pop_front() {
            match part {
                Part::Root(r) => {
                    if missing {
                        // Only reachable through a symlink, which a missing
                        // component cannot be.
                        return Err(OpError::Invalid(shown.to_owned()));
                    }
                    current = PathBuf::from(r);
                }
                Part::Parent => {
                    if missing {
                        return Err(OpError::Invalid(format!(
                            "'..' after a missing component: {shown}"
                        )));
                    }
                    // `current` has no symlinks, so its lexical parent is the real one.
                    current.pop();
                }
                Part::Name(name) => {
                    let next = current.join(&name);
                    if missing {
                        current = next;
                        continue;
                    }
                    match std::fs::symlink_metadata(&next) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            links += 1;
                            if links > MAX_LINKS {
                                return Err(OpError::Invalid(format!(
                                    "too many levels of symbolic links: {shown}"
                                )));
                            }
                            let target = std::fs::read_link(&next)
                                .map_err(|e| OpError::Io(format!("{shown}: {e}")))?;
                            // Resolve the target next, relative to the link's directory
                            // (`current`), or from `/` when it is absolute.
                            for t in parts(&target).collect::<Vec<_>>().into_iter().rev() {
                                pending.push_front(t);
                            }
                        }
                        Ok(_) => current = next,
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::NotFound | ErrorKind::NotADirectory
                            ) =>
                        {
                            missing = true;
                            current = next;
                        }
                        Err(e) => return Err(OpError::Io(format!("{shown}: {e}"))),
                    }
                }
            }
        }
        // Whole-segment containment: `/root-other` does not start with `/root`.
        if !current.starts_with(&self.path) {
            return Err(OpError::EscapesRoot(shown.to_owned()));
        }
        Ok((current, !missing))
    }
}
