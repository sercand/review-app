//! Findings, coverage and the state one review run shares (spec §5.4, §11).
//!
//! [`RunState`] is the single writer for findings, coverage and Q&A: the
//! `record_*` tools and the pipeline mutate through it, and every mutation
//! is persisted through [`Store`] *before* the in-memory cache is updated,
//! so a crash loses nothing that was accepted. It also emits
//! [`PipelineEvent`]s for the AI tab; a send failure means the tab has
//! detached and is ignored.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::ai::events::PipelineEvent;
use crate::ai::store::Store;
use crate::ai::tools::AgentRef;
use crate::model::comment::Anchor;

/// How serious a finding is (spec §5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Something that is wrong.
    Bug,
    /// Probably wrong, or wrong under a condition nobody proved.
    Risk,
    /// Style or polish; never blocks.
    Nit,
    /// The reviewer needs the author to answer something.
    Question,
}

impl Severity {
    /// The stored and wire spelling (spec §11).
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Bug => "bug",
            Severity::Risk => "risk",
            Severity::Nit => "nit",
            Severity::Question => "question",
        }
    }

    /// The title-case label the UI shows and a kept comment leads with.
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Bug => "Bug",
            Severity::Risk => "Risk",
            Severity::Nit => "Nit",
            Severity::Question => "Question",
        }
    }

    /// Inverse of [`Severity::as_str`].
    pub fn parse(raw: &str) -> anyhow::Result<Severity> {
        match raw {
            "bug" => Ok(Severity::Bug),
            "risk" => Ok(Severity::Risk),
            "nit" => Ok(Severity::Nit),
            "question" => Ok(Severity::Question),
            _ => anyhow::bail!("unrecognised severity {raw:?}"),
        }
    }
}

/// The comment body a kept line finding carries: its severity, then the text
/// (spec §12, Task 5).
pub fn kept_body(severity: Severity, body: &str) -> String {
    format!("**{}:** {body}", severity.label())
}

/// What became of a finding, in the spec §11 spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingState {
    /// Recorded by an agent, waiting for the user (or the verifier).
    Draft,
    /// The user kept it for the review draft.
    Kept,
    /// The user dropped it.
    DroppedByUser,
    /// The verifier dropped it, with a reason.
    DroppedByVerifier,
    /// It went to GitHub inside a submitted review.
    Posted,
    /// A re-review found the code fixed.
    Addressed,
    /// A re-review found the issue still there.
    StillPresent,
    /// A re-review could not re-anchor it.
    Outdated,
}

impl FindingState {
    /// The stored and wire spelling (spec §11).
    pub fn as_str(&self) -> &'static str {
        match self {
            FindingState::Draft => "draft",
            FindingState::Kept => "kept",
            FindingState::DroppedByUser => "dropped_by_user",
            FindingState::DroppedByVerifier => "dropped_by_verifier",
            FindingState::Posted => "posted",
            FindingState::Addressed => "addressed",
            FindingState::StillPresent => "still_present",
            FindingState::Outdated => "outdated",
        }
    }

    /// Inverse of [`FindingState::as_str`].
    pub fn parse(raw: &str) -> anyhow::Result<FindingState> {
        match raw {
            "draft" => Ok(FindingState::Draft),
            "kept" => Ok(FindingState::Kept),
            "dropped_by_user" => Ok(FindingState::DroppedByUser),
            "dropped_by_verifier" => Ok(FindingState::DroppedByVerifier),
            "posted" => Ok(FindingState::Posted),
            "addressed" => Ok(FindingState::Addressed),
            "still_present" => Ok(FindingState::StillPresent),
            "outdated" => Ok(FindingState::Outdated),
            _ => anyhow::bail!("unrecognised finding state {raw:?}"),
        }
    }
}

