//! One PR row: the badges it carries and the pure view-model behind them.
//!
//! The widget logic lives in [`pr_row_vm`] and [`render_row`] so it can be
//! unit-tested; the list screen lays rows out at 52 pt each (redesign
//! Task 15).

use eframe::egui;

use crate::ui::components::{tone_colors, Tone};

use crate::model::pr::{CiState, PrSummary, ReviewDecision};
use crate::ui::{icons, theme};

/// A status badge shown on a PR row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// The PR is a draft.
    Draft,
    /// Checks are still running.
    CiPending,
    /// Checks passed.
    CiSuccess,
    /// At least one check failed.
    CiFailure,
    /// The PR is approved.
    Approved,
    /// A reviewer requested changes.
    ChangesRequested,
    /// Reviews are required but incomplete.
    ReviewRequired,
}

impl Badge {
    /// The badge's one-word label.
    pub fn label(self) -> &'static str {
        match self {
            Badge::Draft => "draft",
            Badge::CiPending => "checks running",
            Badge::CiSuccess => "checks passed",
            Badge::CiFailure => "checks failed",
            Badge::Approved => "approved",
            Badge::ChangesRequested => "changes requested",
            Badge::ReviewRequired => "review required",
        }
    }

    /// The badge's colour, taken from the theme where one fits.
    pub fn color(self, visuals: &egui::Visuals) -> egui::Color32 {
        match self {
            Badge::Draft => visuals.weak_text_color(),
            Badge::CiPending | Badge::ReviewRequired => visuals.warn_fg_color,
            Badge::CiSuccess | Badge::Approved => egui::Color32::from_rgb(0x3f, 0xb9, 0x50),
            Badge::CiFailure | Badge::ChangesRequested => visuals.error_fg_color,
        }
    }
}

/// Everything one PR row draws.
#[derive(Clone, Debug, PartialEq)]
pub struct PrRowVm {
    /// The PR title.
    pub title: String,
    /// `owner/name#number`.
    pub repo_number: String,
    /// The author's login.
    pub author: String,
    /// The badges, in display order: draft, checks, review.
    pub badges: Vec<Badge>,
    /// Lines added.
    pub additions: u32,
    /// Lines deleted.
    pub deletions: u32,
    /// How many comments the PR has.
    pub comments: u32,
    /// When the PR was last updated (as GitHub sent it).
    pub updated_at: String,
    /// The compact age of `updated_at` relative to the frame's `now`
    /// (redesign Task 15).
    pub age: String,
}

/// Derive a row's view-model from one summary, with `age` relative to the
/// wall-clock ISO time `now`.
pub fn pr_row_vm(pr: &PrSummary, now: &str) -> PrRowVm {
    let mut badges = Vec::new();
    if pr.draft {
        badges.push(Badge::Draft);
    }
    badges.extend(pr.ci.map(ci_badge));
    badges.extend(pr.review.map(review_badge));

    PrRowVm {
        title: pr.title.clone(),
        repo_number: format!("{}#{}", pr.repo, pr.number),
        author: pr.author.clone(),
        badges,
        additions: pr.additions,
        deletions: pr.deletions,
        comments: pr.comments,
        updated_at: pr.updated_at.clone(),
        age: relative_time(&pr.updated_at, now),
    }
}

/// The badge for a check state.
fn ci_badge(ci: CiState) -> Badge {
    match ci {
        CiState::Pending => Badge::CiPending,
        CiState::Success => Badge::CiSuccess,
        CiState::Failure => Badge::CiFailure,
    }
}

/// The badge for a review decision.
fn review_badge(review: ReviewDecision) -> Badge {
    match review {
        ReviewDecision::Approved => Badge::Approved,
        ReviewDecision::ChangesRequested => Badge::ChangesRequested,
        ReviewDecision::ReviewRequired => Badge::ReviewRequired,
    }
}

/// GitHub's timestamps are `YYYY-MM-DDTHH:MM:SSZ`; parse one to epoch
/// seconds, or `None` for anything else. Hand-rolled so relative times need
/// no date crate.
fn parse_iso(secs: &str) -> Option<i64> {
    let b = secs.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| secs.get(r)?.parse::<i64>().ok();
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (
        num(0..4),
        num(5..7),
        num(8..10),
        num(11..13),
        num(14..16),
        num(17..19),
    ) else {
        return None;
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + s)
}

/// Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days from 1970-01-01 (the inverse of the above).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The current wall-clock time as a GitHub-style ISO 8601 string.
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3_600,
        (tod % 3_600) / 60,
        tod % 60
    )
}

