mod common;
use common::synthetic_files;
use gh_review_desktop::{
    diff::{highlight::*, row_model::*},
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
