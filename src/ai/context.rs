//! Repo context read from the checked-out PR head (spec §6.6): the free-form
//! `.github/REVIEW.md` with its `## Checks` checklist, the `AGENTS.md` and
//! `CLAUDE.md` files scoped by directory, and the `.claude/skills` index.
//!
//! [`load_repo_context`] never fails: every unreadable file is skipped with a
//! `tracing::debug!` that names the path, never its contents.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// One checklist item parsed out of `.github/REVIEW.md`'s `## Checks` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoCheck {
    /// Stable id: `repo:1`, `repo:2`, … in document order.
    pub id: String,
    /// The bullet text, without a `[<glob>] ` prefix.
    pub text: String,
    /// The path glob the bullet scoped itself to, if it started with one.
    pub glob: Option<String>,
}

/// One `AGENTS.md` or `CLAUDE.md`, with `@path.md` imports expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    /// The directory it applies to, repo-relative with forward slashes;
    /// `""` for the repo root.
    pub dir: String,
    /// The file itself, repo-relative with forward slashes.
    pub path: String,
    /// The content, with `@path.md` import lines expanded one level deep.
    pub content: String,
}

/// One `.claude/skills/<dir>/SKILL.md`, as the index shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillInfo {
    /// The frontmatter `name`, or the folder name when there is none.
    pub name: String,
    /// The frontmatter `description`, empty when there is none.
    pub description: String,
    /// The skill's directory, repo-relative with forward slashes.
    pub dir: String,
}

/// Everything the repo itself tells the agents (spec §6.6).
#[derive(Clone, Debug, Default)]
pub struct RepoContext {
    /// `.github/REVIEW.md`, verbatim.
    pub review_md: Option<String>,
    /// The checks parsed out of its `## Checks` section.
    pub checks: Vec<RepoCheck>,
    /// The `AGENTS.md` / `CLAUDE.md` files, sorted by path.
    pub instructions: Vec<InstructionFile>,
    /// The skills, sorted by directory.
    pub skills: Vec<SkillInfo>,
}

impl RepoContext {
    /// The instruction files that apply to any of `paths` (repo-relative):
    /// the root ones, plus nested ones whose directory is an ancestor of a
    /// path.
    pub fn instructions_for(&self, paths: &[&str]) -> Vec<&InstructionFile> {
        self.instructions
            .iter()
            .filter(|file| {
                file.dir.is_empty()
                    || paths
                        .iter()
                        .any(|path| path.starts_with(&format!("{}/", file.dir)))
            })
            .collect()
    }

    /// The skills as `- name: description` lines, one per skill, for a prompt.
    pub fn skill_index(&self) -> String {
        self.skills
            .iter()
            .map(|skill| format!("- {}: {}", skill.name, skill.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Gather the repo context from the checkout at `root`. Never fails; an
/// unreadable file is skipped with a `tracing::debug!`.
pub fn load_repo_context(root: &Path) -> RepoContext {
    let review_md = read_to_string(&root.join(".github").join("REVIEW.md"));
    let checks = review_md.as_deref().map(parse_checks).unwrap_or_default();
    RepoContext {
        review_md,
        checks,
        instructions: load_instructions(root),
        skills: load_skills(root),
    }
}

/// File names that scope instructions to their directory, and the cap on how
/// many the walk collects.
const INSTRUCTION_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
const MAX_INSTRUCTION_FILES: usize = 50;

/// Read a file, skipping it — with a debug log naming the path, never the
/// contents — when it is missing or unreadable.
fn read_to_string(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) => {
            tracing::debug!(path = %path.display(), "repo context: skipping unreadable file: {error}");
            None
        }
    }
}

/// The repo-relative form of `path` with forward slashes.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The bullets under the first `## Checks` heading (case-insensitive), up to
/// the next `##` heading, as `repo:<n>` checks.
fn parse_checks(markdown: &str) -> Vec<RepoCheck> {
    let mut checks: Vec<RepoCheck> = Vec::new();
    let mut in_checks = false;
    let mut seen_checks = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if in_checks {
                // Only a level-2 heading ends the section; `#` and `###`+
                // headings are ordinary lines — and not bullets, so the loop
                // simply skips them.
                if hashes == 2 {
                    in_checks = false;
                }
            } else {
                let heading = trimmed[hashes..].trim();
                in_checks = hashes == 2 && !seen_checks && heading.eq_ignore_ascii_case("checks");
                seen_checks |= in_checks;
            }
            continue;
        }
        if !in_checks {
            continue;
        }
        // Top-level bullets only: an indented line belongs to the bullet above.
        if trimmed.len() != line.len() {
            continue;
        }
        let Some(text) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        // A leading `[<glob>] ` scopes the check to matching paths.
        let (glob, text) = match text
            .strip_prefix('[')
            .and_then(|rest| rest.split_once("] "))
        {
            Some((glob, rest)) if !glob.trim().is_empty() && !rest.trim().is_empty() => {
                (Some(glob.trim().to_owned()), rest.trim())
            }
            _ => (None, text),
        };
        checks.push(RepoCheck {
            id: format!("repo:{}", checks.len() + 1),
            text: text.to_owned(),
            glob,
        });
    }
    checks
}

