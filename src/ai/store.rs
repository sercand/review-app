//! SQLite persistence for AI review data (spec §11).
//!
//! One database at `<config>/ai.sqlite` holds AI sessions, runs, transcripts,
//! findings, coverage, Q&A, and checkout records for crash recovery (§7.3).
//! The migrations, tracked with `PRAGMA user_version`, create the schema and
//! evolve it; opening an already-migrated database is a no-op. No secrets are
//! ever stored here (spec §11).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::Context;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::ai::review::{
    CheckResult, CoverageEntry, Evidence, Finding, FindingAnchor, FindingState, Severity,
};
use crate::ai::stages::assemble::ReviewDraft;
use crate::ai::tools::AgentRef;

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
    base_sha    TEXT,
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
    state        TEXT NOT NULL CHECK (state IN ('started', 'active', 'restored', 'failed', 'superseded'))
);
"#;

/// Migration 2: the assembler's draft travels with its run so the attach
/// snapshot can replay it — a run that finished while the user was on
/// another PR loses its `DraftReady` to the stale-event rule, and without
/// this column the pre-filled review would never happen (spec §5.6, §10.3).
const SCHEMA_V2: &str = r#"
ALTER TABLE ai_runs ADD COLUMN draft_json TEXT;
"#;

/// Where the AI database lives under a config root: `<config>/ai.sqlite`.
pub fn ai_db_path(config_root: &Path) -> PathBuf {
    config_root.join("ai.sqlite")
}

/// Identifies the PR an AI session belongs to. `host`, `owner`, and `repo`
/// are stored (and therefore matched) lowercased, so the unique index on
/// `ai_sessions (host, owner, repo, number)` is case-insensitive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
    /// Restore failed. The tree may still be on the PR head or hold
    /// uncommitted work, so recovery (§7.3) must call
    /// [`restore`](super::checkout::restore) with `discard = false` first:
    /// that returns [`NeedsDecision`](super::checkout::CheckoutError::NeedsDecision)
    /// instead of discarding stray changes.
    Failed,
    /// Terminal: this row was replaced by a newer row for the same PR, which
    /// inherited its original ref and stash. Never a lock holder and never a
    /// recovery target — restoring it too would apply the same stash twice
    /// and `reset --hard` over the work the replacement's restore applied.
    Superseded,
}

impl CheckoutState {
    /// The text stored in the `state` column.
    fn as_str(&self) -> &'static str {
        match self {
            CheckoutState::Started => "started",
            CheckoutState::Active => "active",
            CheckoutState::Restored => "restored",
            CheckoutState::Failed => "failed",
            CheckoutState::Superseded => "superseded",
        }
    }

    /// Inverse of [`CheckoutState::as_str`].
    fn parse(raw: &str) -> anyhow::Result<CheckoutState> {
        match raw {
            "started" => Ok(CheckoutState::Started),
            "active" => Ok(CheckoutState::Active),
            "restored" => Ok(CheckoutState::Restored),
            "failed" => Ok(CheckoutState::Failed),
            "superseded" => Ok(CheckoutState::Superseded),
            _ => anyhow::bail!("unrecognised checkout state {raw:?}"),
        }
    }
}

/// What a run covered. Stored as the lowercase text of each variant, which
/// is exactly what `ai_runs.kind`'s CHECK constraint allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    /// The whole PR diff.
    Full,
    /// Only the commits since `last_reviewed_sha` (spec §5.7).
    Incremental,
}

