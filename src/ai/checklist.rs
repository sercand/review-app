//! The built-in review checklist (spec §5.3) and its per-file-kind assignment.

use std::collections::HashSet;

use globset::GlobBuilder;

use super::context::RepoCheck;

/// What a changed file is, for checklist purposes (spec §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Source,
    Test,
    Asset,
    Config,
    Generated,
}

/// One checklist item: an id, a short title, and the instruction the agent
/// acts on (spec §5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecklistItem {
    pub id: String,
    pub title: String,
    pub instruction: String,
}

/// Lockfile names, matched as the last path component anywhere in the repo.
const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "go.sum",
    "Gemfile.lock",
    "poetry.lock",
];

/// Image, font, and media extensions that make a file an asset even when the
/// diff pipeline does not flag it binary. `svg` is text but still an image.
const ASSET_EXTENSIONS: &[&str] = &[
    // images
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "tiff", "tif", "heic", "avif",
    // fonts
    "woff", "woff2", "ttf", "otf", "eot", // media
    "mp3", "mp4", "mov", "avi", "wav", "ogg", "webm", "m4a", "flac", "aac",
];

/// The last path component of a repo-relative, forward-slash path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The lowercase extension of the file name, if it has a real one: `None` for
/// `Makefile`, and for dotfiles like `.env` whose only dot starts the name.
fn extension(path: &str) -> Option<String> {
    let name = file_name(path);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Generated, lock, and vendored files (spec §5.3): lockfiles by name,
/// `*.min.*`, `*.pb.*`, `*_generated.*` by file name, `vendor/**` and
/// `dist/**` anchored at the repo root.
fn is_generated(path: &str) -> bool {
    let name = file_name(path);
    LOCKFILES.contains(&name)
        || path.starts_with("vendor/")
        || path.starts_with("dist/")
        || name.contains(".min.")
        || name.contains(".pb.")
        || name.contains("_generated.")
}

/// Test files (spec §5.3): `tests/**`, `test/**`, `**/__tests__/**`,
/// `*_test.*`, `*.test.*`, `*.spec.*`, and Python's `test_*.py`.
fn is_test(path: &str) -> bool {
    let name = file_name(path);
    path.starts_with("tests/")
        || path.starts_with("test/")
        || path.starts_with("__tests__/")
        || path.contains("/__tests__/")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || (name.starts_with("test_") && name.ends_with(".py"))
}

/// Files with an image, font, or media extension.
fn is_asset_extension(path: &str) -> bool {
    extension(path).is_some_and(|ext| ASSET_EXTENSIONS.contains(&ext.as_str()))
}

/// Config and CI files (spec §5.3): `.github/**`, `*.toml`, `*.yaml`,
/// `*.yml`, `*.json` (lockfiles were already classified Generated),
/// `Dockerfile*`, `Makefile`, `*.ini`, `.env*`.
fn is_config(path: &str) -> bool {
    let name = file_name(path);
    path.starts_with(".github/")
        || name.starts_with("Dockerfile")
        || name == "Makefile"
        || name.starts_with(".env")
        || matches!(
            extension(path).as_deref(),
            Some("toml" | "yaml" | "yml" | "json" | "ini")
        )
}

/// Classify a changed file. `path` is repo-relative with forward slashes.
///
/// Precedence is Generated → Test → Asset → Config → Source, with one
/// refinement: `is_binary` forces Asset only when the path is neither
/// Generated nor Test. A binary lockfile-like path stays Generated, and a
/// binary file under `tests/` stays Test — the kind drives checklist
/// assignment, not the file's size.
pub fn file_kind(path: &str, is_binary: bool) -> FileKind {
    if is_generated(path) {
        FileKind::Generated
    } else if is_test(path) {
        FileKind::Test
    } else if is_binary || is_asset_extension(path) {
        FileKind::Asset
    } else if is_config(path) {
        FileKind::Config
    } else {
        FileKind::Source
    }
}

/// The built-in checklist, in the spec §5.3 table order. Ids, titles, and
/// instructions match the table verbatim (including its markdown emphasis).
pub fn builtin_items() -> Vec<ChecklistItem> {
    [
        (
            "intent",
            "Matches the PR's intent",
            "Compare the change with the brief's intent; flag unexplained or unrelated changes.",
        ),
        (
            "correctness",
            "Logic and edge cases",
            "Read the whole changed function (not just the hunk); empty/null/error paths, off-by-one, ordering, concurrency.",
        ),
        (
            "callers",
            "Callers and contracts",
            "For every changed signature, exported symbol, or behavior: grep for call sites and open them. Flag callers that weren't updated and changed contracts.",
        ),
        (
            "consistency",
            "Fits its neighbors",
            "Open 1–3 sibling files in the same directory, or the closest analog elsewhere. Compare naming, file/function structure, error handling, logging, and patterns. Flag divergence.",
        ),
        (
            "duplication",
            "Doesn't duplicate existing code",
            "Grep for similar names and key identifiers or literals; look for existing helpers/utilities that already do this. Flag reimplementations.",
        ),
        (
            "tests",
            "Tested",
            "Is new or changed behavior covered by a test in this PR or an existing one? Were tests deleted or weakened?",
        ),
        (
            "api_surface",
            "Public surface and config",
            "Public APIs, schemas, migrations, config keys, env vars, docs: changed consistently and compatibly?",
        ),
        (
            "security",
            "Security basics",
            "Untrusted input, injection, authz checks, secrets in code or logs.",
        ),
        (
            "performance",
            "Obvious performance traps",
            "Work in loops (I/O, queries, allocations), unbounded growth, N+1.",
        ),
        (
            "assets",
            "Binary/asset sanity",
            "For images and other binaries: compare size, dimensions, and format with the siblings in the same folder (via `asset_stats`, §6.3).",
        ),
        (
            "ci",
            "CI results",
            "Explain failed checks that belong to this group; don't repeat linter output. Only report something CI *didn't* catch, or explain the root cause of a failure.",
        ),
    ]
    .into_iter()
    .map(|(id, title, instruction)| ChecklistItem {
        id: id.to_owned(),
        title: title.to_owned(),
        instruction: instruction.to_owned(),
    })
    .collect()
}

/// The checklist ids every group of this kind gets, regardless of what the
/// scout says (spec §5.3 "Default assignment by file kind").
pub fn default_items(kind: FileKind) -> &'static [&'static str] {
    match kind {
        // All built-ins except `assets`.
        FileKind::Source => &[
            "intent",
            "correctness",
            "callers",
            "consistency",
            "duplication",
            "tests",
            "api_surface",
            "security",
            "performance",
            "ci",
        ],
        FileKind::Test => &["intent", "correctness", "consistency"],
        FileKind::Asset => &["assets", "consistency"],
        FileKind::Config => &["intent", "api_surface", "security"],
        FileKind::Generated => &["intent"],
    }
}

