//! Drawing one review thread inside the diff surface (spec §7.4).
//!
//! [`thread_vm`] is the pure view-model — comments with their markdown
//! parsed, the root flagged first — and [`render_thread`] turns it into
//! widgets. Outdated and resolved threads render collapsed to a summary
//! line; the reviewer expands them from there. Avatars are initials in a
//! circle: the build ships no image loaders (spec §5), so nothing is ever
//! fetched.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;

use crate::model::review::PendingComment;
use crate::model::thread::ReviewThread;
use crate::ui::icons::{
    icon_text, CIRCLE_CHECK, COPY, MESSAGES_SQUARE, MESSAGE_SQUARE_DASHED, PENCIL, TRASH_2,
};
use crate::ui::theme::palette_of;
use crate::ui::widgets::markdown::{self, MarkdownDoc};

/// One comment as the thread widget draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct CommentVm {
    /// The author's login.
    pub author: String,
    /// The avatar URL, kept for a later build with image loaders; today the
    /// widget renders initials instead and never fetches this.
    #[allow(dead_code)]
    pub avatar: Option<String>,
    /// The parsed body.
    pub body_doc: MarkdownDoc,
    /// `createdAt` as GitHub sent it; empty for an optimistic comment.
    pub created_at: String,
    /// Whether this is the thread's root comment.
    pub is_root: bool,
}

/// Build the view-model: the root comment first, replies after it.
pub fn thread_vm(t: &ReviewThread) -> Vec<CommentVm> {
    t.comments
        .iter()
        .enumerate()
        .map(|(index, comment)| CommentVm {
            author: comment.author.clone(),
            avatar: comment.avatar_url.clone(),
            body_doc: markdown::to_doc(&comment.body),
            created_at: comment.created_at.clone(),
            is_root: index == 0,
        })
        .collect()
}

/// What the reviewer asked of a thread.
#[derive(Clone, Debug, PartialEq)]
pub enum ThreadAction {
    /// Post a reply to the thread's root comment.
    Reply {
        /// The thread's GraphQL node id, for the result event.
        thread_id: String,
        /// The root comment the reply endpoint anchors to.
        root_comment_id: u64,
        /// The reply body.
        body: String,
    },
    /// Resolve or unresolve the thread.
    ToggleResolved {
        /// The thread's GraphQL node id.
        thread_id: String,
        /// The state to move the thread to.
        resolved: bool,
    },
    /// Copy a suggestion block out of a comment.
    CopySuggestion(String),
}

/// Per-thread text the widget keeps between frames, owned by the screen.
#[derive(Clone, Debug, Default)]
pub struct CommentDrafts {
    /// Reply text being typed, by thread node id.
    pub replies: HashMap<String, String>,
    /// Threads the reviewer manually expanded out of their collapsed state.
    pub expanded: HashSet<String>,
}

/// The initials to draw in an avatar circle: the first alphanumeric
/// character, uppercased, or `?` for an empty login.
pub fn avatar_initials(author: &str) -> String {
    author
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

/// The eight avatar fills (redesign mockup `.avatar`), indexed by login.
/// The tokens live in [`crate::ui::theme`] (Task 17 review); this re-export
/// keeps the widget's API stable.
pub use crate::ui::theme::AVATAR_COLORS;

/// The avatar fill for a login: a stable hash into [`AVATAR_COLORS`].
pub fn avatar_color(login: &str) -> egui::Color32 {
    let mut h: i32 = 0;
    for byte in login.bytes() {
        h = h.wrapping_mul(31).wrapping_add(byte as i32);
    }
    AVATAR_COLORS[(h.unsigned_abs() % AVATAR_COLORS.len() as u32) as usize]
}

/// The initials avatar: a filled circle with white initials — 20 pt in
/// compact rows, 28 pt in threads (mockup `.avatar`).
pub fn avatar(ui: &mut egui::Ui, author: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size / 2.0, avatar_color(author));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        avatar_initials(author),
        egui::FontId::proportional(size * 0.42),
        crate::ui::theme::AVATAR_INK,
    );
}