impl RunKind {
    /// The text stored in the `kind` column.
    fn as_str(&self) -> &'static str {
        match self {
            RunKind::Full => "full",
            RunKind::Incremental => "incremental",
        }
    }

    /// Inverse of [`RunKind::as_str`].
    pub fn parse(raw: &str) -> anyhow::Result<RunKind> {
        match raw {
            "full" => Ok(RunKind::Full),
            "incremental" => Ok(RunKind::Incremental),
            _ => anyhow::bail!("unrecognised run kind {raw:?}"),
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

/// One `ai_runs` row, as the UI needs it to show run status on attach
/// (spec §10.3). `kind` and `status` keep their stored spelling
/// ([`RunKind`] / [`crate::ai::events::RunStatus`] parse them back).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRecord {
    /// Database row id.
    pub id: i64,
    /// `"full"` or `"incremental"`.
    pub kind: String,
    /// For incremental runs: the head the previous run reviewed.
    pub base_sha: Option<String>,
    /// The head this run reviewed.
    pub head_sha: String,
    /// Lifecycle status (`completed|cancelled|failed|budget_exhausted|running`).
    pub status: String,
    /// Why the run stopped early, when it did.
    pub stop_reason: Option<String>,
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

    /// Current schema version (`PRAGMA user_version`); 2 after the migrations.
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

    /// The newest checkout of `repo_path` still in `Started`, `Active` or
    /// `Failed` state — the per-repo lock (§4 step 2). The path is compared
    /// exactly as stored; callers must pass the same `PathBuf` they began
    /// with.
    ///
    /// A `Failed` row may still hold the tree (a stash-push or restore
    /// failure leaves it on the PR head or with the user's changes in place),
    /// so callers must treat it as held. `Superseded` and `Restored` never
    /// hold.
    pub fn checkout_holding(&self, repo_path: &Path) -> anyhow::Result<Option<CheckoutRecord>> {
        let raw = self.conn().query_row(
            &format!(
                "SELECT id, {CHECKOUT_COLUMNS} FROM ai_checkout
                     WHERE repo_path = ?1 AND state IN ('started', 'active', 'failed')
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

    /// The checkout record with this id, when one exists. Used by the
    /// crash-recovery dialog's Restore button (spec §7.3).
    pub fn checkout_get(&self, id: i64) -> anyhow::Result<Option<CheckoutRecord>> {
        let raw = self.conn().query_row(
            &format!("SELECT id, {CHECKOUT_COLUMNS} FROM ai_checkout WHERE id = ?1"),
            rusqlite::params![id],
            raw_from_row,
        );
        match raw {
            Ok(raw) => decode_record(id, raw).map(Some),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("reading checkout by id"),
        }
    }

    /// All checkouts still in `Started`, `Active` or `Failed` state, oldest
    /// first — the crash-recovery list (§7.3).
    ///
    /// `Failed` is included because it is written exactly when recovery is
    /// needed. `Superseded` rows are excluded: a newer row for the same PR
    /// inherited their stash and original ref, so listing both would let a
    /// second restore discard the work the first one applied.
    pub fn checkouts_unrestored(&self) -> anyhow::Result<Vec<CheckoutRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, {CHECKOUT_COLUMNS} FROM ai_checkout
                 WHERE state IN ('started', 'active', 'failed')
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

    /// Insert an `ai_runs` row in status `running` and return its id.
    ///
    /// `base_sha` is `None` (stored NULL) for a first full run, which has no
    /// previous head to diff against.
    pub fn run_begin(
        &self,
        session_id: i64,
        kind: RunKind,
        base_sha: Option<&str>,
        head_sha: &str,
    ) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ai_runs (session_id, kind, base_sha, head_sha, status)
             VALUES (?1, ?2, ?3, ?4, 'running')",
            rusqlite::params![session_id, kind.as_str(), base_sha, head_sha],
        )
        .context("inserting ai_runs row")?;
        Ok(conn.last_insert_rowid())
    }

    /// Close run `id`: final `status`, `ended_at`, and the optional stop
    /// reason and usage JSON.
    pub fn run_end(
        &self,
        run_id: i64,
        status: &str,
        stop_reason: Option<&str>,
        usage_json: Option<&str>,
    ) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_runs
                 SET status = ?2,
                     ended_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                     stop_reason = ?3,
                     usage_json = ?4
                 WHERE id = ?1",
                rusqlite::params![run_id, status, stop_reason, usage_json],
            )
            .context("ending ai_runs row")?;
        anyhow::ensure!(n == 1, "no ai_runs row with id {run_id}");
        Ok(())
    }

    /// Pin the session's incremental-diff base to `sha` (spec §5.7).
    pub fn session_set_last_reviewed(&self, session_id: i64, sha: &str) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_sessions SET last_reviewed_sha = ?2 WHERE id = ?1",
                rusqlite::params![session_id, sha],
            )
            .context("setting ai_sessions last_reviewed_sha")?;
        anyhow::ensure!(n == 1, "no ai_sessions row with id {session_id}");
        Ok(())
    }

    /// The scout's stored brief for the session, as JSON; `None` until a run
    /// has produced one.
    pub fn session_brief(&self, session_id: i64) -> anyhow::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT brief_json FROM ai_sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |row| row.get(0),
            )
            .context("reading ai_sessions brief_json")
    }

    /// Replace the session's brief.
    pub fn session_set_brief(&self, session_id: i64, brief_json: &str) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_sessions SET brief_json = ?2 WHERE id = ?1",
                rusqlite::params![session_id, brief_json],
            )
            .context("setting ai_sessions brief_json")?;
        anyhow::ensure!(n == 1, "no ai_sessions row with id {session_id}");
        Ok(())
    }

    /// Insert an `ai_findings` row from `finding` (its `id` is ignored) and
    /// return the assigned id.
    ///
    /// Spec §11 gives findings no agent column, so the producing agent
    /// round-trips inside `evidence_json` next to the evidence list.
    pub fn finding_insert(&self, session_id: i64, finding: &Finding) -> anyhow::Result<i64> {
        let anchor_json =
            serde_json::to_string(&finding.anchor).context("serializing finding anchor")?;
        let blob = EvidenceBlob {
            agent: finding.agent.clone(),
            evidence: finding.evidence.clone(),
        };
        let evidence_json = serde_json::to_string(&blob).context("serializing finding evidence")?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ai_findings
                (session_id, run_id, path, anchor_json, severity, title, body,
                 evidence_json, checklist_item, state, state_reason, pending_local_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                session_id,
                finding.run_id,
                finding.anchor.path(),
                anchor_json,
                finding.severity.as_str(),
                finding.title,
                finding.body,
                evidence_json,
                finding.checklist_item,
                finding.state.as_str(),
                finding.state_reason,
                finding.pending_local_id.map(|id| id.to_string()),
            ],
        )
        .context("inserting ai_findings row")?;
        Ok(conn.last_insert_rowid())
    }

    /// Move finding `id` to a new `state`, change its `severity` when one is
    /// given (the verifier's downgrade, spec §5.5), with an optional reason.
    /// When `anchor_json` is given the finding is re-anchored to it (the
    /// prior check's corrected anchor, spec §12); the `path` column follows
    /// the new anchor.
    pub fn finding_update(
        &self,
        id: i64,
        state: FindingState,
        severity: Option<Severity>,
        reason: Option<&str>,
        anchor_json: Option<&str>,
    ) -> anyhow::Result<()> {
        let new_path: Option<String> = match anchor_json {
            Some(json) => {
                let anchor: FindingAnchor =
                    serde_json::from_str(json).context("parsing finding anchor")?;
                Some(anchor.path().to_string())
            }
            None => None,
        };
        let n = self
            .conn()
            .execute(
                "UPDATE ai_findings
                    SET state = ?2, severity = COALESCE(?3, severity), state_reason = ?4,
                        anchor_json = COALESCE(?5, anchor_json),
                        path = COALESCE(?6, path)
                  WHERE id = ?1",
                rusqlite::params![
                    id,
                    state.as_str(),
                    severity.map(|severity| severity.as_str()),
                    reason,
                    anchor_json,
                    new_path
                ],
            )
            .context("updating ai_findings row")?;
        anyhow::ensure!(n == 1, "no ai_findings row with id {id}");
        Ok(())
    }

    /// Rewrite a finding's body — the inline editor's save (Task 5). Nothing
    /// else about the row changes.
    pub fn finding_set_body(&self, id: i64, body: &str) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_findings SET body = ?2 WHERE id = ?1",
                rusqlite::params![id, body],
            )
            .context("updating ai_findings body")?;
        anyhow::ensure!(n == 1, "no ai_findings row with id {id}");
        Ok(())
    }

    /// Every finding of the session, oldest first.
    pub fn finding_list(&self, session_id: i64) -> anyhow::Result<Vec<Finding>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, run_id, anchor_json, severity, title, body,
                        evidence_json, checklist_item, state, state_reason, pending_local_id
                 FROM ai_findings WHERE session_id = ?1 ORDER BY id",
            )
            .context("preparing finding list query")?;
        let rows = stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            })
            .context("querying ai_findings")?;
        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                run_id,
                anchor_json,
                severity,
                title,
                body,
                evidence_json,
                checklist_item,
                state,
                state_reason,
                pending_local_id,
            ) = row.context("reading ai_findings row")?;
            let anchor =
                serde_json::from_str(&anchor_json).context("decoding finding anchor_json")?;
            let blob: EvidenceBlob =
                serde_json::from_str(&evidence_json).context("decoding finding evidence_json")?;
            let pending_local_id = pending_local_id
                .map(|raw| {
                    raw.parse::<u64>()
                        .with_context(|| format!("decoding pending_local_id {raw:?}"))
                })
                .transpose()?;
            out.push(Finding {
                id,
                run_id,
                agent: blob.agent,
                anchor,
                severity: Severity::parse(&severity)?,
                title,
                body,
                evidence: blob.evidence,
                checklist_item,
                state: FindingState::parse(&state)?,
                state_reason,
                pending_local_id,
            });
        }
        Ok(out)
    }

    /// Insert one coverage entry for `run_id` and return its id.
    pub fn coverage_insert(&self, run_id: i64, entry: &CoverageEntry) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ai_coverage (run_id, file, item, result, note)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                run_id,
                entry.file,
                entry.item,
                entry.result.as_str(),
                entry.note
            ],
        )
        .context("inserting ai_coverage row")?;
        Ok(conn.last_insert_rowid())
    }

    /// The coverage log of `run_id`, in record order.
    pub fn coverage_list(&self, run_id: i64) -> anyhow::Result<Vec<CoverageEntry>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT file, item, result, note FROM ai_coverage
                 WHERE run_id = ?1 ORDER BY id",
            )
            .context("preparing coverage list query")?;
        let rows = stmt
            .query_map(rusqlite::params![run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .context("querying ai_coverage")?;
        let mut out = Vec::new();
        for row in rows {
            let (file, item, result, note) = row.context("reading ai_coverage row")?;
            out.push(CoverageEntry {
                file,
                item,
                result: CheckResult::parse(&result)?,
                note: note.unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Record the user's answer to an `ask_user` question (spec §6.4).
    pub fn qa_insert(
        &self,
        session_id: i64,
        question: &str,
        answer: Option<&str>,
    ) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ai_qa (session_id, question, answer) VALUES (?1, ?2, ?3)",
            rusqlite::params![session_id, question, answer],
        )
        .context("inserting ai_qa row")?;
        Ok(conn.last_insert_rowid())
    }

    /// The session's stored Q&A, oldest first, as (question, answer) pairs;
    /// an unanswered question reads as an empty answer.
    pub fn qa_list(&self, session_id: i64) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT question, answer FROM ai_qa WHERE session_id = ?1 ORDER BY id")
            .context("preparing qa list query")?;
        let rows = stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })
            .context("querying ai_qa")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("reading ai_qa row")?);
        }
        Ok(out)
    }

    /// Append one transcript message, assigning `seq` as the next number for
    /// `(run_id, agent_id)` in the same transaction as the insert.
    pub fn message_append(
        &self,
        run_id: i64,
        agent_id: &str,
        role: &str,
        content_json: &str,
    ) -> anyhow::Result<i64> {
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .context("starting ai_messages transaction")?;
        let seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM ai_messages
                 WHERE run_id = ?1 AND agent_id = ?2",
                rusqlite::params![run_id, agent_id],
                |row| row.get(0),
            )
            .context("computing ai_messages seq")?;
        tx.execute(
            "INSERT INTO ai_messages (run_id, agent_id, role, seq, content_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![run_id, agent_id, role, seq, content_json],
        )
        .context("inserting ai_messages row")?;
        let id = tx.last_insert_rowid();
        tx.commit().context("committing ai_messages row")?;
        Ok(id)
    }

    /// The transcript of `(run_id, agent_id)` in `seq` order, as
    /// (role, content_json) pairs.
    pub fn messages(&self, run_id: i64, agent_id: &str) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT role, content_json FROM ai_messages
                 WHERE run_id = ?1 AND agent_id = ?2 ORDER BY seq",
            )
            .context("preparing messages query")?;
        let rows = stmt
            .query_map(rusqlite::params![run_id, agent_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("querying ai_messages")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("reading ai_messages row")?);
        }
        Ok(out)
    }

    /// The whole transcript of `run_id` in insertion order, as
    /// (agent_id, role, content_json) triples (spec §10.3: the UI rebuilds
    /// the timeline on attach).
    pub fn run_timeline(&self, run_id: i64) -> anyhow::Result<Vec<(String, String, String)>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, role, content_json FROM ai_messages
                 WHERE run_id = ?1 ORDER BY id",
            )
            .context("preparing run_timeline query")?;
        let rows = stmt
            .query_map(rusqlite::params![run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("querying run_timeline")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("reading run_timeline row")?);
        }
        Ok(out)
    }

    /// The newest run row for `session_id`, when the session has one.
    pub fn latest_run(&self, session_id: i64) -> anyhow::Result<Option<RunRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, base_sha, head_sha, status, stop_reason
                 FROM ai_runs WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
            )
            .context("preparing latest_run query")?;
        let mut rows = stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok(RunRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    base_sha: row.get(2)?,
                    head_sha: row.get(3)?,
                    status: row.get(4)?,
                    stop_reason: row.get(5)?,
                })
            })
            .context("querying latest_run")?;
        match rows.next() {
            Some(row) => Ok(Some(row.context("reading latest_run row")?)),
            None => Ok(None),
        }
    }

    /// The head SHA the last completed review ran against, when any run has
    /// recorded one (drives the incremental re-review offer, spec §5.8).
    pub fn session_last_reviewed(&self, session_id: i64) -> anyhow::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT last_reviewed_sha FROM ai_sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .context("reading last_reviewed_sha")
            .map(|found| found.flatten())
    }

    /// Record the assembler's draft for run `id` (spec §5.6). The attach
    /// snapshot replays it so a run that finished while the user was on
    /// another PR — whose `DraftReady` the stale-event rule dropped — still
    /// pre-fills the review on re-attach.
    pub fn run_set_draft(&self, run_id: i64, draft_json: &str) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_runs SET draft_json = ?2 WHERE id = ?1",
                rusqlite::params![run_id, draft_json],
            )
            .context("setting ai_runs draft_json")?;
        anyhow::ensure!(n == 1, "no ai_runs row with id {run_id}");
        Ok(())
    }

    /// The assembler draft of the newest run for `session_id`, when that run
    /// filed one (spec §10.3 replay). A draft that no longer parses reads as
    /// none, with a warning: a stale row must not fail the whole snapshot.
    pub fn run_latest_draft(&self, session_id: i64) -> anyhow::Result<Option<ReviewDraft>> {
        let raw = self.latest_run_json(session_id, "draft_json")?;
        let Some(json) = raw else { return Ok(None) };
        match serde_json::from_str(&json) {
            Ok(draft) => Ok(Some(draft)),
            Err(error) => {
                tracing::warn!(%error, "ai store: the run draft_json did not parse");
                Ok(None)
            }
        }
    }

    /// The token totals `(input, output)` recorded by the newest run for
    /// `session_id`; `(0, 0)` while that run has not ended, or when its
    /// usage does not parse.
    pub fn run_latest_usage(&self, session_id: i64) -> anyhow::Result<(u64, u64)> {
        let raw = self.latest_run_json(session_id, "usage_json")?;
        let Some(json) = raw else { return Ok((0, 0)) };
        match serde_json::from_str::<UsageBlob>(&json) {
            Ok(usage) => Ok((usage.input, usage.output)),
            Err(error) => {
                tracing::warn!(%error, "ai store: the run usage_json did not parse");
                Ok((0, 0))
            }
        }
    }

    /// One TEXT column of the newest run row for `session_id`; `column` is a
    /// literal from this file, never user input.
    fn latest_run_json(&self, session_id: i64, column: &str) -> anyhow::Result<Option<String>> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {column} FROM ai_runs WHERE session_id = ?1 ORDER BY id DESC LIMIT 1"
                ),
                rusqlite::params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .with_context(|| format!("reading ai_runs {column}"))
            .map(|found| found.flatten())
    }

    /// Move finding `id` to `state` from the UI, keeping the stored
    /// `pending_local_id` unless the caller supplies one (spec §11: the
    /// pending-comment link survives keeps and drops).
    pub fn finding_set_user_state(
        &self,
        id: i64,
        state: FindingState,
        pending_local_id: Option<u64>,
    ) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE ai_findings
                    SET state = ?2, pending_local_id = COALESCE(?3, pending_local_id)
                  WHERE id = ?1",
                rusqlite::params![
                    id,
                    state.as_str(),
                    pending_local_id.map(|id| id.to_string())
                ],
            )
            .context("updating ai_findings user state")?;
        if n == 0 {
            anyhow::bail!("no finding with id {id}");
        }
        Ok(())
    }
}

