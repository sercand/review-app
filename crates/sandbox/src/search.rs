//! `glob` and `grep` over an `ignore` walk: `.gitignore` (also outside a git
//! repository) and `.ignore` files inside the project root apply, hidden files are
//! included, VCS directories (`.git`, `.jj`, `.hg`, `.svn`) are skipped, and symlinks
//! are never followed by the walk. A symlink entry is reported only when it resolves to a file
//! inside the root; links that leave the root, and links to directories, are skipped.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{GlobBuilder, GlobMatcher};
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match, WalkBuilder};

use crate::{GrepMatch, Listing, MAX_GLOB, MAX_GREP, OpError, Root};

/// The longest line text one grep match carries; longer lines are cut at a char
/// boundary, so a minified file cannot blow a frame.
const MAX_LINE_BYTES: usize = 2000;

fn matcher(pattern: &str) -> Result<GlobMatcher, OpError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| OpError::Invalid(format!("glob {pattern:?}: {e}")))
}

/// Directory names a walk never enters: version-control metadata.
const VCS_DIRS: [&str; 4] = [".git", ".jj", ".hg", ".svn"];

/// The ignore files read from in-root directories above a walk's start.
const IGNORE_FILES: [&str; 2] = [".gitignore", ".ignore"];

/// The ignore rules of the in-root strict ancestors of `dir`, deepest first. The walk
/// itself reads only the files at and below its start (`parents(false)`), so nothing
/// above the root ever applies.
fn ancestor_ignores(root: &Root, dir: &Path) -> Vec<Gitignore> {
    let mut out = Vec::new();
    let mut cur = dir.parent();
    while let Some(d) = cur {
        if !d.starts_with(root.path()) {
            break;
        }
        let mut b = GitignoreBuilder::new(d);
        let mut any = false;
        for name in IGNORE_FILES {
            let f = d.join(name);
            if f.is_file() {
                // A malformed line is skipped, as the walk does.
                let _ = b.add(f);
                any = true;
            }
        }
        if any && let Ok(g) = b.build() {
            out.push(g);
        }
        cur = d.parent();
    }
    out
}

/// The files under `dir` (canonical, inside the root) as `(entry path, real path)`:
/// the entry path is what results show, the real path what gets read.
///
/// Hidden files are included; VCS directories are skipped. Only ignore files inside
/// the root apply: the global git excludes and anything above the root are not read.
fn files(root: &Root, dir: &Path, sorted: bool) -> impl Iterator<Item = (PathBuf, PathBuf)> {
    let ancestors = ancestor_ignores(root, dir);
    let mut walk = WalkBuilder::new(dir);
    walk.require_git(false)
        .follow_links(false)
        .hidden(false)
        .parents(false)
        .git_global(false)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            if VCS_DIRS.iter().any(|v| e.file_name() == *v) {
                return false;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            for g in &ancestors {
                match g.matched_path_or_any_parents(e.path(), is_dir) {
                    Match::Ignore(_) => return false,
                    Match::Whitelist(_) => return true,
                    Match::None => {}
                }
            }
            true
        });
    if sorted {
        walk.sort_by_file_name(|a, b| a.cmp(b));
    }
    let root = root.clone();
    walk.build().filter_map(move |entry| {
        let entry = entry.ok()?;
        let ft = entry.file_type()?;
        if ft.is_file() {
            let p = entry.into_path();
            Some((p.clone(), p))
        } else if ft.is_symlink() {
            let real = root.resolve_path(entry.path())?;
            real.is_file().then(|| (entry.into_path(), real))
        } else {
            None
        }
    })
}

fn slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `glob`: files under `directory` whose path relative to it matches `pattern`
/// (`*` never crosses `/`), root-relative, newest first, capped at [`MAX_GLOB`].
pub(crate) fn glob(
    root: &Root,
    pattern: &str,
    directory: &str,
) -> Result<Listing<String>, OpError> {
    let m = matcher(pattern)?;
    let dir = root.resolve(directory)?;
    if !dir.is_dir() {
        return Err(OpError::Invalid(format!("not a directory: {directory}")));
    }
    let mut hits: Vec<(SystemTime, String)> = files(root, &dir, false)
        .filter(|(p, _)| p.strip_prefix(&dir).is_ok_and(|rel| m.is_match(slash(rel))))
        .map(|(p, real)| {
            let mtime = std::fs::metadata(&real)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (mtime, root.relative(&p))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let truncated = hits.len() > MAX_GLOB;
    hits.truncate(MAX_GLOB);
    Ok(Listing {
        items: hits.into_iter().map(|(_, p)| p).collect(),
        truncated,
    })
}

fn cut(line: &str) -> String {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.len() <= MAX_LINE_BYTES {
        return line.to_owned();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

/// `grep`: regex matches in the text files under `directory` (or in that one file),
/// optionally only files whose relative path or name matches `glob`. Binary files
/// (a NUL byte) are skipped. Files are visited in name order; capped at [`MAX_GREP`].
pub(crate) fn grep(
    root: &Root,
    pattern: &str,
    directory: &str,
    glob: Option<&str>,
) -> Result<Listing<GrepMatch>, OpError> {
    let re = RegexMatcher::new(pattern)
        .map_err(|e| OpError::Invalid(format!("regex {pattern:?}: {e}")))?;
    let filter = glob.map(matcher).transpose()?;
    let dir = root.resolve(directory)?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();
    let mut items = Vec::new();
    let mut truncated = false;
    for (p, real) in files(root, &dir, true) {
        if let Some(f) = &filter {
            let rel = p.strip_prefix(&dir).map(slash).unwrap_or_default();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !f.is_match(&rel) && !f.is_match(&name) {
                continue;
            }
        }
        let shown = root.relative(&p);
        let sink = sinks::Lossy(|line, text| {
            if items.len() == MAX_GREP {
                truncated = true;
                return Ok(false);
            }
            items.push(GrepMatch {
                path: shown.clone(),
                line,
                text: cut(text),
            });
            Ok(true)
        });
        // An unreadable file is skipped, as ripgrep does.
        let _ = searcher.search_path(&re, &real, sink);
        if truncated {
            break;
        }
    }
    Ok(Listing { items, truncated })
}