/// A GitHub `createdAt` stamp ("2026-10-01T00:00:00Z") as a relative
/// ("3h ago") label; the raw stamp when it does not parse.
pub fn relative_time(created_at: &str) -> String {
    let Some(then) = parse_epoch_seconds(created_at) else {
        return created_at.to_owned();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let ago = now - then;
    let (n, unit) = if ago < 60 {
        (ago.max(0), "s")
    } else if ago < 3600 {
        (ago / 60, "m")
    } else if ago < 86_400 {
        (ago / 3600, "h")
    } else if ago < 30 * 86_400 {
        (ago / 86_400, "d")
    } else if ago < 365 * 86_400 {
        (ago / (30 * 86_400), "mo")
    } else {
        (ago / (365 * 86_400), "y")
    };
    format!("{n}{unit} ago")
}

/// The Unix seconds of a `YYYY-MM-DDTHH:MM:SSZ` stamp, via days-from-civil.
fn parse_epoch_seconds(stamp: &str) -> Option<i64> {
    let (date, rest) = stamp.split_once('T')?;
    let time = rest.strip_suffix('Z')?;
    let mut date_parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = match (date_parts.next()?, date_parts.next()?, date_parts.next()?) {
        (Some(y), Some(m), Some(d)) => (y, m, d),
        _ => return None,
    };
    let mut time_parts = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, min, s) = match (time_parts.next()?, time_parts.next()?, time_parts.next()?) {
        (Some(h), Some(min), Some(s)) => (h, min, s),
        _ => return None,
    };
    // Days from the Unix epoch, Howard Hinnant's civil-algorithm form.
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + min * 60 + s)
}

/// A ghost text button: transparent until hovered, then `accent_soft`
/// (mockup `.btn.ghost`).
fn ghost_button(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let palette = palette_of(ui);
    let response = ui.add(
        egui::Button::new(
            egui::RichText::new(text.into())
                .small()
                .color(palette.muted),
        )
        .fill(egui::Color32::TRANSPARENT)
        .corner_radius(6.0)
        .min_size(egui::vec2(0.0, 24.0)),
    );
    if response.hovered() {
        ui.painter()
            .rect_filled(response.rect, 6.0, palette.accent_soft);
    }
    response
}

/// The pill of the mockup: small text on a soft fill.
fn pill(ui: &mut egui::Ui, icon: char, text: &str, fg: egui::Color32, bg: egui::Color32) {
    let galley = ui.painter().layout_no_wrap(
        format!("{icon} {text}"),
        egui::FontId::proportional(11.0),
        fg,
    );
    let size = galley.size() + egui::vec2(12.0, 4.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, size.y / 2.0, bg);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
}

/// Where a thread draws: the signed-in user for the reply row's avatar,
/// and the left margin — deeper in unified mode, where the line-number
/// gutters stand in front.
pub struct ThreadPlacement<'a> {
    /// The signed-in user's login.
    pub login: &'a str,
    /// The frame's left margin.
    pub indent: f32,
}

/// Draw one thread.
///
/// `collapsed` shows only the summary line (outdated and resolved threads
/// arrive that way, spec §7.4); the expand control flips it through
/// `drafts`. Every control reports itself as a [`ThreadAction`].
pub fn render_thread(
    ui: &mut egui::Ui,
    thread: &ReviewThread,
    vm: &[CommentVm],
    collapsed: bool,
    drafts: &mut CommentDrafts,
    actions: &mut Vec<ThreadAction>,
    place: ThreadPlacement<'_>,
) {
    let ThreadPlacement { login, indent } = place;
    let palette = palette_of(ui);
    // A comment in the viewer's pending review on GitHub wears the pending
    // style, like a local draft comment does.
    let stroke = if thread.is_pending() {
        palette.pending
    } else {
        palette.line
    };
    thread_frame(ui, indent, stroke, |ui| {
        if collapsed {
            render_collapsed(ui, thread, vm, drafts);
            return;
        }
        render_expanded(ui, thread, vm, drafts, actions, login);
    });
}

