//! SQLite persistence for AI review data (spec §11).
//!
//! One database at `<config>/ai.sqlite` holds AI sessions, runs, transcripts,
//! findings, coverage, Q&A, and checkout records for crash recovery (§7.3).
//! The schema is created by migration 1, tracked with `PRAGMA user_version`;
//! opening an already-migrated database is a no-op. No secrets are ever
//! stored here (spec §11).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::Context;
use rusqlite::Connection;

/// Migration 1: creates every table from spec §11 so later milestones need
/// no schema changes. `_json` columns are TEXT; all ids are
/// `INTEGER PRIMARY KEY`.
const SCHEMA_V1: &str = r#"
CREATE TABLE ai_sessions (
    id                INTEGER PRIMARY KEY,
    host              TEXT NOT NULL,
    owner             TEXT NOT NULL,
    repo              TEXT NOT NULL,
    number            INTEGER NOT NULL,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_reviewed_sha TEXT,
    brief_json        TEXT
);
CREATE UNIQUE INDEX ai_sessions_pr ON ai_sessions (host, owner, repo, number);

CREATE TABLE ai_runs (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES ai_sessions (id),
    kind        TEXT NOT NULL CHECK (kind IN ('full', 'incremental')),
    base_sha    TEXT NOT NULL,
    head_sha    TEXT NOT NULL,
    status      TEXT NOT NULL,
    started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at    TEXT,
    usage_json  TEXT,
    stop_reason TEXT
);

CREATE TABLE ai_messages (
    id           INTEGER PRIMARY KEY,
    run_id       INTEGER NOT NULL REFERENCES ai_runs (id),
    agent_id     TEXT NOT NULL,
    role         TEXT NOT NULL,
    seq          INTEGER NOT NULL,
    content_json TEXT NOT NULL
);

CREATE TABLE ai_findings (
    id               INTEGER PRIMARY KEY,
    session_id       INTEGER NOT NULL REFERENCES ai_sessions (id),
    run_id           INTEGER NOT NULL REFERENCES ai_runs (id),
    path             TEXT NOT NULL,
    anchor_json      TEXT,
    severity         TEXT NOT NULL,
    title            TEXT NOT NULL,
    body             TEXT NOT NULL,
    evidence_json    TEXT,
    checklist_item   TEXT,
    state            TEXT NOT NULL,
    state_reason     TEXT,
    pending_local_id TEXT
);

CREATE TABLE ai_coverage (
    id     INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES ai_runs (id),
    file   TEXT NOT NULL,
    item   TEXT NOT NULL,
    result TEXT NOT NULL,
    note   TEXT
);

CREATE TABLE ai_qa (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES ai_sessions (id),
    question   TEXT NOT NULL,
    answer     TEXT,
    asked_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE ai_checkout (
    id           INTEGER PRIMARY KEY,
    repo         TEXT NOT NULL,
    repo_path    TEXT NOT NULL,
    pr_number    INTEGER NOT NULL,
    original_ref TEXT NOT NULL,
    stash_sha    TEXT,
    head_sha     TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('started', 'active', 'restored', 'failed'))
);
"#;

/// Where the AI database lives under a config root: `<config>/ai.sqlite`.
pub fn ai_db_path(config_root: &Path) -> PathBuf {
    config_root.join("ai.sqlite")
}

/// Identifies the PR an AI session belongs to. `host`, `owner`, and `repo`
/// are stored (and therefore matched) lowercased, so the unique index on
/// `ai_sessions (host, owner, repo, number)` is case-insensitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrKey {
    /// GitHub host, e.g. `github.com`. Stored lowercased.
    pub host: String,
    /// Repository owner. Stored lowercased.
    pub owner: String,
    /// Repository name. Stored lowercased.
    pub repo: String,
    /// Pull request number.
    pub number: u64,
}

/// The ref the working tree was on before the PR head was checked out,
/// recorded so a crash can be undone (§7.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OriginalRef {
    /// A local branch (its short name; slashes and colons allowed).
    Branch(String),
    /// A detached HEAD at the given commit SHA.
    Detached(String),
}

impl OriginalRef {
    /// Text encoding for storage: `branch:<name>` or `detached:<sha>`. The
    /// tag is split at the first `:` on decode, so branch names may contain
    /// slashes and colons.
    fn encode(&self) -> String {
        match self {
            OriginalRef::Branch(name) => format!("branch:{name}"),
            OriginalRef::Detached(sha) => format!("detached:{sha}"),
        }
    }