/// The `AGENTS.md` / `CLAUDE.md` files under `root`, sorted by path and capped
/// at [`MAX_INSTRUCTION_FILES`], each with its imports expanded.
///
/// The walk honors `.gitignore` (even outside a git repository) and skips
/// hidden directories; `.github/REVIEW.md` and `.claude/skills/` are read
/// directly, so nothing needed lives where the walk does not look.
fn load_instructions(root: &Path) -> Vec<InstructionFile> {
    let mut found: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .build()
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::debug!("repo context: {error}");
                None
            }
        })
        .filter(|entry| {
            entry.file_type().is_some_and(|kind| kind.is_file())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| INSTRUCTION_NAMES.contains(&name))
        })
        .map(|entry| entry.into_path())
        .collect();
    found.sort();
    found.truncate(MAX_INSTRUCTION_FILES);

    let mut files = Vec::new();
    for path in found {
        let Some(raw) = read_to_string(&path) else {
            continue;
        };
        let path = relative(root, &path);
        let dir = match path.rsplit_once('/') {
            Some((dir, _)) => dir.to_owned(),
            None => String::new(),
        };
        files.push(InstructionFile {
            dir,
            content: expand_imports(&raw, &root.join(&path), root),
            path,
        });
    }
    files
}

/// Replace every line that is exactly `@path.md` (after trimming) with that
/// file's contents. Imports go one level deep — inserted content is not
/// re-scanned — and stay inside the repo; a line whose target escapes the
/// root or cannot be read is kept verbatim.
fn expand_imports(content: &str, file: &Path, root: &Path) -> String {
    let dir = file.parent().unwrap_or(root);
    let mut out = String::new();
    for line in content.lines() {
        let token = line.trim();
        let imported = if token.len() > 1 && token.starts_with('@') && token.ends_with(".md") {
            confine(root, dir, &token[1..]).and_then(|target| read_to_string(&target))
        } else {
            None
        };
        match imported {
            Some(text) => {
                out.push_str(text.trim_end_matches('\n'));
                out.push('\n');
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Resolve `target` against `dir` lexically — `..` is text here, not the
/// filesystem — and keep the result only when it stays inside `root`.
fn confine(root: &Path, dir: &Path, target: &str) -> Option<PathBuf> {
    let joined = dir.join(target);
    let mut parts: Vec<Component<'_>> = Vec::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_) | Component::CurDir) => {
                    parts.pop();
                }
                _ => parts.push(part),
            },
            part => parts.push(part),
        }
    }
    let resolved: PathBuf = parts.iter().fold(PathBuf::new(), |mut path, part| {
        path.push(part.as_os_str());
        path
    });
    resolved.starts_with(root).then_some(resolved)
}