/// `iso`'s compact age relative to `now` (`"2m"`, `"3h"`, `"3d"`, `"2mo"`,
/// `"1y"`); unparseable input comes back unchanged.
pub fn relative_time(iso: &str, now: &str) -> String {
    let (Some(then), Some(now)) = (parse_iso(iso), parse_iso(now)) else {
        return iso.to_owned();
    };
    let diff = now - then;
    if diff < 0 {
        return "now".to_owned();
    }
    if diff < 60 {
        format!("{diff}s")
    } else if diff < 3_600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3_600)
    } else if diff < 30 * 86_400 {
        format!("{}d", diff / 86_400)
    } else if diff < 365 * 86_400 {
        format!("{}mo", diff / (30 * 86_400))
    } else {
        format!("{}y", diff / (365 * 86_400))
    }
}

/// GitHub's compact count style: `940`, `1.2k`, `12k`, `3.4M`.
pub fn compact_count(n: u32) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        compact_unit(n, 1_000, 'k')
    } else {
        compact_unit(n, 1_000_000, 'M')
    }
}

/// `n` scaled to `unit` with `suffix`, one decimal below 10 units.
fn compact_unit(n: u32, unit: u32, suffix: char) -> String {
    let whole = n / unit;
    if whole >= 10 || n % unit < unit / 10 {
        format!("{whole}{suffix}")
    } else {
        format!("{}.{d}{suffix}", whole, d = (n % unit) * 10 / unit)
    }
}

/// The additions/deletions shares of the diff bar, `(add, del)`.
pub fn diff_fractions(additions: u32, deletions: u32) -> (f32, f32) {
    let total = additions + deletions;
    if total == 0 {
        return (0.0, 0.0);
    }
    (
        additions as f32 / total as f32,
        deletions as f32 / total as f32,
    )
}

/// The icon standing for a scope name on a row's "why" strip (Task 15).
pub fn scope_icon(name: &str) -> char {
    match name {
        "assigned" => icons::USER_CHECK,
        "authored" => icons::PENCIL_LINE,
        "review-requested" => icons::EYE,
        "involves" => icons::USERS,
        _ => icons::AT_SIGN,
    }
}

/// The height of one PR list row (Task 15).
pub const ROW_H: f32 = 52.0;

/// The row grid's fixed columns after the title, left to right (mockup
/// `prs.html`): why, CI, review, changes, comments, updated.
const COLS: [f32; 6] = [54.0, 26.0, 150.0, 120.0, 44.0, 64.0];

/// The gap between grid columns.
const COL_GAP: f32 = 14.0;

/// The row's side padding.
const PAD_X: f32 = 16.0;

/// The x ranges of the state icon, the title and the six fixed columns in
/// a row spanning `rect`.
fn columns(rect: egui::Rect) -> (egui::Rangef, egui::Rangef, [egui::Rangef; 6]) {
    let mut right = rect.right() - PAD_X;
    let mut fixed = [egui::Rangef::new(0.0, 0.0); 6];
    for i in (0..COLS.len()).rev() {
        fixed[i] = egui::Rangef::new(right - COLS[i], right);
        right -= COLS[i] + COL_GAP;
    }
    let state = egui::Rangef::new(rect.left() + PAD_X, rect.left() + PAD_X + 20.0);
    let title = egui::Rangef::new(state.max + COL_GAP, right.max(state.max + COL_GAP + 60.0));
    (state, title, fixed)
}

/// The column header above the rows: "Pull request", "Why", "CI",
/// "Review", "Changes", "Updated" in `faint` 11.5 pt.
pub fn render_header(ui: &mut egui::Ui) {
    let pal = theme::palette_of(ui);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, pal.bg);
    ui.painter()
        .hline(rect.x_range(), rect.bottom() - 0.5, (1.0, pal.line));
    let (_, title, fixed) = columns(rect);
    let font = egui::FontId::proportional(11.5);
    let cy = rect.center().y;
    let left = |x: f32, text: &str| {
        ui.painter().text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            text,
            font.clone(),
            pal.faint,
        );
    };
    left(title.min, "Pull request");
    left(fixed[0].min, "Why");
    left(fixed[1].min, "CI");
    left(fixed[2].min, "Review");
    ui.painter().text(
        egui::pos2(fixed[3].max, cy),
        egui::Align2::RIGHT_CENTER,
        "Changes",
        font.clone(),
        pal.faint,
    );
    ui.painter().text(
        egui::pos2(fixed[5].max, cy),
        egui::Align2::RIGHT_CENTER,
        "Updated",
        font,
        pal.faint,
    );
}