/// The `ai_findings.evidence_json` shape: the evidence list plus the agent
/// that recorded it, because spec §11 gives findings no agent column.
#[derive(Serialize, Deserialize)]
struct EvidenceBlob {
    agent: AgentRef,
    evidence: Vec<Evidence>,
}

/// The `ai_runs.usage_json` shape the pipeline's `finish_run` writes.
#[derive(Deserialize)]
struct UsageBlob {
    input: u64,
    output: u64,
}

/// Bring `conn` up to the latest schema. Each migration runs in one
/// transaction together with its `user_version` bump, so a half-applied
/// migration never reports as done.
fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V2)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
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
    fn run_timeline_latest_run_and_user_state_round_trip() {
        let store = Store::open_in_memory().expect("in-memory store");
        let key = PrKey {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 7,
        };
        let session = store.session_id(&key).expect("session row");

        assert_eq!(store.latest_run(session).expect("no run yet"), None);
        assert_eq!(
            store.session_last_reviewed(session).expect("no sha yet"),
            None
        );

        let run = store
            .run_begin(session, RunKind::Full, None, "head1")
            .expect("run row");
        store
            .message_append(run, "scout", "assistant", r#"{"text":"hi"}"#)
            .expect("message");
        store
            .session_set_last_reviewed(session, "head1")
            .expect("last reviewed");

        let latest = store.latest_run(session).expect("latest run");
        assert_eq!(latest.expect("a run").head_sha, "head1");
        let timeline = store.run_timeline(run).expect("timeline");
        assert_eq!(
            timeline,
            vec![(
                "scout".to_string(),
                "assistant".to_string(),
                r#"{"text":"hi"}"#.to_string()
            )]
        );
        assert_eq!(
            store.session_last_reviewed(session).expect("sha"),
            Some("head1".to_string())
        );

        let finding_id = store
            .finding_insert(
                session,
                &Finding {
                    id: 0,
                    run_id: run,
                    agent: AgentRef {
                        id: 1,
                        role: crate::ai::tools::AgentRole::Reviewer,
                        label: "reviewer 1".to_string(),
                    },
                    anchor: FindingAnchor::Summary {
                        path: "a.rs".to_string(),
                    },
                    severity: Severity::Risk,
                    title: "t".to_string(),
                    body: "b".to_string(),
                    evidence: vec![Evidence {
                        path: "a.rs".to_string(),
                        line: None,
                        note: "n".to_string(),
                    }],
                    checklist_item: None,
                    state: FindingState::Draft,
                    state_reason: None,
                    pending_local_id: None,
                },
            )
            .expect("finding row");
        store
            .finding_set_user_state(finding_id, FindingState::Kept, Some(9))
            .expect("keep finding");
        let kept = &store.finding_list(session).expect("findings")[0];
        assert_eq!(kept.state, FindingState::Kept);
        assert_eq!(kept.pending_local_id, Some(9));
        // A later state change without a local id keeps the stored link.
        store
            .finding_set_user_state(finding_id, FindingState::Posted, None)
            .expect("post finding");
        let posted = &store.finding_list(session).expect("findings")[0];
        assert_eq!(posted.state, FindingState::Posted);
        assert_eq!(posted.pending_local_id, Some(9));
    }

    #[test]
    fn finding_set_body_rewrites_only_the_body() {
        let store = Store::open_in_memory().expect("in-memory store");
        let key = PrKey {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 7,
        };
        let session = store.session_id(&key).expect("session row");
        let run = store
            .run_begin(session, RunKind::Full, None, "head1")
            .expect("run row");
        let id = store
            .finding_insert(
                session,
                &Finding {
                    id: 0,
                    run_id: run,
                    agent: AgentRef {
                        id: 1,
                        role: crate::ai::tools::AgentRole::Reviewer,
                        label: "reviewer 1".to_string(),
                    },
                    anchor: FindingAnchor::Line(crate::test_support::line_anchor("a.rs", 3)),
                    severity: Severity::Risk,
                    title: "t".to_string(),
                    body: "before".to_string(),
                    evidence: vec![],
                    checklist_item: None,
                    state: FindingState::Draft,
                    state_reason: None,
                    pending_local_id: None,
                },
            )
            .expect("finding row");

        store.finding_set_body(id, "after").expect("set body");

        let finding = &store.finding_list(session).expect("findings")[0];
        assert_eq!(finding.body, "after");
        assert_eq!(finding.title, "t", "the edit leaves the title alone");
        assert_eq!(finding.state, FindingState::Draft);
        assert!(store.finding_set_body(id + 100, "x").is_err());
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

        // A Failed row may still hold the tree, so it is both the lock holder
        // and a recovery target (§7.3).
        store.checkout_set_state(id, CheckoutState::Failed).unwrap();
        let held = store
            .checkout_holding(&path)
            .unwrap()
            .expect("a failed row still holds");
        assert_eq!(held.state, CheckoutState::Failed);
        let unrestored = store.checkouts_unrestored().unwrap();
        assert_eq!(unrestored.len(), 1);
        assert_eq!(unrestored[0].id, id);
        assert_eq!(unrestored[0].stash_sha.as_deref(), Some("stash42"));

        store
            .checkout_set_state(id, CheckoutState::Restored)
            .unwrap();
        assert!(store.checkout_holding(&path).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
    }

    #[test]
    fn superseded_checkouts_are_neither_locks_nor_recovery_targets() {
        let store = Store::open_in_memory().unwrap();
        let path = PathBuf::from("/repos/thing");

        let id = store
            .checkout_begin(&record(
                "/repos/thing",
                OriginalRef::Branch("main".to_string()),
            ))
            .unwrap();
        store.checkout_set_stash(id, "stash42").unwrap();
        store
            .checkout_set_state(id, CheckoutState::Superseded)
            .unwrap();

        // Its replacement carries the same stash and original ref; listing the
        // old row too would apply that stash twice.
        let mut replacement = record("/repos/thing", OriginalRef::Branch("main".to_string()));
        replacement.stash_sha = Some("stash42".to_string());
        let new_id = store.checkout_begin(&replacement).unwrap();

        let held = store
            .checkout_holding(&path)
            .unwrap()
            .expect("new row holds");
        assert_eq!(held.id, new_id);
        let unrestored = store.checkouts_unrestored().unwrap();
        assert_eq!(unrestored.len(), 1);
        assert_eq!(unrestored[0].id, new_id);

        // The superseded row round-trips as `superseded` …
        let state: String = store
            .conn()
            .query_row("SELECT state FROM ai_checkout WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "superseded");
        // … and is absent from both queries even once the replacement is done.
        store
            .checkout_set_state(new_id, CheckoutState::Restored)
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
            assert_eq!(store.user_version().unwrap(), 2);
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store.user_version().unwrap(),
            2,
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

    /// A database left at migration 1 (before `ai_runs.draft_json` existed)
    /// upgrades on the next open and keeps every row (spec §11).
    #[test]
    fn migration_v2_upgrades_a_v1_database_and_keeps_rows() {
        use crate::ai::stages::assemble::{ReviewDraft, Verdict};
        use std::collections::BTreeMap;

        let dir = tempdir();
        let db = ai_db_path(dir.path());
        let key = PrKey {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 7,
        };
        let run;
        {
            let store = Store::open(&db).unwrap();
            let session = store.session_id(&key).unwrap();
            run = store
                .run_begin(session, RunKind::Full, None, "head1")
                .unwrap();
            store
                .run_end(run, "completed", None, Some(r#"{"input":7,"output":3}"#))
                .unwrap();
            // Back to a migration-1 database: the new column goes, and the
            // version with it.
            store
                .conn()
                .execute_batch(
                    "ALTER TABLE ai_runs DROP COLUMN draft_json; PRAGMA user_version = 1;",
                )
                .unwrap();
            assert_eq!(store.user_version().unwrap(), 1);
        }

        let store = Store::open(&db).unwrap();
        assert_eq!(store.user_version().unwrap(), 2, "the v1 database upgrades");
        let session = store.session_id(&key).unwrap();
        let latest = store
            .latest_run(session)
            .unwrap()
            .expect("the run row survived the upgrade");
        assert_eq!(latest.id, run);
        assert_eq!(latest.status, "completed");
        assert_eq!(
            store.run_latest_usage(session).unwrap(),
            (7, 3),
            "the usage the v1 database already had still reads back"
        );
        assert_eq!(
            store.run_latest_draft(session).unwrap(),
            None,
            "the upgraded column starts empty"
        );

        // And the new column works on the upgraded database.
        let draft = ReviewDraft {
            summary: "a clean change".to_string(),
            verdict: Verdict::Approve,
            rationale: "no findings".to_string(),
            rewrites: BTreeMap::new(),
        };
        store
            .run_set_draft(run, &serde_json::to_string(&draft).unwrap())
            .unwrap();
        assert_eq!(store.run_latest_draft(session).unwrap(), Some(draft));
    }

    /// The draft and usage the snapshot replays: both read from the newest
    /// run row, and a run that has not ended (or never assembled) reads as
    /// nothing (spec §10.3).
    #[test]
    fn run_draft_and_usage_replay_from_the_latest_run() {
        use crate::ai::stages::assemble::{ReviewDraft, Verdict};
        use std::collections::BTreeMap;

        let store = Store::open_in_memory().unwrap();
        let session = session(&store);
        assert_eq!(store.run_latest_draft(session).unwrap(), None);
        assert_eq!(store.run_latest_usage(session).unwrap(), (0, 0));

        let first = store
            .run_begin(session, RunKind::Full, None, "head1")
            .unwrap();
        let draft = ReviewDraft {
            summary: "adds the retry loop".to_string(),
            verdict: Verdict::RequestChanges,
            rationale: "a kept bug blocks the merge".to_string(),
            rewrites: BTreeMap::from([(4, "rewritten".to_string())]),
        };
        store
            .run_set_draft(first, &serde_json::to_string(&draft).unwrap())
            .unwrap();
        store
            .run_end(first, "completed", None, Some(r#"{"input":10,"output":2}"#))
            .unwrap();
        assert_eq!(
            store.run_latest_draft(session).unwrap(),
            Some(draft.clone())
        );
        assert_eq!(store.run_latest_usage(session).unwrap(), (10, 2));

        // A newer run that has not assembled (or ended) shadows the old one.
        let second = store
            .run_begin(session, RunKind::Full, None, "head2")
            .unwrap();
        assert_eq!(store.run_latest_draft(session).unwrap(), None);
        assert_eq!(store.run_latest_usage(session).unwrap(), (0, 0));
        store
            .run_set_draft(second, &serde_json::to_string(&draft).unwrap())
            .unwrap();
        assert_eq!(store.run_latest_draft(session).unwrap(), Some(draft));

        // A row that does not parse reads as nothing, not an error.
        store
            .conn()
            .execute(
                "UPDATE ai_runs SET draft_json = 'not json' WHERE id = ?1",
                [second],
            )
            .unwrap();
        assert_eq!(store.run_latest_draft(session).unwrap(), None);
        store
            .conn()
            .execute(
                "UPDATE ai_runs SET usage_json = '{\"tokens\":10}' WHERE id = ?1",
                [second],
            )
            .unwrap();
        assert_eq!(store.run_latest_usage(session).unwrap(), (0, 0));

        assert!(store.run_set_draft(999, "{}").is_err());
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

    fn session(store: &Store) -> i64 {
        store
            .session_id(&PrKey {
                host: "github.com".to_string(),
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 7,
            })
            .unwrap()
    }

    #[test]
    fn run_and_session_apis_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let session = session(&store);

        let incremental = store
            .run_begin(session, RunKind::Incremental, Some("sha_base"), "sha_head")
            .unwrap();
        let full = store
            .run_begin(session, RunKind::Full, None, "sha_head")
            .unwrap();

        // The kind is stored in the CHECK constraint's spelling, and a full
        // run's missing base is NULL, not an empty string.
        let (kind, base): (String, Option<String>) = store
            .conn()
            .query_row(
                "SELECT kind, base_sha FROM ai_runs WHERE id = ?1",
                [full],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "full");
        assert_eq!(base, None);

        store
            .run_end(
                incremental,
                "completed",
                Some("done"),
                Some("{\"tokens\":10}"),
            )
            .unwrap();
        let (status, ended, usage): (String, Option<String>, Option<String>) = store
            .conn()
            .query_row(
                "SELECT status, ended_at, usage_json FROM ai_runs WHERE id = ?1",
                [incremental],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "completed");
        assert!(ended.is_some(), "run_end stamps ended_at");
        assert_eq!(usage.as_deref(), Some("{\"tokens\":10}"));
        assert!(store.run_end(999, "failed", None, None).is_err());

        assert_eq!(store.session_brief(session).unwrap(), None);
        store.session_set_brief(session, "{\"groups\":[]}").unwrap();
        assert_eq!(
            store.session_brief(session).unwrap().as_deref(),
            Some("{\"groups\":[]}")
        );
        store
            .session_set_last_reviewed(session, "sha_head")
            .unwrap();
        let sha: String = store
            .conn()
            .query_row(
                "SELECT last_reviewed_sha FROM ai_sessions WHERE id = ?1",
                [session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sha, "sha_head");
    }

    #[test]
    fn message_append_sequences_per_run_and_agent() {
        let store = Store::open_in_memory().unwrap();
        let session = session(&store);
        let run_a = store
            .run_begin(session, RunKind::Full, None, "sha_head")
            .unwrap();
        let run_b = store
            .run_begin(session, RunKind::Full, None, "sha_head")
            .unwrap();

        for i in 1..=3 {
            store
                .message_append(run_a, "reviewer 1", "assistant", &format!("{{\"n\":{i}}}"))
                .unwrap();
        }
        store
            .message_append(run_a, "scout", "assistant", "{}")
            .unwrap();
        store
            .message_append(run_b, "reviewer 1", "user", "{}")
            .unwrap();

        // Each (run, agent) stream counts from 1 on its own.
        let seqs: Vec<i64> = {
            let conn = store.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT seq FROM ai_messages WHERE run_id = ?1 AND agent_id = ?2 ORDER BY seq",
                )
                .unwrap();
            stmt.query_map(rusqlite::params![run_a, "reviewer 1"], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(seqs, vec![1, 2, 3]);

        let scout_seq: i64 = store
            .conn()
            .query_row(
                "SELECT seq FROM ai_messages WHERE run_id = ?1 AND agent_id = 'scout'",
                [run_a],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(scout_seq, 1, "the other agent of the same run starts fresh");

        // `messages` returns the transcript in seq order.
        let msgs = store.messages(run_a, "reviewer 1").unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0], ("assistant".to_string(), "{\"n\":1}".to_string()));
        assert_eq!(msgs[2], ("assistant".to_string(), "{\"n\":3}".to_string()));
        assert_eq!(store.messages(run_b, "reviewer 1").unwrap().len(), 1);
        assert!(store.messages(run_a, "nobody").unwrap().is_empty());
    }
}