/// The `.claude/skills/*/SKILL.md` entries, sorted by directory, with the
/// frontmatter `name` and `description` (the folder name is the fallback
/// name).
fn load_skills(root: &Path) -> Vec<SkillInfo> {
    let skills_dir = root.join(".claude").join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        if skills_dir.exists() {
            tracing::debug!(path = %skills_dir.display(), "repo context: cannot read skills directory");
        }
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::debug!("repo context: {error}");
                None
            }
        })
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    let mut skills = Vec::new();
    for dir in dirs {
        let Some(raw) = read_to_string(&dir.join("SKILL.md")) else {
            continue;
        };
        let (keys, _) = split_frontmatter(&raw);
        let fallback = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        skills.push(SkillInfo {
            name: keys
                .get("name")
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or(fallback),
            description: keys.get("description").cloned().unwrap_or_default(),
            dir: relative(root, &dir),
        });
    }
    skills
}

/// Split a leading `---` frontmatter block into its `name` / `description`
/// keys and the body after the closing `---`. Not a YAML parser: plain
/// `key: value` lines only, which is all the spec needs (spec §6.6). Without
/// a closing delimiter the whole input is the body.
pub(crate) fn split_frontmatter(content: &str) -> (BTreeMap<String, String>, String) {
    let mut lines = content.lines().peekable();
    if lines.peek().is_none_or(|line| line.trim() != "---") {
        return (BTreeMap::new(), content.to_owned());
    }
    lines.next();
    let mut keys = BTreeMap::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if key == "name" || key == "description" {
                keys.insert(key.to_owned(), unquote(value.trim()));
            }
        }
    }
    if !closed {
        return (BTreeMap::new(), content.to_owned());
    }
    (keys, lines.collect::<Vec<_>>().join("\n"))
}