/// What a finding is anchored to (spec §5.6).
///
/// Serializes as a tagged enum; `anchor_json` round-trips only through this
/// crate's own store, so the shape is ours to choose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindingAnchor {
    /// A line (or range) inside the PR diff.
    Line(Anchor),
    /// A file the finding is about but that has no commentable line — a
    /// binary or asset file, or a whole-file remark (spec §5.6).
    Summary {
        /// The file the item is about.
        path: String,
    },
}

impl FindingAnchor {
    /// The file the finding is about, whichever kind it is.
    pub fn path(&self) -> &str {
        match self {
            FindingAnchor::Line(anchor) => &anchor.path,
            FindingAnchor::Summary { path } => path,
        }
    }
}

/// One `{ path, line, note }` pointer inside a finding's evidence (spec §5.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// The file the evidence points at; it may be outside the PR.
    pub path: String,
    /// The line, when the evidence has one.
    pub line: Option<u32>,
    /// What it shows; never empty.
    pub note: String,
}

/// One recorded finding: a row of `ai_findings` in memory.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// Database row id. Assigned by [`Store::finding_insert`]; `add_finding`
    /// returns it.
    pub id: i64,
    /// The run that recorded it.
    pub run_id: i64,
    /// The agent that recorded it.
    pub agent: AgentRef,
    /// Where it is anchored.
    pub anchor: FindingAnchor,
    /// How serious the recorder thought it was.
    pub severity: Severity,
    /// One-line headline.
    pub title: String,
    /// The full explanation.
    pub body: String,
    /// What it was checked against; never empty (spec §5.4).
    pub evidence: Vec<Evidence>,
    /// The checklist item it came out of, if any.
    pub checklist_item: Option<String>,
    /// Lifecycle state.
    pub state: FindingState,
    /// Why it entered that state (verifier reason, duplicate pointer, …).
    pub state_reason: Option<String>,
    /// The local pending-comment id once the finding is in the pending
    /// review (spec §11).
    pub pending_local_id: Option<u64>,
}

/// A finding as a tool hands it over, before it has an id or a state.
#[derive(Clone, Debug, PartialEq)]
pub struct NewFinding {
    /// The agent recording it.
    pub agent: AgentRef,
    /// Where it is anchored.
    pub anchor: FindingAnchor,
    /// How serious it is.
    pub severity: Severity,
    /// One-line headline.
    pub title: String,
    /// The full explanation.
    pub body: String,
    /// What it was checked against; `add_finding` requires it non-empty.
    pub evidence: Vec<Evidence>,
    /// The checklist item it came out of, if any.
    pub checklist_item: Option<String>,
}

/// The outcome of one checklist check for one file (spec §5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckResult {
    /// The check found a problem; a finding should exist for it.
    Issue,
    /// Checked and clean; the note says what was checked.
    Ok,
    /// The item does not apply to this file; the note says why.
    NotApplicable,
    /// The reviewer never recorded the pair after two reminders. Written by
    /// the app only — the `record_check` tool refuses it.
    Skipped,
}

impl CheckResult {
    /// The stored spelling, matching the tool's vocabulary.
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckResult::Issue => "issue",
            CheckResult::Ok => "ok",
            CheckResult::NotApplicable => "n/a",
            CheckResult::Skipped => "skipped",
        }
    }

    /// Inverse of [`CheckResult::as_str`].
    pub fn parse(raw: &str) -> anyhow::Result<CheckResult> {
        match raw {
            "issue" => Ok(CheckResult::Issue),
            "ok" => Ok(CheckResult::Ok),
            "n/a" => Ok(CheckResult::NotApplicable),
            "skipped" => Ok(CheckResult::Skipped),
            _ => anyhow::bail!("unrecognised check result {raw:?}"),
        }
    }
}

/// One line of the coverage log: what was checked, for which file.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageEntry {
    /// The file the check ran against.
    pub file: String,
    /// The checklist item that was checked.
    pub item: String,
    /// What came out.
    pub result: CheckResult,
    /// What was checked and how; required for `ok` and `n/a` (spec §5.4).
    pub note: String,
}

