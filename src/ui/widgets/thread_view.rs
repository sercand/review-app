//! Drawing one review thread inside the diff surface (spec §7.4).
//!
//! [`thread_vm`] is the pure view-model — comments with their markdown
//! parsed, the root flagged first — and [`render_thread`] turns it into
//! widgets. Outdated and resolved threads render collapsed to a summary
//! line; the reviewer expands them from there. Avatars are initials in a
//! circle: the build ships no image loaders (spec §5), so nothing is ever
//! fetched.

use std::collections::{HashMap, HashSet};

use eframe::egui;

use crate::model::thread::ReviewThread;
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
) {
    egui::Frame::new()
        .inner_margin(8.0)
        .corner_radius(6.0)
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("thread", thread.id.as_str()))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if collapsed {
                        render_collapsed(ui, thread, vm, drafts);
                        return;
                    }
                    render_expanded(ui, thread, vm, drafts, actions);
                });
        });
}

/// The one-line summary an outdated or resolved thread shows.
fn render_collapsed(
    ui: &mut egui::Ui,
    thread: &ReviewThread,
    vm: &[CommentVm],
    drafts: &mut CommentDrafts,
) {
    ui.horizontal(|ui| {
        if thread.is_resolved {
            ui.label(egui::RichText::new("Resolved").strong());
        }
        if thread.is_outdated {
            ui.label(egui::RichText::new("Outdated").weak());
        }
        let author = vm.first().map_or("someone", |c| c.author.as_str());
        ui.label(format!(
            "{author} · {} comment{}",
            vm.len(),
            if vm.len() == 1 { "" } else { "s" }
        ));
        if ui.button("Show thread").clicked() {
            drafts.expanded.insert(thread.id.clone());
        }
    });
}

/// The full thread: comments, resolve control, and the reply box.
fn render_expanded(
    ui: &mut egui::Ui,
    thread: &ReviewThread,
    vm: &[CommentVm],
    drafts: &mut CommentDrafts,
    actions: &mut Vec<ThreadAction>,
) {
    ui.horizontal(|ui| {
        let label = if thread.is_resolved {
            "Unresolve"
        } else {
            "Resolve"
        };
        if ui.button(label).clicked() {
            actions.push(ThreadAction::ToggleResolved {
                thread_id: thread.id.clone(),
                resolved: !thread.is_resolved,
            });
        }
        if thread.is_outdated {
            ui.label(egui::RichText::new("outdated").weak());
        }
        if ui.button("Collapse").clicked() {
            drafts.expanded.remove(&thread.id);
        }
    });
    ui.separator();

    for comment in vm {
        ui.horizontal(|ui| {
            avatar(ui, &comment.author);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(&comment.author);
                    if !comment.created_at.is_empty() {
                        ui.weak(&comment.created_at);
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
                        if ui.button("Copy suggestion").clicked() {
                            actions.push(ThreadAction::CopySuggestion(text.clone()));
                        }
                    }
                }
            });
        });
        ui.add_space(4.0);
    }

    // A reply anchors to the root comment's database id; an optimistic
    // thread has one, so its reply box works too.
    let Some(root_comment_id) = thread.root_comment_id() else {
        return;
    };
    let key = thread.id.clone();
    let draft = drafts.replies.entry(key.clone()).or_default();
    let response = ui.add(
        egui::TextEdit::singleline(draft)
            .hint_text("Reply…")
            .desired_width(280.0),
    );
    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let clicked = ui
        .add_enabled_ui(!draft.trim().is_empty(), |ui| ui.button("Reply").clicked())
        .inner;
    if (clicked || enter) && !draft.trim().is_empty() {
        actions.push(ThreadAction::Reply {
            thread_id: key.clone(),
            root_comment_id,
            body: draft.trim().to_owned(),
        });
        drafts.replies.remove(&key);
    }
}

/// The initials avatar: a filled circle with one letter.
fn avatar(ui: &mut egui::Ui, author: &str) {
    let size = 24.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let fill = ui.visuals().faint_bg_color;
    let text = ui.visuals().strong_text_color();
    ui.painter().circle_filled(rect.center(), size / 2.0, fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        avatar_initials(author),
        egui::FontId::proportional(11.0),
        text,
    );
}

#[cfg(test)]
mod tests {
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
                    database_id: 1,
                    author: "a".into(),
                    avatar_url: None,
                    body: "root".into(),
                    created_at: "1".into(),
                },
                ThreadComment {
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

    /// The widget must paint an expanded thread — comments, resolve control,
    /// reply box — and the collapsed summary, without panicking and without
    /// inventing actions on its own.
    #[test]
    fn render_paints_expanded_and_collapsed_without_actions() {
        let t = ReviewThread {
            comments: vec![ThreadComment {
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
            render_thread(ui, &t, &vm, false, &mut drafts, &mut actions);
            render_thread(ui, &t, &vm, true, &mut drafts, &mut actions);
        });
        assert!(actions.is_empty(), "an untouched thread emits no actions");
        assert!(
            drafts.replies.contains_key("T"),
            "the reply box opens a draft slot"
        );
    }
}
