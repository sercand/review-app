//! End-to-end tests for the review pipeline (spec §9, Task 15).
//!
//! Each test drives [`start_pipeline`] against a temp git repo with a real
//! diff, a `LocalHost` sandbox and a [`ScriptedLlm`] routed by agent label,
//! then asserts on the event stream, the store and the control handle.
//! `max_parallel_reviewers` is pinned to 1 so reviewer order is deterministic.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use gh_review_desktop::ai::events::{PipelineEvent, StageKind, StageStatus};
use gh_review_desktop::ai::interaction::Interaction;
use gh_review_desktop::ai::model::Llm;
use gh_review_desktop::ai::pipeline::{
    GroupState, PipelineControl, PipelineInputs, RunMode, RunStatus, start_pipeline,
};
use gh_review_desktop::ai::review::FindingState;
use gh_review_desktop::ai::snapshot::PrSnapshot;
use gh_review_desktop::ai::stages::assemble::Verdict;
use gh_review_desktop::ai::store::{PrKey, Store};
use gh_review_desktop::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
use gh_review_desktop::ai::tools::AgentRole;
use gh_review_desktop::diff::parse::parse_patch;
use gh_review_desktop::github::files::{FileStatus, LoadedFile, PatchState, PrFile};
use gh_review_desktop::github::metadata::PrMetadata;
use gh_review_desktop::model::RepoRef;
use gh_review_desktop::model::diff::FileDiff;
use gh_review_desktop::persist::settings::{AiSettings, ModelSettings, RepoAiSettings};
use review_sandbox::{LocalHost, SandboxOps};

/// The PR patch of `src/a.rs`: RIGHT-side lines 1..=5, lines 3..=5 added.
const A_PATCH: &str =
    "@@ -1,2 +1,5 @@\n fn one() {}\n fn two() {}\n+fn helper_dup() {\n+    helper();\n+}\n";

/// The checklist items the app assigns a source file.
const SOURCE_ITEMS: [&str; 10] = [
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
];

/// The checklist items the app assigns an asset file.
const ASSET_ITEMS: [&str; 2] = ["assets", "consistency"];

// ---------------------------------------------------------------------------
// Fixture repo and snapshot
// ---------------------------------------------------------------------------

/// Run git with a hermetic identity, panicking on failure.
fn gitx(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
        .arg("-c")
        .arg("user.name=t")
        .arg("-c")
        .arg("user.email=t@t")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .args(args)
        .output()
        .expect("git on PATH");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A temp git repo on `main`: a base commit, then a real diff that grows
/// `src/a.rs` (duplicating a helper that lives in `src/util.rs`) and
/// `assets/logo.png`.
fn fixture_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    gitx(dir.path(), &["init", "-b", "main", "."]);
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::create_dir_all(dir.path().join("assets")).expect("assets dir");
    std::fs::write(
        dir.path().join("src/util.rs"),
        "pub fn helper() {\n    // the one real helper\n}\n",
    )
    .expect("write util");
    std::fs::write(dir.path().join("src/a.rs"), "fn one() {}\nfn two() {}\n").expect("write a");
    std::fs::write(dir.path().join("assets/logo.png"), b"small").expect("write logo");
    gitx(dir.path(), &["add", "-A"]);
    gitx(dir.path(), &["commit", "-m", "base"]);

    std::fs::write(
        dir.path().join("src/a.rs"),
        "fn one() {}\nfn two() {}\nfn helper_dup() {\n    helper();\n}\n",
    )
    .expect("write a head");
    std::fs::write(dir.path().join("assets/logo.png"), vec![0u8; 4096]).expect("write logo head");
    gitx(dir.path(), &["add", "-A"]);
    gitx(
        dir.path(),
        &["commit", "-m", "duplicate the helper and grow the logo"],
    );
    dir
}