/// The thread card (mockup `.thread`): indented `indent` from the row's left
/// edge, a 10 pt radius, a `bg` fill and a 1 pt `stroke` border, sized to
/// its content.
fn thread_frame(
    ui: &mut egui::Ui,
    indent: f32,
    stroke: egui::Color32,
    add: impl FnOnce(&mut egui::Ui),
) {
    let palette = palette_of(ui);
    egui::Frame::new()
        .outer_margin(egui::Margin {
            left: indent as i8,
            right: 14,
            top: 10,
            bottom: 10,
        })
        .inner_margin(egui::Margin::symmetric(12, 10))
        .corner_radius(10.0)
        .fill(palette.bg)
        .stroke(egui::Stroke::new(1.0, stroke))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// The one-line summary an outdated or resolved thread shows: the state,
/// the root body's first characters, and a Show button.
fn render_collapsed(
    ui: &mut egui::Ui,
    thread: &ReviewThread,
    vm: &[CommentVm],
    drafts: &mut CommentDrafts,
) {
    let palette = palette_of(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.set_height(32.0);
        if thread.is_resolved {
            let author = vm.first().map_or("someone", |c| c.author.as_str());
            ui.label(
                egui::RichText::new(icon_text(CIRCLE_CHECK, &format!("Resolved by {author}")))
                    .color(palette.ok),
            );
        } else {
            ui.label(egui::RichText::new("Outdated").color(palette.faint));
        }
        if let Some(root) = vm.first() {
            let preview: String = root
                .body_doc
                .plain_text()
                .chars()
                .take(60)
                .collect::<String>();
            ui.label(egui::RichText::new(preview).color(palette.faint));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ghost_button(ui, "Show").clicked() {
                drafts.expanded.insert(thread.id.clone());
            }
        });
    });
}

/// The full thread: bar, comments, and the reply row.
fn render_expanded(
    ui: &mut egui::Ui,
    thread: &ReviewThread,
    vm: &[CommentVm],
    drafts: &mut CommentDrafts,
    actions: &mut Vec<ThreadAction>,
    login: &str,
) {
    let palette = palette_of(ui);
    // Reply and resolve address the thread by its GraphQL node id, and an
    // optimistic thread's id is local until a refresh confirms it, so both
    // controls stay hidden until then (the prefix matches `optimistic_thread`
    // in `state::pr_detail`). A pending thread on GitHub can be neither
    // resolved nor replied to until its review is submitted.
    let pending = thread.is_pending();
    let confirmed = !thread.id.starts_with("optimistic-") && !pending;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            egui::RichText::new(icon_text(
                MESSAGES_SQUARE,
                &format!(
                    "{} comment{}",
                    vm.len(),
                    if vm.len() == 1 { "" } else { "s" }
                ),
            ))
            .color(palette.muted),
        );
        if thread.is_outdated {
            ui.label(egui::RichText::new("outdated").color(palette.faint));
        }
        if pending {
            crate::ui::components::pill(
                ui,
                Some(MESSAGE_SQUARE_DASHED),
                "Pending on GitHub",
                crate::ui::components::Tone::Pending,
            )
            .on_hover_text("Part of your pending review. Submit the review to publish it.");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ghost_button(ui, "Collapse").clicked() {
                drafts.expanded.remove(&thread.id);
            }
            if confirmed {
                let label = if thread.is_resolved {
                    "Unresolve"
                } else {
                    "Resolve"
                };
                if ghost_button(ui, label).clicked() {
                    actions.push(ThreadAction::ToggleResolved {
                        thread_id: thread.id.clone(),
                        resolved: !thread.is_resolved,
                    });
                }
            }
        });
    });
    ui.separator();

    for comment in vm {
        ui.horizontal(|ui| {
            avatar(ui, &comment.author, 28.0);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.strong(&comment.author);
                    if !comment.created_at.is_empty() {
                        ui.label(
                            egui::RichText::new(relative_time(&comment.created_at))
                                .small()
                                .color(palette.faint),
                        );
                    }
                });
                markdown::render(ui, &comment.body_doc);
                for block in &comment.body_doc.blocks {
                    if let markdown::MdBlock::Code {
                        suggestion: true,
                        text,
                        ..
                    } = block
                    {
                        if ui
                            .add(egui::Button::new(egui::RichText::new(icon_text(
                                COPY,
                                "Copy suggestion",
                            ))))
                            .clicked()
                        {
                            actions.push(ThreadAction::CopySuggestion(text.clone()));
                        }
                    }
                }
            });
        });
        ui.add_space(4.0);
    }

    // A reply anchors to the root comment's database id and carries the
    // thread's GraphQL node id; an optimistic thread's id is synthetic until
    // a refresh confirms it, so its reply box and resolve control stay
    // hidden until then.
    if !confirmed {
        return;
    }
    let Some(root_comment_id) = thread.root_comment_id() else {
        return;
    };
    let key = thread.id.clone();
    drafts.replies.entry(key.clone()).or_default();
    let mut submitted = false;
    ui.horizontal(|ui| {
        avatar(ui, login, 20.0);
        let draft = drafts
            .replies
            .get_mut(&key)
            .expect("the draft slot was just opened");
        let response = crate::ui::components::Field::new(draft)
            .hint("Reply")
            .width((ui.available_width() - 80.0).max(120.0))
            .show(ui);
        let send = crate::ui::components::Btn::new(crate::ui::components::BtnKind::Ghost, "Reply")
            .show(ui);
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (send.clicked() || enter) && !draft.trim().is_empty() {
            submitted = true;
            actions.push(ThreadAction::Reply {
                thread_id: key.clone(),
                root_comment_id,
                body: draft.trim().to_owned(),
            });
        }
    });
    if submitted {
        drafts.replies.remove(&key);
    }
}