/// Whether a repo check applies to `path`: unscoped checks apply everywhere;
/// a scoped one matches its glob against the forward-slash path. A glob that
/// fails to compile matches nothing (never panics).
fn check_applies(glob: Option<&str>, path: &str) -> bool {
    let Some(pattern) = glob else {
        return true;
    };
    // `literal_separator(false)` so `assets/**/*.png` also matches
    // `assets/x.png`; repo globs are not directory-strict.
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .is_ok_and(|matcher| matcher.is_match(path))
}

fn push_unique(ids: &mut Vec<String>, seen: &mut HashSet<String>, id: &str) {
    if seen.insert(id.to_owned()) {
        ids.push(id.to_owned());
    }
}

/// The checklist ids for one file: the kind's defaults, plus repo checks from
/// `.github/REVIEW.md` whose glob matches (unscoped = all files), plus the
/// scout's extras. Deduplicated; unknown ids pass through.
///
/// Order: builtin order first (the defaults are already in it), then repo
/// checks in input order, then scout extras in input order. Extras are not
/// validated against the built-ins — the scout may add a repo-specific id,
/// and the Plan 3 renderer shows unknown ids' text from the extras list.
pub fn assign(
    path: &str,
    kind: FileKind,
    repo_checks: &[RepoCheck],
    scout_extra: &[String],
) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for id in default_items(kind) {
        push_unique(&mut ids, &mut seen, id);
    }
    for check in repo_checks {
        if check_applies(check.glob.as_deref(), path) {
            push_unique(&mut ids, &mut seen, &check.id);
        }
    }
    for id in scout_extra {
        push_unique(&mut ids, &mut seen, id);
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(id: &str, glob: Option<&str>) -> RepoCheck {
        RepoCheck {
            id: id.to_string(),
            text: format!("check {id}"),
            glob: glob.map(str::to_string),
        }
    }

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn file_kind_table() {
        let cases: &[(&str, bool, FileKind)] = &[
            // Generated: every lockfile
            ("Cargo.lock", false, FileKind::Generated),
            ("rust/sub/Cargo.lock", false, FileKind::Generated),
            ("package-lock.json", false, FileKind::Generated),
            ("web/yarn.lock", false, FileKind::Generated),
            ("pnpm-lock.yaml", false, FileKind::Generated),
            ("go.sum", false, FileKind::Generated),
            ("Gemfile.lock", false, FileKind::Generated),
            ("poetry.lock", false, FileKind::Generated),
            // Generated: minified, protobuf, generated, vendored, dist
            ("assets/js/app.min.js", false, FileKind::Generated),
            ("gen/user.pb.go", false, FileKind::Generated),
            ("src/models_generated.rs", false, FileKind::Generated),
            ("vendor/github.com/x/y.go", false, FileKind::Generated),
            ("dist/bundle.js", false, FileKind::Generated),
            // Test: every pattern
            ("tests/routing.rs", false, FileKind::Test),
            ("test/helpers.py", false, FileKind::Test),
            ("web/src/__tests__/button.jsx", false, FileKind::Test),
            ("src/main_test.go", false, FileKind::Test),
            ("ui/Button.test.tsx", false, FileKind::Test),
            ("ui/Button.spec.ts", false, FileKind::Test),
            ("tests/test_utils.py", false, FileKind::Test),
            // Asset: by extension (image, font, media) and by binary flag
            ("assets/logo.png", false, FileKind::Asset),
            ("fonts/Inter.woff2", false, FileKind::Asset),
            ("media/intro.mp4", false, FileKind::Asset),
            ("data/blob.dat", true, FileKind::Asset),
            // Config: every pattern
            (".github/workflows/ci.yml", false, FileKind::Config),
            ("Cargo.toml", false, FileKind::Config),
            ("docker-compose.yaml", false, FileKind::Config),
            ("config/app.yml", false, FileKind::Config),
            ("package.json", false, FileKind::Config),
            ("Dockerfile", false, FileKind::Config),
            ("docker/Dockerfile.prod", false, FileKind::Config),
            ("Makefile", false, FileKind::Config),
            ("settings.ini", false, FileKind::Config),
            (".env", false, FileKind::Config),
            (".env.production", false, FileKind::Config),
            // Source: everything else
            ("src/main.rs", false, FileKind::Source),
            ("api/users.ts", false, FileKind::Source),
            // Precedence collisions
            ("dist/generated.min.js", false, FileKind::Generated),
            ("tests/fixtures/logo.png", false, FileKind::Test),
            ("vendor/pkg/test_x.py", false, FileKind::Generated),
            ("Cargo.lock", true, FileKind::Generated),
            ("tests/data.bin", true, FileKind::Test),
            ("config/app.json", true, FileKind::Asset),
        ];
        for (path, is_binary, want) in cases {
            assert_eq!(
                file_kind(path, *is_binary),
                *want,
                "file_kind({path:?}, {is_binary})"
            );
        }
    }

    #[test]
    fn builtin_ids_match_spec() {
        let items = builtin_items();
        let got: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(
            got,
            vec![
                "intent",
                "correctness",
                "callers",
                "consistency",
                "duplication",
                "tests",
                "api_surface",
                "security",
                "performance",
                "assets",
                "ci",
            ]
        );
        for item in &items {
            assert!(!item.title.is_empty(), "{} has no title", item.id);
            assert!(
                !item.instruction.is_empty(),
                "{} has no instruction",
                item.id
            );
        }
    }

    #[test]
    fn default_items_by_kind() {
        assert_eq!(
            default_items(FileKind::Source),
            &[
                "intent",
                "correctness",
                "callers",
                "consistency",
                "duplication",
                "tests",
                "api_surface",
                "security",
                "performance",
                "ci",
            ]
        );
        assert_eq!(
            default_items(FileKind::Test),
            &["intent", "correctness", "consistency"]
        );
        assert_eq!(default_items(FileKind::Asset), &["assets", "consistency"]);
        assert_eq!(
            default_items(FileKind::Config),
            &["intent", "api_surface", "security"]
        );
        assert_eq!(default_items(FileKind::Generated), &["intent"]);
    }

    #[test]
    fn assign_merges_defaults_repo_checks_and_extras() {
        let checks = vec![
            check("repo:1", None),
            check("repo:2", Some("assets/**/*.png")),
            check("repo:3", Some("**/*.py")),
        ];
        let extras = ids(&["custom:changelog", "intent"]);
        assert_eq!(
            assign("assets/icons/big.png", FileKind::Source, &checks, &extras),
            ids(&[
                // Source defaults, in builtin order
                "intent",
                "correctness",
                "callers",
                "consistency",
                "duplication",
                "tests",
                "api_surface",
                "security",
                "performance",
                "ci",
                // matching repo checks, in input order
                "repo:1",
                "repo:2",
                // extras last, in input order; the duplicate "intent" is dropped
                "custom:changelog",
            ])
        );
        // A scoped check only applies where its glob matches.
        let got = assign("src/main.rs", FileKind::Source, &checks, &[]);
        assert!(got.contains(&"repo:1".to_string()));
        assert!(!got.contains(&"repo:2".to_string()));
        assert!(!got.contains(&"repo:3".to_string()));
        assert!(
            assign("scripts/x.py", FileKind::Source, &checks, &[]).contains(&"repo:3".to_string())
        );
    }

    #[test]
    fn assign_glob_details() {
        // literal_separator(false): `assets/**/*.png` also matches `assets/x.png`.
        let checks = vec![check("repo:1", Some("assets/**/*.png"))];
        assert!(
            assign("assets/x.png", FileKind::Asset, &checks, &[]).contains(&"repo:1".to_string())
        );
        assert!(
            !assign("assets/icons/x.svg", FileKind::Asset, &checks, &[])
                .contains(&"repo:1".to_string())
        );
        // A glob that fails to compile matches nothing; it never panics.
        let broken = vec![check("repo:1", Some("assets/[0-"))];
        assert!(
            !assign("assets/x.png", FileKind::Asset, &broken, &[]).contains(&"repo:1".to_string())
        );
    }

    #[test]
    fn scout_cannot_remove_defaults() {
        // Extras only ever add: defaults are present whatever the scout says.
        let got = assign("assets/logo.png", FileKind::Asset, &[], &ids(&["repo:9"]));
        assert_eq!(&got[..2], ["assets", "consistency"]);
        assert!(got.contains(&"repo:9".to_string()));
        let got = assign("src/main.rs", FileKind::Source, &[], &[]);
        assert_eq!(got, ids(default_items(FileKind::Source)));
    }
}