    /// Inverse of [`OriginalRef::encode`].
    fn decode(raw: &str) -> anyhow::Result<OriginalRef> {
        match raw.split_once(':') {
            Some(("branch", rest)) => Ok(OriginalRef::Branch(rest.to_string())),
            Some(("detached", rest)) => Ok(OriginalRef::Detached(rest.to_string())),
            _ => anyhow::bail!("unrecognised original_ref {raw:?}"),
        }
    }
}

/// Lifecycle of a PR checkout, stored as the lowercase text of each variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckoutState {
    /// Checkout started; restore has not run yet.
    Started,
    /// The PR head is checked out and in use.
    Active,
    /// The original ref (and stash, if any) was restored.
    Restored,
    /// Restore failed; the tree may still be on the PR head.
    Failed,
}

impl CheckoutState {
    /// The text stored in the `state` column.
    fn as_str(&self) -> &'static str {
        match self {
            CheckoutState::Started => "started",
            CheckoutState::Active => "active",
            CheckoutState::Restored => "restored",
            CheckoutState::Failed => "failed",
        }
    }

    /// Inverse of [`CheckoutState::as_str`].
    fn parse(raw: &str) -> anyhow::Result<CheckoutState> {
        match raw {
            "started" => Ok(CheckoutState::Started),
            "active" => Ok(CheckoutState::Active),
            "restored" => Ok(CheckoutState::Restored),
            "failed" => Ok(CheckoutState::Failed),
            _ => anyhow::bail!("unrecognised checkout state {raw:?}"),
        }
    }
}

/// One row of `ai_checkout`: a PR checkout awaiting (or having had) its
/// restore. `repo_path` is stored exactly as passed to
/// [`Store::checkout_begin`]; callers must query with the same path (Task 11
/// keeps one `PathBuf` per repo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutRecord {
    /// Database row id. Ignored by [`Store::checkout_begin`], which assigns
    /// a fresh one.
    pub id: i64,
    /// Absolute path of the local clone.
    pub repo_path: PathBuf,
    /// `owner/repo` slug.
    pub repo: String,
    /// Pull request number that was checked out.
    pub pr_number: u64,
    /// What the working tree was on before the checkout.
    pub original: OriginalRef,
    /// Stash created before the checkout, if any.
    pub stash_sha: Option<String>,
    /// The PR head SHA that was checked out.
    pub head_sha: String,
    /// Current lifecycle state.
    pub state: CheckoutState,
}

/// The `ai_checkout` columns behind a [`CheckoutRecord`]'s fields, in struct
/// order; queries prefix these with `id`.
const CHECKOUT_COLUMNS: &str =
    "repo_path, repo, pr_number, original_ref, stash_sha, head_sha, state";

/// Raw (still encoded) form of a [`CheckoutRecord`] minus its id. PR numbers
/// travel as `i64` (SQLite has no unsigned class) and are cast back on decode.
type RawRecord = (String, String, i64, String, Option<String>, String, String);