/// What one run has produced so far: findings, coverage, Q&A and the done
/// flags — all persisted through [`Store`] the moment they happen, and
/// cached here for cheap reads.
///
/// Share it as `Arc<RunState>`; every method takes `&self` and locks an
/// internal mutex, so parallel reviewers write safely. The mutex is never
/// held across an LLM call.
pub struct RunState {
    store: Arc<Store>,
    session_id: i64,
    run_id: i64,
    events: mpsc::UnboundedSender<PipelineEvent>,
    inner: Mutex<RunCache>,
}

/// The in-memory mirror of this run's rows.
#[derive(Default)]
struct RunCache {
    findings: Vec<Finding>,
    coverage: Vec<CoverageEntry>,
    qa: Vec<(String, String)>,
    /// Ids of agents that called `done` since the loop last took the flag.
    done: BTreeSet<u32>,
}

impl RunState {
    /// A run state writing to `store` for session `session_id` and run
    /// `run_id`, announcing changes on `events`.
    pub fn new(
        store: Arc<Store>,
        session_id: i64,
        run_id: i64,
        events: mpsc::UnboundedSender<PipelineEvent>,
    ) -> RunState {
        // Answered questions belong to the session, not one run: preload
        // them so an incremental run's agents see the previous run's Q&A
        // (spec §12). A read failure just means this run starts without.
        let qa = match store.qa_list(session_id) {
            Ok(qa) => qa,
            Err(error) => {
                tracing::warn!(%error, "loading session Q&A");
                Vec::new()
            }
        };
        RunState {
            store,
            session_id,
            run_id,
            events,
            inner: Mutex::new(RunCache {
                qa,
                ..RunCache::default()
            }),
        }
    }

    /// The run's id.
    pub fn run_id(&self) -> i64 {
        self.run_id
    }

    /// The session's id.
    pub fn session_id(&self) -> i64 {
        self.session_id
    }

    /// The store this run persists through, for writers that are not
    /// findings or coverage (the agent loop's transcript, spec §9.2).
    pub fn store(&self) -> &std::sync::Arc<Store> {
        &self.store
    }

    /// Locks the cache, recovering from poisoning: it mirrors rows that are
    /// already committed, so a panicking thread lost nothing.
    fn cache(&self) -> MutexGuard<'_, RunCache> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Store a new finding as [`FindingState::Draft`] and return its id.
    ///
    /// Persists first, so the returned id is a real row id. Evidence is
    /// required here, not just in the tool: `RunState` is the single writer
    /// and the verifier or pipeline may call it directly (spec §5.4).
    pub fn add_finding(&self, f: NewFinding) -> anyhow::Result<i64> {
        anyhow::ensure!(
            !f.evidence.is_empty(),
            "finding requires at least one evidence entry"
        );
        let mut finding = Finding {
            id: 0,
            run_id: self.run_id,
            agent: f.agent,
            anchor: f.anchor,
            severity: f.severity,
            title: f.title,
            body: f.body,
            evidence: f.evidence,
            checklist_item: f.checklist_item,
            state: FindingState::Draft,
            state_reason: None,
            pending_local_id: None,
        };
        let id = self.store.finding_insert(self.session_id, &finding)?;
        finding.id = id;
        self.cache().findings.push(finding);
        let _ = self.events.send(PipelineEvent::FindingAdded { id });
        Ok(id)
    }

    /// Move finding `id` to a new state, with the reason it entered it.
    pub fn set_finding_state(
        &self,
        id: i64,
        state: FindingState,
        reason: Option<String>,
    ) -> anyhow::Result<()> {
        self.set_finding_state_and_anchor(id, state, reason, None)
    }