/// The `o/r` PR #7 metadata every unit test uses, re-built here because
/// integration tests cannot see `test_support`.
fn meta(head_sha: &str) -> PrMetadata {
    PrMetadata {
        node_id: "node".to_owned(),
        number: 7,
        title: "a pull request".to_owned(),
        body: String::new(),
        author: "author".to_owned(),
        state: "open".to_owned(),
        draft: false,
        base_ref: "main".to_owned(),
        base_sha: "sha_base".to_owned(),
        head_ref: "feature".to_owned(),
        head_sha: head_sha.to_owned(),
        additions: 3,
        deletions: 0,
        changed_files: 2,
        requested_reviewers: Vec::new(),
        labels: Vec::new(),
        milestone: None,
        html_url: "https://github.com/o/r/pull/7".to_owned(),
        mergeable: Some(true),
        mergeable_state: "clean".to_owned(),
        merge_state_status: "clean".to_owned(),
        merged: false,
    }
}

/// The frozen PR: `src/a.rs` with a parsed patch, `assets/logo.png` binary.
fn snapshot() -> PrSnapshot {
    PrSnapshot {
        repo: RepoRef::new("o", "r"),
        number: 7,
        metadata: meta("head_sha_1"),
        files: vec![
            LoadedFile {
                file: PrFile {
                    path: "src/a.rs".to_owned(),
                    previous_path: None,
                    status: FileStatus::Modified,
                    additions: 3,
                    deletions: 0,
                    changes: 3,
                    patch: Some(A_PATCH.to_owned()),
                    patch_state: PatchState::Present,
                    blob_sha: "b-a".to_owned(),
                },
                diff: Some(FileDiff {
                    hunks: parse_patch(A_PATCH).expect("the test patch parses"),
                }),
            },
            LoadedFile {
                file: PrFile {
                    path: "assets/logo.png".to_owned(),
                    previous_path: None,
                    status: FileStatus::Modified,
                    additions: 0,
                    deletions: 0,
                    changes: 0,
                    patch: None,
                    patch_state: PatchState::Binary,
                    blob_sha: "b-logo".to_owned(),
                },
                diff: None,
            },
        ],
        threads: Vec::new(),
        conversation: Vec::new(),
        checks: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn model() -> ModelSettings {
    ModelSettings {
        provider: "work".into(),
        model: "claude-test".into(),
        context_window: 100_000,
        max_tokens: 4096,
        reasoning: None,
    }
}

/// A started run with every handle the test needs; `_repo` keeps the
/// checkout (and with it the `LocalHost` root) alive for the whole test.
struct Started {
    control: PipelineControl,
    rx: mpsc::UnboundedReceiver<PipelineEvent>,
    handle: JoinHandle<RunStatus>,
    llm: Arc<ScriptedLlm>,
    store: Arc<Store>,
    session: i64,
    _repo: TempDir,
}

fn start(scripts: BTreeMap<String, Vec<ScriptStep>>) -> Started {
    start_with(scripts, AiSettings::default(), None, None)
}

/// Start a run over the fixture repo: one parallel reviewer, the per-agent
/// script, and optional `bash_disabled` reason and per-call token usage.
fn start_with(
    scripts: BTreeMap<String, Vec<ScriptStep>>,
    mut settings: AiSettings,
    bash_disabled: Option<String>,
    usage_per_call: Option<(u64, u64)>,
) -> Started {
    settings.max_parallel_reviewers = 1;
    let repo = fixture_repo();
    let host = LocalHost::new(repo.path()).expect("local host");
    let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
    let session = store
        .session_id(&PrKey {
            host: "github.com".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 7,
        })
        .expect("session row");
    let mut llm = ScriptedLlm::per_agent(model(), scripts);
    if let Some((input, output)) = usage_per_call {
        llm = llm.usage_per_call(input, output);
    }
    let llm = Arc::new(llm);
    let main_llm: Arc<dyn Llm> = llm.clone();
    let interaction: Arc<dyn Interaction> = Arc::new(ScriptedInteraction::new());
    let sandbox: Arc<dyn SandboxOps> = Arc::new(host);
    let (control, rx, handle) = start_pipeline(PipelineInputs {
        store: store.clone(),
        session_id: session,
        snapshot: Arc::new(snapshot()),
        repo_root: repo.path().to_path_buf(),
        sandbox,
        settings,
        repo_settings: RepoAiSettings::default(),
        llm: main_llm,
        lookup_llm: None,
        interaction,
        mode: RunMode::Full,
        bash_disabled,
    });
    Started {
        control,
        rx,
        handle,
        llm,
        store,
        session,
        _repo: repo,
    }
}

/// The next event, failing the test rather than hanging forever.
async fn next_event(rx: &mut mpsc::UnboundedReceiver<PipelineEvent>) -> PipelineEvent {
    tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("an event within 30s")
        .expect("the event channel is open")
}

/// Drain events until `RunEnded`, running `on_event` on each as it arrives.
async fn drain_until_end(
    rx: &mut mpsc::UnboundedReceiver<PipelineEvent>,
    mut on_event: impl FnMut(&PipelineEvent),
) -> Vec<PipelineEvent> {
    let mut out = Vec::new();
    loop {
        let event = next_event(rx).await;
        let last = matches!(event, PipelineEvent::RunEnded { .. });
        on_event(&event);
        out.push(event);
        if last {
            return out;
        }
    }
}

/// Drain events until `wanted` matches, then return everything taken.
async fn drain_until(
    rx: &mut mpsc::UnboundedReceiver<PipelineEvent>,
    wanted: impl Fn(&PipelineEvent) -> bool,
) -> Vec<PipelineEvent> {
    let mut out = Vec::new();
    loop {
        let event = next_event(rx).await;
        let hit = wanted(&event);
        out.push(event);
        if hit {
            return out;
        }
    }
}

/// All text blocks of a request, concatenated.
fn request_text(req: &review_llm::GenerateRequest) -> String {
    req.messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|block| match block {
            review_llm::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tool names of a request.
fn tool_names(req: &review_llm::GenerateRequest) -> Vec<String> {
    req.tools.iter().map(|t| t.name.clone()).collect()
}

/// The stage/brief/draft/run markers, in arrival order, for stage-order
/// assertions.
fn markers(events: &[PipelineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            PipelineEvent::Stage { stage, status } => {
                let stage = match stage {
                    StageKind::Scout => "scout",
                    StageKind::Review => "review",
                    StageKind::Challenge => "challenge",
                    StageKind::Verify => "verify",
                    StageKind::PriorCheck => "prior_check",
                    StageKind::Assemble => "assemble",
                };
                let status = match status {
                    StageStatus::Started => "started",
                    StageStatus::Done => "done",
                    StageStatus::Failed(_) => "failed",
                };
                Some(format!("{stage}:{status}"))
            }
            PipelineEvent::BriefReady => Some("brief".to_string()),
            PipelineEvent::DraftReady { .. } => Some("draft".to_string()),
            PipelineEvent::RunEnded { status, .. } => Some(format!("ended:{}", status.as_str())),
            _ => None,
        })
        .collect()
}

fn count_events(events: &[PipelineEvent], wanted: impl Fn(&PipelineEvent) -> bool) -> usize {
    events.iter().filter(|event| wanted(event)).count()
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// A two-group brief: `g1` core (High, source) and `g2` assets (Low, skim).
fn brief_two_groups() -> Value {
    json!({
        "intent": "duplicates a helper and ships a big logo",
        "description_matches": true,
        "groups": [
            { "title": "core", "risk": "high", "reason": "new logic", "files": ["src/a.rs"] },
            { "title": "assets", "risk": "low", "reason": "binary asset",
              "files": ["assets/logo.png"], "skim": true },
        ],
    })
}

/// A one-group brief: `g1` core (Medium, source).
/// A one-group brief covering the whole PR: `g1` core (Medium).
fn brief_one_group() -> Value {
    json!({
        "intent": "duplicates a helper",
        "description_matches": true,
        "groups": [
            { "title": "core", "risk": "medium", "reason": "new logic",
              "files": ["src/a.rs", "assets/logo.png"] },
        ],
    })
}

/// A brief with two non-skim groups: `g1` High, `g2` Low.
fn brief_two_plain_groups() -> Value {
    json!({
        "intent": "duplicates a helper and ships a big logo",
        "description_matches": true,
        "groups": [
            { "title": "core", "risk": "high", "reason": "new logic", "files": ["src/a.rs"] },
            { "title": "assets", "risk": "low", "reason": "binary asset", "files": ["assets/logo.png"] },
        ],
    })
}

fn scout_step(brief: Value) -> Vec<ScriptStep> {
    vec![ScriptStep::ToolCalls(vec![(
        "submit_brief".to_string(),
        brief,
    )])]
}

fn check_calls(path: &str, items: &[&str]) -> Vec<(String, Value)> {
    items
        .iter()
        .map(|item| {
            (
                "record_check".to_string(),
                json!({"file": path, "item": item, "result": "ok", "note": "checked"}),
            )
        })
        .collect()
}

/// The duplication finding anchored on the added lines with
/// `src/util.rs:3` as evidence.
fn core_finding_call() -> (String, Value) {
    (
        "record_finding".to_string(),
        json!({
            "path": "src/a.rs",
            "line": 4,
            "severity": "risk",
            "title": "duplicates the src/util.rs helper",
            "body": "helper_dup reimplements util::helper instead of calling it",
            "evidence": [{"path": "src/util.rs", "line": 3, "note": "the existing helper already does this"}],
            "checklist_item": "duplication",
        }),
    )
}

/// g1's reviewer turn: the duplication finding, every assigned check, then
/// `done`.
fn core_review_step() -> ScriptStep {
    let mut calls = vec![core_finding_call()];
    calls.extend(check_calls("src/a.rs", &SOURCE_ITEMS));
    calls.push(("done".to_string(), json!({"summary": "all checked"})));
    ScriptStep::ToolCalls(calls)
}

/// One reviewer turn over a group that holds both files: the finding, every
/// assigned check on both files, then `done`.
fn full_review_step() -> ScriptStep {
    let mut calls = vec![core_finding_call()];
    calls.extend(check_calls("src/a.rs", &SOURCE_ITEMS));
    calls.extend(check_calls("assets/logo.png", &ASSET_ITEMS));
    calls.push(("done".to_string(), json!({"summary": "all checked"})));
    ScriptStep::ToolCalls(calls)
}

/// g2's reviewer turn: a summary (asset) finding, both asset checks, `done`.
fn asset_review_step() -> ScriptStep {
    let mut calls = vec![(
        "record_finding".to_string(),
        json!({
            "path": "assets/logo.png",
            "line": 1,
            "severity": "nit",
            "title": "the logo grew to 4 KB",
            "body": "a 4 KB favicon is heavy; ship a compressed one",
            "evidence": [{"path": "assets/logo.png", "note": "the file size in the diff"}],
            "checklist_item": "assets",
        }),
    )];
    calls.extend(check_calls("assets/logo.png", &ASSET_ITEMS));
    calls.push(("done".to_string(), json!({"summary": "asset checked"})));
    ScriptStep::ToolCalls(calls)
}

fn verdicts_step(ids: &[i64]) -> ScriptStep {
    let verdicts: Vec<Value> = ids
        .iter()
        .map(|id| json!({"finding_id": id, "verdict": "keep", "reason": "re-opened it, it stands"}))
        .collect();
    ScriptStep::ToolCalls(vec![(
        "submit_verdicts".to_string(),
        json!({"verdicts": verdicts}),
    )])
}

fn draft_step(verdict: &str, rationale: &str) -> ScriptStep {
    ScriptStep::ToolCalls(vec![(
        "submit_review_draft".to_string(),
        json!({
            "summary": "the PR duplicates a helper and grows the logo",
            "verdict": verdict,
            "rationale": rationale,
            "rewrites": [],
        }),
    )])
}

/// Three text-only turns: the reviewer runs out of coverage reminders and
/// the stage records the gaps as skipped — a normal completion.
fn reviewer_gives_up() -> Vec<ScriptStep> {
    vec![
        ScriptStep::Text("looks fine".into()),
        ScriptStep::Text("really, done".into()),
        ScriptStep::Text("last word".into()),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_review_produces_a_draft_with_verified_findings() {
    let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
    scripts.insert("scout".into(), scout_step(brief_two_groups()));
    // g2 (the skim group) is scheduled first, then g1.
    scripts.insert(
        "reviewer".into(),
        vec![
            asset_review_step(),
            ScriptStep::Text("asset checked".into()),
            core_review_step(),
            ScriptStep::Text("all checked".into()),
        ],
    );
    // One batch per path, clusters ordered by smallest finding id: the asset
    // finding (1) first, then the source finding (2).
    scripts.insert(
        "verifier".into(),
        vec![verdicts_step(&[1]), verdicts_step(&[2])],
    );
    scripts.insert(
        "assembler".into(),
        vec![draft_step(
            "comment",
            "a duplication risk and a heavy asset",
        )],
    );

    let mut s = start(scripts);
    let events = drain_until_end(&mut s.rx, |_| {}).await;
    let status = s.handle.await.expect("the orchestrator joins");
    assert_eq!(status, RunStatus::Completed);

    // The stage lifecycle, in order, with the brief after the scout and the
    // draft after the assembler.
    assert_eq!(
        markers(&events),
        vec![
            "scout:started",
            "scout:done",
            "brief",
            "review:started",
            "review:done",
            "challenge:started",
            "challenge:done",
            "verify:started",
            "verify:done",
            "assemble:started",
            "assemble:done",
            "draft",
            "ended:completed",
        ]
    );

    // The draft: the model's verdict stands — comment meets the floor.
    let draft = events
        .iter()
        .find_map(|event| match event {
            PipelineEvent::DraftReady { draft } => Some(draft.clone()),
            _ => None,
        })
        .expect("a DraftReady event");
    assert_eq!(draft.verdict, Verdict::Comment);
    assert_eq!(draft.rationale, "a duplication risk and a heavy asset");

    // Both findings survived the verifier and are kept as drafts.
    let findings = s.store.finding_list(s.session).expect("finding rows");
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(
        findings.iter().all(|f| f.state == FindingState::Draft),
        "{findings:?}"
    );
    let asset = findings
        .iter()
        .find(|f| f.anchor.path() == "assets/logo.png")
        .expect("the asset finding");
    assert!(
        matches!(
            asset.anchor,
            gh_review_desktop::ai::review::FindingAnchor::Summary { .. }
        ),
        "a binary file anchors as a summary item: {:?}",
        asset.anchor
    );
    let core = findings
        .iter()
        .find(|f| f.anchor.path() == "src/a.rs")
        .expect("the core finding");
    assert_eq!(
        core.evidence.first().map(|e| e.path.as_str()),
        Some("src/util.rs"),
        "the evidence points at the existing helper"
    );

    // Coverage complete: one Coverage event per assigned pair.
    assert_eq!(
        count_events(&events, |e| matches!(e, PipelineEvent::Coverage { .. })),
        SOURCE_ITEMS.len() + ASSET_ITEMS.len()
    );

    // Every agent announced itself and its usage: scout, two reviewers, two
    // verifiers, assembler.
    assert_eq!(
        count_events(&events, |e| matches!(e, PipelineEvent::AgentStarted { .. })),
        6
    );
    assert_eq!(
        count_events(&events, |e| matches!(
            e,
            PipelineEvent::AgentFinished { .. }
        )),
        6
    );
    assert_eq!(
        count_events(&events, |e| matches!(e, PipelineEvent::Usage { .. })),
        6
    );
    let groups: Vec<Option<String>> = events
        .iter()
        .filter_map(|e| match e {
            PipelineEvent::AgentStarted { agent, group } if agent.role == AgentRole::Reviewer => {
                Some(group.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(groups, vec![Some("g2".into()), Some("g1".into())]);

    // The reviewers keep `bash` when it is available.
    let reviewer_request = s.llm.requests_for("reviewer").remove(0);
    assert!(tool_names(&reviewer_request).contains(&"bash".to_string()));
}

#[tokio::test]
async fn reviewer_failure_fails_only_its_group() {
    let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
    scripts.insert("scout".into(), scout_step(brief_two_groups()));
    // g2 (skim) first and fine; g1's model call fails outright.
    scripts.insert(
        "reviewer".into(),
        vec![
            asset_review_step(),
            ScriptStep::Text("asset checked".into()),
            ScriptStep::Fail(review_llm::Error::Timeout),
        ],
    );
    scripts.insert("verifier".into(), vec![verdicts_step(&[1])]);
    scripts.insert(
        "assembler".into(),
        vec![draft_step("approve", "only a nit survived")],
    );

    let mut s = start(scripts);
    let events = drain_until_end(&mut s.rx, |_| {}).await;
    let status = s.handle.await.expect("the orchestrator joins");
    assert_eq!(
        status,
        RunStatus::Completed,
        "a failed reviewer does not fail the run"
    );

    let groups = s.control.status().groups;
    assert!(
        groups.contains(&("g1".to_string(), GroupState::Failed)),
        "{groups:?}"
    );
    assert!(
        groups.contains(&("g2".to_string(), GroupState::Done)),
        "{groups:?}"
    );

    // The run still assembled g2's finding.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PipelineEvent::DraftReady { .. })),
        "the draft still arrives"
    );
    let findings = s.store.finding_list(s.session).expect("finding rows");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].anchor.path(), "assets/logo.png");
}

#[tokio::test]
async fn cancel_ends_the_run_cancelled_and_persists_partial_findings() {
    let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
    scripts.insert("scout".into(), scout_step(brief_one_group()));
    // The `read_file` after the finding parks the reviewer in
    // `spawn_blocking`, so the test's `cancel` lands while it is running.
    scripts.insert(
        "reviewer".into(),
        vec![
            ScriptStep::ToolCalls(vec![
                core_finding_call(),
                ("read_file".to_string(), json!({"path": "src/a.rs"})),
            ]),
            full_review_step(),
            ScriptStep::Text("all checked".into()),
        ],
    );
    scripts.insert("assembler".into(), vec![draft_step("comment", "unused")]);

    let mut s = start(scripts);
    drain_until(&mut s.rx, |e| {
        matches!(e, PipelineEvent::FindingAdded { .. })
    })
    .await;
    s.control.cancel();

    let status = s.handle.await.expect("the orchestrator joins");
    assert_eq!(status, RunStatus::Cancelled);

    let events = drain_until_end(&mut s.rx, |_| {}).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PipelineEvent::DraftReady { .. })),
        "the run stops before the assembler"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            PipelineEvent::RunEnded {
                status: RunStatus::Cancelled,
                ..
            }
        )),
        "the UI is told the run was cancelled"
    );

    // The finding the reviewer recorded before the cancel is persisted.
    let findings = s.store.finding_list(s.session).expect("finding rows");
    assert!(!findings.is_empty(), "partial findings survive a cancel");
    assert_eq!(findings[0].anchor.path(), "src/a.rs");
}

#[tokio::test]
async fn pause_and_note_reach_a_running_reviewer() {
    let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
    scripts.insert("scout".into(), scout_step(brief_one_group()));
    scripts.insert(
        "reviewer".into(),
        vec![
            ScriptStep::ToolCalls(vec![("read_file".to_string(), json!({"path": "src/a.rs"}))]),
            full_review_step(),
            ScriptStep::Text("all checked".into()),
        ],
    );
    scripts.insert("verifier".into(), vec![verdicts_step(&[1])]);
    scripts.insert("assembler".into(), vec![draft_step("comment", "one risk")]);

    let mut s = start(scripts);
    // Wait until the reviewer is genuinely mid-run: its first model call is
    // done and it is executing `read_file`, so the note lands in call two.
    drain_until(&mut s.rx, |e| {
        matches!(
            e,
            PipelineEvent::ToolCall { name, agent, .. }
                if name == "read_file" && agent.role == AgentRole::Reviewer
        )
    })
    .await;
    s.control.pause();
    s.control.note_all("focus on errors");
    s.control.resume();

    let status = s.handle.await.expect("the orchestrator joins");
    assert_eq!(status, RunStatus::Completed);

    let requests = s.llm.requests_for("reviewer");
    assert!(requests.len() >= 2, "{:?}", requests.len());
    let text = request_text(&requests[1]);
    assert!(
        text.contains("[note from the user] focus on errors"),
        "the second reviewer request carries the note:\n{text}"
    );
}

#[tokio::test]
async fn add_lead_after_group_started_is_rejected() {
    let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
    scripts.insert("scout".into(), scout_step(brief_two_plain_groups()));
    // g1 (High) is scheduled first; g2 (Low) gets the accepted lead. g1's
    // `read_file` parks it in `spawn_blocking` so the control calls land
    // while g1 runs and g2 is still pending. g2's extra check is never
    // recorded, so its reviewer rides out the two coverage reminders
    // (three text turns) before the stage completes.
    scripts.insert(
        "reviewer".into(),
        vec![
            ScriptStep::ToolCalls(vec![("read_file".to_string(), json!({"path": "src/a.rs"}))]),
            core_review_step(),
            ScriptStep::Text("all checked".into()),
            asset_review_step(),
            ScriptStep::Text("asset checked".into()),
            ScriptStep::Text("really".into()),
            ScriptStep::Text("last word".into()),
        ],
    );
    scripts.insert(
        "verifier".into(),
        vec![verdicts_step(&[1]), verdicts_step(&[2])],
    );
    scripts.insert(
        "assembler".into(),
        vec![draft_step("comment", "a risk and a nit")],
    );

    let mut s = start(scripts);
    drain_until(&mut s.rx, |e| {
        matches!(
            e,
            PipelineEvent::AgentStarted { agent, .. } if agent.role == AgentRole::Reviewer
        )
    })
    .await;

    // g1 has started; g2 is still waiting for its reviewer.
    let late = s.control.add_lead("g1", "too late for this one");
    assert!(late.is_err(), "{late:?}");
    assert!(
        late.unwrap_err().contains("already started"),
        "the error says why"
    );
    s.control
        .add_lead("g2", "watch the locking")
        .expect("g2 has not started");
    s.control
        .add_check("g2", "custom:changelog")
        .expect("g2 has not started");

    let status = s.handle.await.expect("the orchestrator joins");
    assert_eq!(status, RunStatus::Completed);

    // g2's seed carries the lead and the extra check.
    let seeded = s
        .llm
        .requests_for("reviewer")
        .iter()
        .any(|r| request_text(r).contains("watch the locking"));
    assert!(seeded, "the accepted lead reached a reviewer seed");
    let checked = s
        .llm
        .requests_for("reviewer")
        .iter()
        .any(|r| request_text(r).contains("custom:changelog"));
    assert!(checked, "the accepted check reached a reviewer seed");
}

#[tokio::test]
async fn token_budget_stops_early_but_still_assembles() {
    let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
    scripts.insert("scout".into(), scout_step(brief_two_plain_groups()));
    scripts.insert("reviewer".into(), vec![]);
    scripts.insert("verifier".into(), vec![]);
    scripts.insert(
        "assembler".into(),
        vec![draft_step("approve", "nothing survived the budget")],
    );

    let mut settings = AiSettings::default();
    settings.run_token_budget = 1;
    // The scout's single call already spends 200 tokens.
    let mut s = start_with(scripts, settings, None, Some((100, 100)));

    let events = drain_until_end(&mut s.rx, |_| {}).await;
    let status = s.handle.await.expect("the orchestrator joins");
    assert_eq!(status, RunStatus::BudgetExhausted);

    assert!(
        events
            .iter()
            .any(|e| matches!(e, PipelineEvent::DraftReady { .. })),
        "the assembler still runs on what exists"
    );
    assert!(
        !markers(&events)
            .iter()
            .any(|m| m.starts_with("challenge:") || m.starts_with("verify:")),
        "challenge and verify are skipped: {:?}",
        markers(&events)
    );
    let groups = s.control.status().groups;
    assert!(
        groups
            .iter()
            .all(|(_, state)| *state == GroupState::Skipped),
        "the pending groups are skipped: {groups:?}"
    );
    assert_eq!(groups.len(), 2);
}

#[tokio::test]
async fn bash_disabled_drops_the_tools_and_says_why() {
    let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
    scripts.insert("scout".into(), scout_step(brief_one_group()));
    scripts.insert("reviewer".into(), reviewer_gives_up());
    scripts.insert(
        "assembler".into(),
        vec![draft_step("approve", "nothing was found")],
    );

    let reason = "bwrap is not installed";
    let s = start_with(
        scripts,
        AiSettings::default(),
        Some(reason.to_string()),
        None,
    );
    let status = s.handle.await.expect("the orchestrator joins");
    assert_eq!(status, RunStatus::Completed);

    let note = format!("bash is unavailable in this review: {reason}");
    for prefix in ["scout", "reviewer", "assembler"] {
        let requests = s.llm.requests_for(prefix);
        assert!(!requests.is_empty(), "{prefix} ran");
        let text = request_text(&requests[0]);
        assert!(
            text.contains(&note),
            "{prefix}'s seed says why bash is gone:\n{text}"
        );
        let names = tool_names(&requests[0]);
        assert!(!names.contains(&"bash".to_string()), "{prefix}: {names:?}");
        assert!(
            !names.contains(&"request_prepare".to_string()),
            "{prefix}: {names:?}"
        );
    }
}