/// Read the columns after `id` from a row selected with `CHECKOUT_COLUMNS`.
fn raw_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecord> {
    Ok((
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

/// Decode a [`RawRecord`] into a [`CheckoutRecord`].
fn decode_record(id: i64, raw: RawRecord) -> anyhow::Result<CheckoutRecord> {
    let (repo_path, repo, pr_number, original, stash_sha, head_sha, state) = raw;
    Ok(CheckoutRecord {
        id,
        repo_path: repo_path.into(),
        repo,
        pr_number: pr_number.max(0) as u64,
        original: OriginalRef::decode(&original)?,
        stash_sha,
        head_sha,
        state: CheckoutState::parse(&state)?,
    })
}

/// SQLite-backed store for all AI review data (spec §11). Shareable across
/// threads: every operation takes the internal connection lock, so `Store`
/// is `Send + Sync`.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) the database at `path`: parent directories
    /// are created, WAL and foreign keys are enabled, and migrations run.
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::configure(&mut conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Open a private in-memory database, fully configured and migrated.
    pub fn open_in_memory() -> anyhow::Result<Store> {
        let mut conn = Connection::open_in_memory().context("opening in-memory database")?;
        Self::configure(&mut conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn configure(conn: &mut Connection) -> anyhow::Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(conn)?;
        Ok(())
    }

    /// Take the connection lock, tolerating a poisoned mutex (a panicking
    /// thread cannot leave SQLite itself inconsistent).
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Current schema version (`PRAGMA user_version`); 1 after migration 1.
    pub fn user_version(&self) -> anyhow::Result<i64> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Get-or-create the `ai_sessions` row for `key` and return its id.
    pub fn session_id(&self, key: &PrKey) -> anyhow::Result<i64> {
        self.conn()
            .query_row(
                "INSERT INTO ai_sessions (host, owner, repo, number)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (host, owner, repo, number) DO UPDATE SET id = id
                 RETURNING id",
                rusqlite::params![
                    key.host.to_lowercase(),
                    key.owner.to_lowercase(),
                    key.repo.to_lowercase(),
                    key.number as i64
                ],
                |row| row.get(0),
            )
            .context("get-or-create ai_sessions row")
    }

    /// Insert a new `ai_checkout` row from `rec` (its `id` is ignored) and
    /// return the assigned id.
    pub fn checkout_begin(&self, rec: &CheckoutRecord) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ai_checkout (repo_path, repo, pr_number, original_ref, stash_sha, head_sha, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                rec.repo_path.to_string_lossy(),
                rec.repo,
                rec.pr_number as i64,
                rec.original.encode(),
                rec.stash_sha,
                rec.head_sha,
                rec.state.as_str()
            ],
        )
        .context("inserting ai_checkout row")?;
        Ok(conn.last_insert_rowid())
    }

    /// Record the stash SHA created during checkout of row `id`.
    pub fn checkout_set_stash(&self, id: i64, sha: &str) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_checkout SET stash_sha = ?2 WHERE id = ?1",
                rusqlite::params![id, sha],
            )
            .context("setting ai_checkout stash_sha")?;
        anyhow::ensure!(n == 1, "no ai_checkout row with id {id}");
        Ok(())
    }

    /// Move row `id` to a new lifecycle `state`.
    pub fn checkout_set_state(&self, id: i64, state: CheckoutState) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_checkout SET state = ?2 WHERE id = ?1",
                rusqlite::params![id, state.as_str()],
            )
            .context("setting ai_checkout state")?;
        anyhow::ensure!(n == 1, "no ai_checkout row with id {id}");
        Ok(())
    }

    /// The newest checkout of `repo_path` still in `Started` or `Active`
    /// state — the per-repo lock (§4 step 2). The path is compared exactly
    /// as stored; callers must pass the same `PathBuf` they began with.
    pub fn checkout_holding(&self, repo_path: &Path) -> anyhow::Result<Option<CheckoutRecord>> {
        let raw = self.conn().query_row(
            &format!(
                "SELECT id, {CHECKOUT_COLUMNS} FROM ai_checkout
                     WHERE repo_path = ?1 AND state IN ('started', 'active')
                     ORDER BY id DESC LIMIT 1"
            ),
            rusqlite::params![repo_path.to_string_lossy()],
            |row| Ok((row.get::<_, i64>(0)?, raw_from_row(row)?)),
        );
        match raw {
            Ok((id, raw)) => decode_record(id, raw).map(Some),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("querying holding checkout"),
        }
    }

    /// All checkouts still in `Started` or `Active` state, oldest first —
    /// the crash-recovery list (§7.3).
    pub fn checkouts_unrestored(&self) -> anyhow::Result<Vec<CheckoutRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, {CHECKOUT_COLUMNS} FROM ai_checkout
                 WHERE state IN ('started', 'active')
                 ORDER BY id"
            ))
            .context("preparing unrestored checkouts query")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, raw_from_row(row)?)))
            .context("querying unrestored checkouts")?;
        let mut out = Vec::new();
        for row in rows {
            let (id, raw) = row.context("reading unrestored checkout")?;
            out.push(decode_record(id, raw)?);
        }
        Ok(out)
    }
}