/// Paint one 52 pt PR row across the full available width and return its
/// click response. The cells sit on the fixed grid of [`render_header`]:
/// state icon, title over a repo/author meta line, the scopes that matched,
/// the CI icon, the review pill, `+a −d` with a 40×6 bar, the comment
/// count, and the relative time. Hover fills `surface`, selection fills
/// `accent_soft`.
pub fn render_row(ui: &mut egui::Ui, pr: &PrSummary, now: &str, selected: bool) -> egui::Response {
    let pal = theme::palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_H),
        egui::Sense::click(),
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter_at(rect);
    if selected {
        painter.rect_filled(rect, 0.0, pal.accent_soft);
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, pal.surface);
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, (1.0, pal.line_soft));
    let (state, title, fixed) = columns(rect);
    let cy = rect.center().y;

    // State icon.
    let (icon, color) = if pr.draft {
        (icons::GIT_PULL_REQUEST_DRAFT, pal.faint)
    } else {
        (icons::GIT_PULL_REQUEST, pal.ok)
    };
    painter.text(
        egui::pos2(state.center(), cy),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(16.0),
        color,
    );

    // Title over the meta line: repo, #number, "by", avatar, author, Draft.
    let title_galley = crate::ui::components::elided_galley(
        ui,
        &pr.title,
        crate::ui::fonts::semibold(13.0),
        pal.text,
        title.span(),
    );
    let title_elided = title_galley.elided;
    painter.galley(
        egui::pos2(title.min, rect.top() + 9.0),
        title_galley,
        pal.text,
    );
    let meta_y = rect.top() + 33.0;
    let small = egui::FontId::proportional(12.0);
    let mut x = title.min;
    let put = |x: &mut f32, text: String, color: egui::Color32, gap: f32| {
        let g = painter.layout_no_wrap(text, small.clone(), color);
        let w = g.size().x;
        painter.galley(egui::pos2(*x, meta_y - g.size().y / 2.0), g, color);
        *x += w + gap;
    };
    put(&mut x, pr.repo.to_string(), pal.muted, 6.0);
    put(&mut x, format!("#{}", pr.number), pal.faint, 6.0);
    put(&mut x, "by".to_owned(), pal.faint, 5.0);
    let avatar = egui::Rect::from_center_size(egui::pos2(x + 8.0, meta_y), egui::vec2(16.0, 16.0));
    painter.circle_filled(
        avatar.center(),
        8.0,
        crate::ui::widgets::thread_view::avatar_color(&pr.author),
    );
    painter.text(
        avatar.center(),
        egui::Align2::CENTER_CENTER,
        crate::ui::widgets::thread_view::avatar_initials(&pr.author),
        egui::FontId::proportional(7.5),
        theme::AVATAR_INK,
    );
    x += 21.0;
    put(&mut x, pr.author.clone(), pal.muted, 6.0);
    if pr.draft {
        let g = painter.layout_no_wrap(
            "Draft".to_owned(),
            egui::FontId::proportional(10.5),
            pal.muted,
        );
        let r = egui::Rect::from_min_size(
            egui::pos2(x, meta_y - 8.0),
            egui::vec2(g.size().x + 12.0, 16.0),
        );
        painter.rect_filled(r, 8.0, pal.raised);
        painter.galley(r.center() - g.size() / 2.0, g, pal.muted);
    }

    // Why: the scopes that matched.
    let mut wx = fixed[0].min;
    for scope in &pr.scopes {
        painter.text(
            egui::pos2(wx + 7.0, cy),
            egui::Align2::CENTER_CENTER,
            scope_icon(scope),
            egui::FontId::proportional(14.0),
            pal.faint,
        );
        wx += 16.0;
    }

    // CI.
    if let Some(ci) = pr.ci {
        let (icon, color) = match ci {
            CiState::Success => (icons::CIRCLE_CHECK, pal.ok),
            CiState::Failure => (icons::CIRCLE_X, pal.bad),
            CiState::Pending => (icons::CIRCLE_DOT_DASHED, pal.warn),
        };
        painter.text(
            egui::pos2(fixed[1].min + 8.0, cy),
            egui::Align2::CENTER_CENTER,
            icon,
            egui::FontId::proportional(16.0),
            color,
        );
    } else {
        painter.text(
            egui::pos2(fixed[1].min + 8.0, cy),
            egui::Align2::CENTER_CENTER,
            icons::MINUS,
            egui::FontId::proportional(14.0),
            pal.faint,
        );
    }

    // Review.
    review_pill(ui, &painter, egui::pos2(fixed[2].min, cy), pr.review);

    // Changes: +a −d and the bar, right-aligned.
    let bar = egui::Rect::from_min_size(
        egui::pos2(fixed[3].max - 40.0, cy - 3.0),
        egui::vec2(40.0, 6.0),
    );
    painter.rect_filled(bar, 3.0, pal.raised);
    let (a, d) = diff_fractions(pr.additions, pr.deletions);
    let w_add = (bar.width() * a).round();
    if w_add > 0.0 {
        painter.rect_filled(
            egui::Rect::from_min_size(bar.min, egui::vec2(w_add, 6.0)),
            0.0,
            pal.ok,
        );
    }
    if d > 0.0 {
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(bar.min.x + w_add, bar.min.y), bar.max),
            0.0,
            pal.bad,
        );
    }
    let mono = egui::FontId::monospace(11.5);
    let del = painter.layout_no_wrap(
        format!("−{}", compact_count(pr.deletions)),
        mono.clone(),
        pal.bad,
    );
    let del_x = bar.min.x - 8.0 - del.size().x;
    painter.galley(egui::pos2(del_x, cy - del.size().y / 2.0), del, pal.bad);
    let add = painter.layout_no_wrap(format!("+{}", compact_count(pr.additions)), mono, pal.ok);
    painter.galley(
        egui::pos2(del_x - 6.0 - add.size().x, cy - add.size().y / 2.0),
        add,
        pal.ok,
    );

    // Comments.
    let (comment_color, comment_text) = if pr.comments > 0 {
        (pal.muted, compact_count(pr.comments))
    } else {
        (pal.faint.gamma_multiply(0.6), "0".to_owned())
    };
    painter.text(
        egui::pos2(fixed[4].max, cy),
        egui::Align2::RIGHT_CENTER,
        format!("{} {}", icons::MESSAGE_SQUARE, comment_text),
        egui::FontId::proportional(12.0),
        comment_color,
    );

    // Updated.
    painter.text(
        egui::pos2(fixed[5].max, cy),
        egui::Align2::RIGHT_CENTER,
        relative_time(&pr.updated_at, now),
        egui::FontId::proportional(12.0),
        pal.muted,
    );

    if title_elided {
        response.on_hover_text(&pr.title)
    } else {
        response
    }
}