/// Draw one pending comment: the thread frame with a `pending` stroke, a
/// "Pending" pill, and edit and delete controls (mockup `.thread.pending`).
/// Every control reports itself as a [`PendingAction`].
pub fn render_pending(
    ui: &mut egui::Ui,
    pending: &PendingComment,
    login: &str,
    indent: f32,
    actions: &mut Vec<PendingAction>,
) {
    let palette = palette_of(ui);
    thread_frame(ui, indent, palette.pending, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            avatar(ui, login, 28.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.strong(login);
                    pill(
                        ui,
                        MESSAGE_SQUARE_DASHED,
                        "Pending",
                        palette.pending,
                        palette.pending_soft,
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let delete = crate::ui::icons::icon_button(ui, TRASH_2, "Delete");
                        let edit = crate::ui::icons::icon_button(ui, PENCIL, "Edit");
                        if edit.clicked() {
                            actions.push(PendingAction::Edit {
                                local_id: pending.local_id,
                            });
                        }
                        if delete.clicked() {
                            actions.push(PendingAction::Delete {
                                local_id: pending.local_id,
                            });
                        }
                    });
                });
                markdown::render(ui, &markdown::to_doc(&pending.body));
            });
        });
    });
}

/// What the reviewer asked of a pending comment.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingAction {
    /// Re-open the composer with this pending comment's text.
    Edit {
        /// The pending comment's local id.
        local_id: u64,
    },
    /// Drop the pending comment from the draft review.
    Delete {
        /// The pending comment's local id.
        local_id: u64,
    },
}

#[cfg(test)]
mod tests {
    #[test]
    fn epoch_seconds_follow_the_calendar() {
        assert_eq!(super::parse_epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            super::parse_epoch_seconds("2026-10-17T10:00:00Z"),
            Some(1_792_231_200)
        );
        assert_eq!(
            super::parse_epoch_seconds("2000-02-29T12:30:05Z"),
            Some(951_827_405)
        );
    }

    use super::*;
    use crate::model::diff::Side;
    use crate::model::thread::{ReviewThread, SubjectType, ThreadComment};

    fn thread(path: &str, line: Option<u32>, side: Side, subject: SubjectType) -> ReviewThread {
        ReviewThread {
            id: "T".into(),
            is_resolved: false,
            is_outdated: line.is_none(),
            path: path.into(),
            line,
            start_line: None,
            side: Some(side),
            subject_type: subject,
            comments: vec![],
        }
    }