    /// Move finding `id` to a new state and, when `anchor` is given,
    /// re-anchor it (the prior check's corrected anchor, spec §12). Store
    /// row and cache update together, with exactly one change event.
    pub fn set_finding_state_and_anchor(
        &self,
        id: i64,
        state: FindingState,
        reason: Option<String>,
        anchor: Option<FindingAnchor>,
    ) -> anyhow::Result<()> {
        let anchor_json = match &anchor {
            Some(anchor) => Some(serde_json::to_string(anchor).context("serializing anchor")?),
            None => None,
        };
        self.store
            .finding_update(id, state, None, reason.as_deref(), anchor_json.as_deref())?;
        if let Some(finding) = self.cache().findings.iter_mut().find(|f| f.id == id) {
            finding.state = state;
            finding.state_reason = reason.clone();
            if let Some(anchor) = anchor {
                finding.anchor = anchor;
            }
        }
        let _ = self.events.send(PipelineEvent::FindingChanged { id });
        Ok(())
    }

    /// Move finding `id` to a lower `severity`, keeping it a
    /// [`FindingState::Draft`] and recording why (the verifier's downgrade,
    /// spec §5.5).
    pub fn downgrade_finding(
        &self,
        id: i64,
        severity: Severity,
        reason: Option<String>,
    ) -> anyhow::Result<()> {
        self.store.finding_update(
            id,
            FindingState::Draft,
            Some(severity),
            reason.as_deref(),
            None,
        )?;
        if let Some(finding) = self.cache().findings.iter_mut().find(|f| f.id == id) {
            finding.severity = severity;
            finding.state = FindingState::Draft;
            finding.state_reason = reason;
        }
        let _ = self.events.send(PipelineEvent::FindingChanged { id });
        Ok(())
    }

    /// The findings recorded through this state, oldest first.
    pub fn findings(&self) -> Vec<Finding> {
        self.cache().findings.clone()
    }

    /// Append one line to the coverage log.
    pub fn record_check(&self, e: CoverageEntry) -> anyhow::Result<()> {
        self.store.coverage_insert(self.run_id, &e)?;
        self.cache().coverage.push(e.clone());
        let _ = self.events.send(PipelineEvent::Coverage {
            file: e.file,
            item: e.item,
        });
        Ok(())
    }

    /// The coverage log of this run, in record order.
    pub fn coverage(&self) -> Vec<CoverageEntry> {
        self.cache().coverage.clone()
    }

    /// The `(file, item)` pairs of `assigned` with no coverage entry yet —
    /// the anti-rubber-stamp check the loop makes when a reviewer says it is
    /// done (spec §5.4). Keeps the assignment order.
    pub fn missing(&self, assigned: &[(String, String)]) -> Vec<(String, String)> {
        let cache = self.cache();
        assigned
            .iter()
            .filter(|(file, item)| {
                !cache
                    .coverage
                    .iter()
                    .any(|c| &c.file == file && &c.item == item)
            })
            .cloned()
            .collect()
    }

    /// Store the user's answer to an `ask_user` question for reuse (§5.7).
    pub fn add_qa(&self, q: &str, a: &str) -> anyhow::Result<()> {
        self.store.qa_insert(self.session_id, q, Some(a))?;
        self.cache().qa.push((q.to_string(), a.to_string()));
        Ok(())
    }

    /// The session's stored Q&A, oldest first.
    pub fn qa(&self) -> Vec<(String, String)> {
        self.cache().qa.clone()
    }

    /// Record that `agent` called `done`. The agent loop consumes it with
    /// [`RunState::take_done`].
    pub fn mark_done(&self, agent: &AgentRef) {
        self.cache().done.insert(agent.id);
    }

