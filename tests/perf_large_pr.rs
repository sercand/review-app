mod common;
use common::synthetic_files;
use gh_review_desktop::{
    diff::{highlight::*, row_model::*},
    model::diff::DiffMode,
    state::{filters::*, viewed::ViewedSet},
};
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(16);

#[test]
#[ignore = "wall-clock guardrail; run with `cargo test --release -- --ignored`"]
fn visible_slice_work_stays_within_frame_budget() {
    let files = synthetic_files(1_500, 34); // ≥ 50k rows
    let model = build(&files);
    assert!(model.rows.len() >= 50_000);

    let mut cache = HighlightCache::new(4_096, Box::new(SyntectHighlighter::new(true)));
    cache.spans("rs", "fn warm_up() {}"); // syntax set loads outside the timed loop

    let viewport = 40.0 * ROW_H;
    let total = model.offset_of(model.rows.len() - 1);
    let worst = (0..1_000)
        .map(|i| {
            let y = (i as f32 * 7_919.0) % total;
            let t = Instant::now();
            for idx in model.visible_range(y, viewport) {
                if let RowRef::Line { file, hunk, row } = &model.rows[idx] {
                    let r = &files[*file as usize].diff.as_ref().unwrap().hunks[*hunk as usize]
                        .rows[*row as usize];
                    let shown = display_text(&r.text);
                    let _ = cache.spans("rs", &shown);
                }
            }
            t.elapsed()
        })
        .max()
        .unwrap();
    assert!(worst < FRAME, "worst visible-slice frame took {worst:?}");
}

#[test]
#[ignore = "wall-clock guardrail; run with `cargo test --release -- --ignored`"]
fn filtering_and_folder_state_are_fast() {
    let files: Vec<_> = synthetic_files(1_500, 34)
        .into_iter()
        .map(|f| f.file)
        .collect();
    let filters = FilterState {
        path_query: "src/*".into(),
        hide_viewed: true,
        ..Default::default()
    };
    let t = Instant::now();
    let kept = apply(&files, &filters, &ViewedSet::default());
    assert!(!kept.is_empty());
    assert!(t.elapsed() < FRAME);
}

#[cfg(unix)]
#[test]
#[ignore = "wall-clock guardrail; run with `cargo test --release -- --ignored`"]
fn ai_drafts_stay_within_the_rebuild_budget() {
    use gh_review_desktop::ai::review::{Finding, FindingAnchor, FindingState, Severity};
    use gh_review_desktop::ai::tools::{AgentRef, AgentRole};
    use gh_review_desktop::model::RepoRef;
    use gh_review_desktop::model::comment::Anchor;
    use gh_review_desktop::model::diff::Side;
    use gh_review_desktop::runtime::{Event, SessionId};
    use gh_review_desktop::state::pr_detail::PrSession;

    let dir = tempfile::tempdir().expect("a temporary config root");
    let (mut session, _) = PrSession::open_at(
        dir.path(),
        SessionId(1),
        "github.com".to_owned(),
        RepoRef::new("o", "r"),
        7,
    );
    let files = synthetic_files(1_500, 34);
    let paths: Vec<String> = files.iter().map(|f| f.file.path.clone()).collect();
    session.on_event(&Event::PrFilesPage {
        session: SessionId(1),
        files,
        done: true,
    });
    // `rebuild_rows` renders one file. Select it explicitly, then seed most
    // of the drafts on it: those are the rows the timed splice must insert.
    session.select_file(0);
    let shown_path = session.files[0].file.path.clone();
    const DRAFTS_ON_SHOWN_FILE: usize = 200;

    // 500 open drafts, 200 on the rendered file and the rest spread over the
    // PR so the path scans are exercised too.
    let agent = AgentRef {
        id: 1,
        role: AgentRole::Reviewer,
        label: "reviewer:g1".to_owned(),
    };
    for id in 0..500i64 {
        let (path, line) = if (id as usize) < DRAFTS_ON_SHOWN_FILE {
            (shown_path.clone(), (id as u32 % 34) + 1)
        } else {
            (paths[(id as usize * 3) % paths.len()].clone(), 2)
        };
        session.ai.findings.insert(
            id,
            Finding {
                id,
                run_id: 1,
                agent: agent.clone(),
                anchor: FindingAnchor::Line(Anchor {
                    path,
                    side: Side::Right,
                    line,
                    start_line: None,
                    start_side: None,
                }),
                severity: Severity::Bug,
                title: "a draft".to_owned(),
                body: "the body".to_owned(),
                evidence: Vec::new(),
                checklist_item: None,
                state: FindingState::Draft,
                state_reason: None,
                pending_local_id: None,
            },
        );
    }

    // Warm the caches and the syntax set the first rebuild touches.
    session.rebuild_rows();
    let draft_rows = session
        .row_model
        .rows
        .iter()
        .filter(|row| matches!(row, RowRef::AiDraft { .. }))
        .count();
    assert_eq!(
        draft_rows, DRAFTS_ON_SHOWN_FILE,
        "the timed model carries every draft seeded on the rendered file"
    );
    let t = Instant::now();
    session.rebuild_rows();
    assert!(
        t.elapsed() < FRAME,
        "the AI draft splice took {:?}",
        t.elapsed()
    );
}

#[test]
#[ignore = "wall-clock guardrail; run with `cargo test --release -- --ignored`"]
fn selecting_a_huge_file_stays_within_frame_budget() {
    let mut files = synthetic_files(1_500, 34);
    // One 50k-line file, minified lines included.
    files.push(synthetic_files(1, 50_000).remove(0));
    let huge = files.len() - 1;
    for mode in [DiffMode::Unified, DiffMode::Split] {
        let t = Instant::now();
        let model = build_one(&files, Some(huge), mode);
        assert!(model.rows.len() >= 25_000);
        assert!(t.elapsed() < FRAME, "{mode:?} build took {:?}", t.elapsed());
        let t = Instant::now();
        let _ = build_one(&files, Some(0), mode); // switching to a small file
        assert!(t.elapsed() < Duration::from_millis(2));
    }
}