    #[test]
    fn thread_vm_orders_root_first() {
        let t = ReviewThread {
            comments: vec![
                ThreadComment {
                    pending: false,
                    database_id: 1,
                    author: "a".into(),
                    avatar_url: None,
                    body: "root".into(),
                    created_at: "1".into(),
                },
                ThreadComment {
                    pending: false,
                    database_id: 2,
                    author: "b".into(),
                    avatar_url: None,
                    body: "reply".into(),
                    created_at: "2".into(),
                },
            ],
            ..thread("a.rs", Some(1), Side::Right, SubjectType::Line)
        };
        let vm = thread_vm(&t);
        assert!(vm[0].is_root && !vm[1].is_root);
        assert_eq!(vm.len(), 2);
    }

    #[test]
    fn avatars_are_initials() {
        assert_eq!(avatar_initials("octocat"), "O");
        assert_eq!(avatar_initials("Ünknown"), "Ü");
        assert_eq!(avatar_initials(""), "?");
        assert_eq!(avatar_initials(" 🎉party"), "P", "symbols are skipped");
    }

    #[test]
    fn avatar_colours_are_stable() {
        assert_eq!(avatar_color("sercand"), avatar_color("sercand"));
        assert!(AVATAR_COLORS.contains(&avatar_color("dogacanaydin")));
        assert_eq!(
            AVATAR_COLORS[0],
            egui::Color32::from_hex("#5b7ee5").unwrap()
        );
    }

    /// The widget must paint an expanded thread — comments, resolve control,
    /// reply box — and the collapsed summary, without panicking and without
    /// inventing actions on its own.
    #[test]
    fn render_paints_expanded_and_collapsed_without_actions() {
        let t = ReviewThread {
            comments: vec![ThreadComment {
                pending: false,
                database_id: 1,
                author: "octocat".into(),
                avatar_url: Some("https://avatars/1".into()),
                body: "root with ```suggestion\nx\n``` inside".into(),
                created_at: "2026-10-01T00:00:00Z".into(),
            }],
            ..thread("a.rs", Some(1), Side::Right, SubjectType::Line)
        };
        let vm = thread_vm(&t);

        let mut drafts = CommentDrafts::default();
        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            render_thread(
                ui,
                &t,
                &vm,
                false,
                &mut drafts,
                &mut actions,
                ThreadPlacement {
                    login: "me",
                    indent: 64.0,
                },
            );
            render_thread(
                ui,
                &t,
                &vm,
                true,
                &mut drafts,
                &mut actions,
                ThreadPlacement {
                    login: "me",
                    indent: 64.0,
                },
            );
        });
        assert!(actions.is_empty(), "an untouched thread emits no actions");
        assert!(
            drafts.replies.contains_key("T"),
            "the reply box opens a draft slot"
        );
    }

    /// An optimistic thread's id is synthetic until a refresh confirms it,
    /// so it must show no reply box and no resolve control.
    #[test]
    fn an_optimistic_thread_hides_reply_and_resolve() {
        let t = ReviewThread {
            id: "optimistic-1".into(),
            comments: vec![ThreadComment {
                pending: false,
                database_id: 1,
                author: "me".into(),
                avatar_url: None,
                body: "in flight".into(),
                created_at: String::new(),
            }],
            ..thread("a.rs", Some(1), Side::Right, SubjectType::Line)
        };
        let vm = thread_vm(&t);

        let mut drafts = CommentDrafts::default();
        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            render_thread(
                ui,
                &t,
                &vm,
                false,
                &mut drafts,
                &mut actions,
                ThreadPlacement {
                    login: "me",
                    indent: 64.0,
                },
            );
        });
        assert!(actions.is_empty(), "an untouched thread emits no actions");
        assert!(
            !drafts.replies.contains_key("optimistic-1"),
            "an optimistic thread gets no reply box until a refresh confirms it"
        );
    }
}