    /// Whether `agent` called `done` since the last take; taking clears it.
    pub fn take_done(&self, agent: &AgentRef) -> bool {
        self.cache().done.remove(&agent.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::store::{PrKey, RunKind, Store};
    use crate::ai::tools::{AgentRef, AgentRole};
    use crate::model::diff::Side;
    use tokio::sync::mpsc;

    fn reviewer() -> AgentRef {
        AgentRef {
            id: 1,
            role: AgentRole::Reviewer,
            label: "reviewer 1".to_string(),
        }
    }

    /// A fresh store with one session and one run, a `RunState` over it, and
    /// the event receiver (kept alive so `try_recv` sees what was sent).
    fn run_state() -> (
        Arc<Store>,
        i64,
        i64,
        RunState,
        mpsc::UnboundedReceiver<PipelineEvent>,
    ) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = store
            .session_id(&PrKey {
                host: "github.com".to_string(),
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 7,
            })
            .unwrap();
        let run = store
            .run_begin(session, RunKind::Full, None, "sha_head")
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let state = RunState::new(store.clone(), session, run, tx);
        (store, session, run, state, rx)
    }

    fn line_finding() -> NewFinding {
        NewFinding {
            agent: reviewer(),
            anchor: FindingAnchor::Line(Anchor {
                path: "src/a.rs".to_string(),
                side: Side::Right,
                line: 12,
                start_line: None,
                start_side: None,
            }),
            severity: Severity::Bug,
            title: "panics on empty input".to_string(),
            body: "line 12 indexes without a length check".to_string(),
            evidence: vec![Evidence {
                path: "src/b.rs".to_string(),
                line: Some(4),
                note: "the only caller passes an empty vec".to_string(),
            }],
            checklist_item: Some("empty-input".to_string()),
        }
    }

    #[test]
    fn missing_lists_unrecorded_pairs() {
        let (_store, _session, _run, state, _rx) = run_state();
        let assigned = vec![
            ("a.rs".to_string(), "errors".to_string()),
            ("a.rs".to_string(), "naming".to_string()),
            ("b.rs".to_string(), "errors".to_string()),
        ];
        assert_eq!(state.missing(&assigned).len(), 3, "nothing recorded yet");
        state
            .record_check(CoverageEntry {
                file: "a.rs".to_string(),
                item: "errors".to_string(),
                result: CheckResult::Ok,
                note: "grepped unwrap, 0 hits".to_string(),
            })
            .unwrap();
        assert_eq!(
            state.missing(&assigned),
            vec![
                ("a.rs".to_string(), "naming".to_string()),
                ("b.rs".to_string(), "errors".to_string()),
            ],
            "only the recorded pair drops out, in assignment order"
        );
    }

    #[test]
    fn run_state_persists_and_reloads() {
        let (store, session, run, state, mut rx) = run_state();
        let id = state.add_finding(line_finding()).unwrap();
        state
            .set_finding_state(id, FindingState::Kept, Some("real".to_string()))
            .unwrap();
        state.add_qa("is empty allowed?", "yes").unwrap();
        state
            .record_check(CoverageEntry {
                file: "src/a.rs".to_string(),
                item: "empty-input".to_string(),
                result: CheckResult::Issue,
                note: String::new(),
            })
            .unwrap();

        // The in-memory view carries the new state.
        let findings = state.findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, id);
        assert_eq!(findings[0].run_id, run);
        assert_eq!(findings[0].state, FindingState::Kept);

        // The store holds the same rows: findings round-trip agent, anchor,
        // evidence and state through their JSON columns.
        let rows = store.finding_list(session).unwrap();
        assert_eq!(rows, findings);
        assert_eq!(rows[0].agent, reviewer());
        assert_eq!(rows[0].severity, Severity::Bug);
        assert_eq!(rows[0].checklist_item.as_deref(), Some("empty-input"));
        assert_eq!(rows[0].state_reason.as_deref(), Some("real"));
        assert_eq!(rows[0].pending_local_id, None);
        assert_eq!(store.coverage_list(run).unwrap(), state.coverage());
        assert_eq!(
            store.qa_list(session).unwrap(),
            vec![("is empty allowed?".to_string(), "yes".to_string())]
        );

        // The UI was told about each mutation, in order.
        assert_eq!(rx.try_recv(), Ok(PipelineEvent::FindingAdded { id }));
        assert_eq!(rx.try_recv(), Ok(PipelineEvent::FindingChanged { id }));
        assert_eq!(
            rx.try_recv(),
            Ok(PipelineEvent::Coverage {
                file: "src/a.rs".to_string(),
                item: "empty-input".to_string(),
            })
        );
    }