/// Bring `conn` up to the latest schema. Each migration runs in one
/// transaction together with its `user_version` bump, so a half-applied
/// migration never reports as done.
fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= 1 {
        return Ok(());
    }
    let tx = conn.transaction()?;
    tx.execute_batch(SCHEMA_V1)?;
    tx.pragma_update(None, "user_version", 1)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    fn record(path: &str, original: OriginalRef) -> CheckoutRecord {
        CheckoutRecord {
            id: 0,
            repo_path: PathBuf::from(path),
            repo: "owner/repo".to_string(),
            pr_number: 3,
            original,
            stash_sha: None,
            head_sha: "abc123".to_string(),
            state: CheckoutState::Started,
        }
    }

    #[test]
    fn session_id_is_stable_and_case_insensitive() {
        let store = Store::open_in_memory().unwrap();
        let upper = PrKey {
            host: "GitHub.com".to_string(),
            owner: "Owner".to_string(),
            repo: "Repo".to_string(),
            number: 7,
        };
        let lower = PrKey {
            host: "github.com".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 7,
        };
        let a = store.session_id(&upper).unwrap();
        let b = store.session_id(&lower).unwrap();
        assert_eq!(a, b, "host/owner/repo are matched case-insensitively");
        assert_eq!(
            store.session_id(&upper).unwrap(),
            a,
            "get-or-create is stable"
        );

        let other_pr = PrKey { number: 8, ..lower };
        assert_ne!(store.session_id(&other_pr).unwrap(), a);
    }

    #[test]
    fn checkout_lifecycle_and_repo_lock() {
        let store = Store::open_in_memory().unwrap();
        let path = PathBuf::from("/repos/thing");

        let id = store
            .checkout_begin(&record(
                "/repos/thing",
                OriginalRef::Branch("main".to_string()),
            ))
            .unwrap();
        assert!(id > 0);

        let held = store.checkout_holding(&path).unwrap().expect("holding");
        assert_eq!(held.id, id);
        assert_eq!(held.repo_path, path);
        assert_eq!(held.repo, "owner/repo");
        assert_eq!(held.pr_number, 3);
        assert_eq!(held.original, OriginalRef::Branch("main".to_string()));
        assert_eq!(held.head_sha, "abc123");
        assert_eq!(held.state, CheckoutState::Started);

        store.checkout_set_stash(id, "stash42").unwrap();
        store.checkout_set_state(id, CheckoutState::Active).unwrap();
        let held = store
            .checkout_holding(&path)
            .unwrap()
            .expect("still holding");
        assert_eq!(held.state, CheckoutState::Active);
        assert_eq!(held.stash_sha.as_deref(), Some("stash42"));

        store
            .checkout_set_state(id, CheckoutState::Restored)
            .unwrap();
        assert!(store.checkout_holding(&path).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
    }

    #[test]
    fn unrestored_survives_reopen() {
        let dir = tempdir();
        let db = ai_db_path(dir.path());
        assert_eq!(db, dir.path().join("ai.sqlite"));
        {
            let store = Store::open(&db).unwrap();
            store
                .checkout_begin(&record(
                    "/repos/thing",
                    OriginalRef::Detached("deadbeef".to_string()),
                ))
                .unwrap();
        }
        let store = Store::open(&db).unwrap();
        let unrestored = store.checkouts_unrestored().unwrap();
        assert_eq!(unrestored.len(), 1);
        assert_eq!(unrestored[0].state, CheckoutState::Started);
        assert_eq!(
            unrestored[0].original,
            OriginalRef::Detached("deadbeef".to_string())
        );
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempdir();
        let db = ai_db_path(dir.path());
        {
            let store = Store::open(&db).unwrap();
            assert_eq!(store.user_version().unwrap(), 1);
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store.user_version().unwrap(),
            1,
            "reopening must not re-migrate"
        );
        // The unique index from migration 1 survives the second open.
        let key = PrKey {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 1,
        };
        assert_eq!(
            store.session_id(&key).unwrap(),
            store.session_id(&key).unwrap()
        );
    }

    #[test]
    fn original_ref_round_trips_branch_with_slashes_and_detached() {
        let store = Store::open_in_memory().unwrap();

        let branch = OriginalRef::Branch("feature/nested/slash:colon".to_string());
        store
            .checkout_begin(&record("/repos/a", branch.clone()))
            .unwrap();
        let held = store
            .checkout_holding(&PathBuf::from("/repos/a"))
            .unwrap()
            .expect("holding");
        assert_eq!(held.original, branch);

        let detached = OriginalRef::Detached("0123456789abcdef".to_string());
        store
            .checkout_begin(&record("/repos/b", detached.clone()))
            .unwrap();
        let held = store
            .checkout_holding(&PathBuf::from("/repos/b"))
            .unwrap()
            .expect("holding");
        assert_eq!(held.original, detached);
    }
}