/// The review-state pill, left-aligned at `left_center`; "No reviews" in
/// `faint` for a PR with no decision (mockup `.pill`).
fn review_pill(
    ui: &egui::Ui,
    painter: &egui::Painter,
    left_center: egui::Pos2,
    review: Option<ReviewDecision>,
) {
    let pal = theme::palette_of(ui);
    let Some(review) = review else {
        painter.text(
            left_center,
            egui::Align2::LEFT_CENTER,
            "No reviews",
            egui::FontId::proportional(12.0),
            pal.faint,
        );
        return;
    };
    let (icon, text, tone) = match review {
        ReviewDecision::Approved => (icons::CHECK, "Approved", Tone::Ok),
        ReviewDecision::ChangesRequested => (icons::FILE_DIFF, "Changes requested", Tone::Bad),
        ReviewDecision::ReviewRequired => (icons::EYE, "Review required", Tone::Warn),
    };
    let (fg, bg) = tone_colors(pal, tone);
    let galley = painter.layout_no_wrap(
        format!("{icon} {text}"),
        egui::FontId::proportional(11.5),
        fg,
    );
    let rect = egui::Rect::from_min_size(
        egui::pos2(left_center.x, left_center.y - 10.0),
        egui::vec2(galley.size().x + 14.0, 20.0),
    );
    painter.rect_filled(rect, 10.0, bg);
    painter.galley(rect.center() - galley.size() / 2.0, galley, fg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pr_summary;

    #[test]
    fn row_vm_surfaces_draft_ci_and_review() {
        let pr = PrSummary {
            draft: true,
            ci: Some(CiState::Failure),
            review: Some(ReviewDecision::ChangesRequested),
            ..pr_summary(7, "t", 3)
        };
        let vm = pr_row_vm(&pr, "2026-10-19T12:00:00Z");
        assert_eq!(
            vm.badges,
            vec![Badge::Draft, Badge::CiFailure, Badge::ChangesRequested]
        );
        assert_eq!(vm.repo_number, "o/r#7");
    }

    #[test]
    fn a_plain_pr_has_no_badges() {
        let vm = pr_row_vm(&pr_summary(1, "t", 0), "2026-10-01T00:00:00Z");
        assert!(vm.badges.is_empty());
        assert_eq!(vm.repo_number, "o/r#1");
    }

    #[test]
    fn compact_counts_and_relative_times() {
        assert_eq!(compact_count(940), "940");
        assert_eq!(compact_count(1200), "1.2k");
        assert_eq!(compact_count(12_400), "12k");
        assert_eq!(compact_count(3_400_000), "3.4M");
        let now = "2026-10-19T12:00:00Z";
        assert_eq!(relative_time("2026-10-19T11:58:00Z", now), "2m");
        assert_eq!(relative_time("2026-10-19T09:00:00Z", now), "3h");
        assert_eq!(relative_time("2026-10-16T12:00:00Z", now), "3d");
        assert_eq!(relative_time("2026-08-19T12:00:00Z", now), "2mo");
        assert_eq!(relative_time("2025-10-19T12:00:00Z", now), "1y");
        assert_eq!(relative_time("garbage", now), "garbage");
        assert_eq!(diff_fractions(100, 100), (0.5, 0.5));
        assert_eq!(diff_fractions(0, 0), (0.0, 0.0));
    }
}