    #[test]
    fn add_finding_requires_evidence() {
        let (store, session, _run, state, _rx) = run_state();
        let mut finding = line_finding();
        finding.evidence.clear();

        let err = state.add_finding(finding).unwrap_err().to_string();
        assert!(err.contains("evidence"), "{err}");
        assert!(state.findings().is_empty(), "the cache is untouched");
        assert!(
            store.finding_list(session).unwrap().is_empty(),
            "the store is untouched"
        );
    }

    #[test]
    fn done_sets_flag_for_that_agent_only() {
        let (_store, _session, _run, state, _rx) = run_state();
        let challenger = AgentRef {
            id: 2,
            role: AgentRole::Challenger,
            label: "challenger".to_string(),
        };
        assert!(!state.take_done(&reviewer()), "nobody is done yet");
        state.mark_done(&reviewer());
        assert!(!state.take_done(&challenger), "the other agent is not done");
        assert!(state.take_done(&reviewer()));
        assert!(!state.take_done(&reviewer()), "taking the flag clears it");
    }

    #[test]
    fn run_kind_and_severity_serde_roundtrip() {
        for (kind, text) in [
            (RunKind::Full, "full"),
            (RunKind::Incremental, "incremental"),
        ] {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, text);
            assert_eq!(serde_json::from_value::<RunKind>(json).unwrap(), kind);
        }
        for (severity, text) in [
            (Severity::Bug, "bug"),
            (Severity::Risk, "risk"),
            (Severity::Nit, "nit"),
            (Severity::Question, "question"),
        ] {
            let json = serde_json::to_value(severity).unwrap();
            assert_eq!(json, text);
            assert_eq!(serde_json::from_value::<Severity>(json).unwrap(), severity);
            assert_eq!(severity.as_str(), text, "stored text matches serde");
            assert_eq!(Severity::parse(text).unwrap(), severity);
        }
        for (state, text) in [
            (FindingState::Draft, "draft"),
            (FindingState::Kept, "kept"),
            (FindingState::DroppedByUser, "dropped_by_user"),
            (FindingState::DroppedByVerifier, "dropped_by_verifier"),
            (FindingState::Posted, "posted"),
            (FindingState::Addressed, "addressed"),
            (FindingState::StillPresent, "still_present"),
            (FindingState::Outdated, "outdated"),
        ] {
            let json = serde_json::to_value(state).unwrap();
            assert_eq!(json, text, "spec §11 spelling");
            assert_eq!(serde_json::from_value::<FindingState>(json).unwrap(), state);
            assert_eq!(state.as_str(), text, "stored text matches serde");
            assert_eq!(FindingState::parse(text).unwrap(), state);
        }
    }

    #[test]
    fn anchors_round_trip_through_their_json_shape() {
        let line = FindingAnchor::Line(Anchor {
            path: "a.rs".to_string(),
            side: Side::Left,
            line: 11,
            start_line: Some(10),
            start_side: Some(Side::Left),
        });
        let json = serde_json::to_string(&line).unwrap();
        assert_eq!(serde_json::from_str::<FindingAnchor>(&json).unwrap(), line);
        let summary = FindingAnchor::Summary {
            path: "logo.png".to_string(),
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(
            serde_json::from_str::<FindingAnchor>(&json).unwrap(),
            summary
        );
        assert_eq!(line.path(), "a.rs");
        assert_eq!(summary.path(), "logo.png");
    }
}