/// Strip one layer of matching surrounding quotes from a frontmatter value.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    let quoted = bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''));
    if quoted {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    /// Write `content` to `rel` under `root`, creating parent directories.
    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn review_md_checks_parse_with_optional_globs() {
        let dir = tempdir();
        write(
            dir.path(),
            ".github/REVIEW.md",
            "Free-form guidance.\n\n## Checks\n- Use X\n- [assets/**/*.png] ≤ 200 KB\n\n## Other\n- not a check\n",
        );
        let ctx = load_repo_context(dir.path());
        assert!(ctx.review_md.is_some());
        assert_eq!(
            ctx.checks,
            [
                RepoCheck {
                    id: "repo:1".to_owned(),
                    text: "Use X".to_owned(),
                    glob: None,
                },
                RepoCheck {
                    id: "repo:2".to_owned(),
                    text: "≤ 200 KB".to_owned(),
                    glob: Some("assets/**/*.png".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn context_parses_this_repos_review_md() {
        // This repo ships its own `.github/REVIEW.md` (spec §17 call 11);
        // it must stay parseable by the checks grammar above.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/REVIEW.md");
        let markdown = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let checks = parse_checks(&markdown);
        assert!(
            checks.len() >= 8,
            "expected ≥ 8 checks, got {}: {:?}",
            checks.len(),
            checks.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        let globs: Vec<&str> = checks.iter().filter_map(|c| c.glob.as_deref()).collect();
        assert_eq!(globs, ["src/ai/**", "src/diff/**"]);
    }

    #[test]
    fn only_a_level_two_heading_closes_the_checks_section() {
        let checks = parse_checks("## Checks\n- A\n### Bugs\n- B\n## Other\n- C\n");
        let texts: Vec<&str> = checks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["A", "B"]);
    }

    #[test]
    fn nested_agents_md_scopes_to_paths() {
        let dir = tempdir();
        write(dir.path(), "AGENTS.md", "root rules\n");
        write(dir.path(), "src/api/AGENTS.md", "api rules\n");
        let ctx = load_repo_context(dir.path());
        assert_eq!(ctx.instructions.len(), 2);
        assert_eq!(ctx.instructions[0].dir, "");
        assert_eq!(ctx.instructions[0].path, "AGENTS.md");
        assert_eq!(ctx.instructions[1].dir, "src/api");
        assert_eq!(ctx.instructions[1].path, "src/api/AGENTS.md");

        let for_api = ctx.instructions_for(&["src/api/x.rs"]);
        assert_eq!(for_api.len(), 2, "{:?}", paths_of(&for_api));
        let for_web = ctx.instructions_for(&["web/y.ts"]);
        assert_eq!(paths_of(&for_web), ["AGENTS.md"]);

        // A sibling directory whose name merely starts with the scoped dir
        // does not match.
        let for_other = ctx.instructions_for(&["src/apifuzz/x.rs"]);
        assert_eq!(paths_of(&for_other), ["AGENTS.md"]);
    }

    fn paths_of<'a>(files: &[&'a InstructionFile]) -> Vec<&'a str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn claude_md_import_is_expanded_once_and_confined() {
        let dir = tempdir();
        write(
            dir.path(),
            "CLAUDE.md",
            "main rules\n@AGENTS.md\n@../../etc/passwd.md\n",
        );
        write(dir.path(), "AGENTS.md", "root rules\n@nested.md\n");
        write(dir.path(), "nested.md", "nested rules\n");
        let ctx = load_repo_context(dir.path());

        let claude = content_of(&ctx, "CLAUDE.md");
        assert!(claude.contains("root rules"), "{claude}");
        assert!(!claude.contains("@AGENTS.md"), "{claude}");
        // Outside the repo: kept verbatim.
        assert!(claude.contains("@../../etc/passwd.md"), "{claude}");
        // One level only: AGENTS.md's own import arrives as a literal line.
        assert!(claude.contains("@nested.md"), "{claude}");
        assert!(!claude.contains("nested rules"), "{claude}");

        // AGENTS.md itself does expand its own import.
        let agents = content_of(&ctx, "AGENTS.md");
        assert!(agents.contains("nested rules"), "{agents}");
    }

    #[test]
    fn second_level_import_is_not_expanded() {
        let dir = tempdir();
        write(dir.path(), "CLAUDE.md", "@a.md\n");
        write(dir.path(), "a.md", "a body\n@b.md\n");
        write(dir.path(), "b.md", "b body\n@c.md\n");
        write(dir.path(), "c.md", "c body\n");
        let ctx = load_repo_context(dir.path());

        let claude = content_of(&ctx, "CLAUDE.md");
        assert!(claude.contains("a body"), "{claude}");
        // b.md is second level for CLAUDE.md: its line stays literal and
        // neither it nor c.md is pulled in.
        assert!(claude.contains("@b.md"), "{claude}");
        assert!(!claude.contains("b body"), "{claude}");
        assert!(!claude.contains("c body"), "{claude}");
    }

    #[test]
    fn walk_skips_gitignored_agents_md() {
        let dir = tempdir();
        write(dir.path(), ".gitignore", "generated/\n");
        write(dir.path(), "AGENTS.md", "root rules\n");
        write(dir.path(), "generated/AGENTS.md", "generated rules\n");
        let ctx = load_repo_context(dir.path());
        let paths: Vec<&str> = ctx.instructions.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["AGENTS.md"]);
    }

    #[test]
    fn skills_index_reads_frontmatter() {
        let dir = tempdir();
        write(
            dir.path(),
            ".claude/skills/pdf/SKILL.md",
            "---\nname: pdf\ndescription: Do things with PDFs.\n---\nBody.\n",
        );
        write(
            dir.path(),
            ".claude/skills/plain/SKILL.md",
            "No frontmatter here.\n",
        );
        write(dir.path(), ".claude/skills/notes.txt", "not a skill\n");
        let ctx = load_repo_context(dir.path());

        assert_eq!(ctx.skills.len(), 2, "{:?}", ctx.skills);
        assert_eq!(ctx.skills[0].name, "pdf");
        assert_eq!(ctx.skills[0].description, "Do things with PDFs.");
        assert_eq!(ctx.skills[0].dir, ".claude/skills/pdf");
        // No frontmatter: the folder name is the fallback name.
        assert_eq!(ctx.skills[1].name, "plain");
        assert_eq!(ctx.skills[1].description, "");

        let index = ctx.skill_index();
        let lines: Vec<&str> = index.lines().collect();
        assert_eq!(lines[0], "- pdf: Do things with PDFs.");
        assert!(lines[1].starts_with("- plain:"), "{}", lines[1]);
    }

    fn content_of(ctx: &RepoContext, path: &str) -> String {
        ctx.instructions
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| {
                panic!(
                    "no {path}: {:?}",
                    paths_of(&ctx.instructions.iter().collect::<Vec<_>>())
                )
            })
            .content
            .clone()
    }
}
