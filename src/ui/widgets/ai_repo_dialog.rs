//! The repo-commands dialog (AI settings Task 7): edits one repository's
//! [`RepoAiSettings`] entry — the commands agents may run, the prepare
//! commands, and the sandbox's extra paths. It moved here from the old
//! Settings "Repositories" list; the Local checkout row's edit affordance
//! opens it, and it works for a repo that has no folder linked yet.
//!
//! Like every widget here it never mutates state: Save and Cancel report a
//! [`RepoDialogAction`] for `app.rs` to apply. The four fields are the
//! dialog's own one-per-line text buffers, so a frame never re-splits the
//! saved lists. Unix-only with [`crate::ai`].

use std::path::PathBuf;

use eframe::egui::{self, Frame, Margin, RichText};

use crate::persist::settings::{AiSettings, RepoAiSettings};
use crate::ui::components::{Btn, BtnKind};
use crate::ui::{fonts, icons, theme};

/// The dialog's working copy for the entry keyed `host/owner/name`. The
/// buffers are owned by the dialog state: the saved lists are joined when
/// the dialog opens and split when it saves, never per frame.
#[derive(Clone, Debug, PartialEq)]
pub struct RepoCommandsDialog {
    /// The repository key (`host/owner/name`) whose entry is edited.
    pub repo: String,
    /// One command per line: what agents may run in the sandbox.
    pub commands: String,
    /// One command per line: what runs outside the sandbox before a run.
    pub prepare: String,
    /// One path per line: extra paths the sandbox may write.
    pub extra_write_paths: String,
    /// One path per line: extra paths the sandbox may not read.
    pub extra_deny_read: String,
}

impl RepoCommandsDialog {
    /// Open the dialog for `repo_key`: the entry's lists, or empty buffers
    /// when the repo has no entry yet — commands are editable before the
    /// folder is linked.
    pub fn open(repo_key: &str, entry: Option<&RepoAiSettings>) -> RepoCommandsDialog {
        RepoCommandsDialog {
            repo: repo_key.to_string(),
            commands: entry.map_or_else(String::new, |entry| entry.commands.join("\n")),
            prepare: entry.map_or_else(String::new, |entry| entry.prepare.join("\n")),
            extra_write_paths: entry
                .map_or_else(String::new, |entry| join_paths(&entry.extra_write_paths)),
            extra_deny_read: entry
                .map_or_else(String::new, |entry| join_paths(&entry.extra_deny_read)),
        }
    }

    /// The four lists the buffers describe: trimmed lines, blanks dropped.
    pub fn lists(&self) -> RepoCommands {
        RepoCommands {
            commands: split_lines(&self.commands),
            prepare: split_lines(&self.prepare),
            extra_write_paths: split_lines(&self.extra_write_paths)
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            extra_deny_read: split_lines(&self.extra_deny_read)
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        }
    }
}

/// The four lists the dialog edits, as the settings entry holds them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoCommands {
    /// Commands agents may run in the sandbox.
    pub commands: Vec<String>,
    /// Commands run outside the sandbox before a run.
    pub prepare: Vec<String>,
    /// Extra paths the sandbox may write.
    pub extra_write_paths: Vec<PathBuf>,
    /// Extra paths the sandbox may not read.
    pub extra_deny_read: Vec<PathBuf>,
}

/// Split a one-per-line buffer: each line trimmed, blank lines dropped.
pub fn split_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Patch (or create) the entry for `repo` with the dialog's lists. The
/// entry's `path` survives untouched — the dialog edits commands, not the
/// folder link — and a repo with no entry gets a fresh row with no path,
/// which is simply not listed as linked until a folder is chosen. The key
/// is stored lowercase and matched case-insensitively, like
/// [`link_repo`](crate::ai::repo_link::link_repo).
pub fn put_repo_commands(ai: &mut AiSettings, repo: &str, lists: &RepoCommands) {
    let key = repo.to_lowercase();
    match ai
        .repos
        .iter_mut()
        .find(|entry| entry.repo.to_lowercase() == key)
    {
        Some(entry) => apply(entry, lists),
        None => {
            let mut entry = RepoAiSettings {
                repo: key,
                ..Default::default()
            };
            apply(&mut entry, lists);
            ai.repos.push(entry);
        }
    }

    fn apply(entry: &mut RepoAiSettings, lists: &RepoCommands) {
        entry.commands = lists.commands.clone();
        entry.prepare = lists.prepare.clone();
        entry.extra_write_paths = lists.extra_write_paths.clone();
        entry.extra_deny_read = lists.extra_deny_read.clone();
    }
}

/// A control the dialog reported.
#[derive(Clone, Debug, PartialEq)]
pub enum RepoDialogAction {
    /// Commit the buffers to the repo's entry; the app reads them from the
    /// dialog state it owns.
    Save,
    /// Close without saving.
    Cancel,
}

/// Paint the dialog; `ctx` hosts the window the way the other AI modals
/// do. The X reports [`RepoDialogAction::Cancel`]; `Esc` is the app's
/// `dispatch_close`, which reports the same action.
pub fn render(
    ctx: &egui::Context,
    dialog: &mut RepoCommandsDialog,
    actions: &mut Vec<RepoDialogAction>,
) {
    let p = theme::palette(ctx.theme() == egui::Theme::Dark);
    let slug = dialog
        .repo
        .split_once('/')
        .map_or(dialog.repo.as_str(), |(_, slug)| slug);
    let title = format!("Commands for {slug}");
    let mut close = false;
    let mut save = false;
    egui::Window::new(title.clone())
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .frame(
            Frame::new()
                .fill(p.surface)
                .corner_radius(10.0)
                .stroke((1.0, p.line))
                .inner_margin(Margin::symmetric(16, 12)),
        )
        .show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&title).font(fonts::semibold(14.0)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icons::icon_button(ui, icons::X, "Close  Esc").clicked() {
                        close = true;
                    }
                });
            });
            ui.label(
                RichText::new(format!(
                    "{} — kept when the folder is unlinked.",
                    dialog.repo
                ))
                .color(p.faint)
                .size(11.5),
            );

            field_label(ui, "Commands agents may run (one per line)");
            text_area(ui, &mut dialog.commands, "cargo test", 3);
            field_label(
                ui,
                "Prepare commands, run outside the sandbox (one per line)",
            );
            text_area(ui, &mut dialog.prepare, "cargo fetch", 2);
            field_label(ui, "Extra writable paths (one per line)");
            text_area(ui, &mut dialog.extra_write_paths, "~/.cargo", 2);
            field_label(ui, "Extra unreadable paths (one per line)");
            text_area(ui, &mut dialog.extra_deny_read, "~/.ssh", 2);

            ui.horizontal(|ui| {
                if Btn::new(BtnKind::Primary, "Save").show(ui).clicked() {
                    save = true;
                }
                if Btn::new(BtnKind::Default, "Cancel").show(ui).clicked() {
                    close = true;
                }
            });
        });
    if close {
        actions.push(RepoDialogAction::Cancel);
    }
    if save {
        actions.push(RepoDialogAction::Save);
    }
}

/// A small field heading above a text area.
fn field_label(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette_of(ui);
    ui.label(RichText::new(text).color(p.muted).size(11.5));
}

/// One framed multi-line buffer.
fn text_area(ui: &mut egui::Ui, text: &mut String, hint: &str, rows: usize) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.bg)
        .corner_radius(6.0)
        .stroke((1.0, p.line))
        .inner_margin(Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(text)
                    .hint_text(RichText::new(hint).color(p.faint))
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(rows),
            );
        });
}

/// Paths joined one per line, the buffer form of a path list.
fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> RepoAiSettings {
        RepoAiSettings {
            repo: "github.com/acme/widgets".to_string(),
            path: PathBuf::from("/src/widgets"),
            commands: vec!["cargo test".to_string(), "cargo clippy".to_string()],
            prepare: vec!["cargo fetch".to_string()],
            extra_write_paths: vec![PathBuf::from("/tmp/build")],
            extra_deny_read: vec![PathBuf::from("/secrets")],
        }
    }

    /// Opening the dialog joins the entry's lists into the buffers, one
    /// line each; a repo with no entry opens blank.
    #[test]
    fn open_seeds_the_buffers_from_the_entry() {
        let dialog = RepoCommandsDialog::open("github.com/acme/widgets", Some(&entry()));
        assert_eq!(dialog.repo, "github.com/acme/widgets");
        assert_eq!(dialog.commands, "cargo test\ncargo clippy");
        assert_eq!(dialog.prepare, "cargo fetch");
        assert_eq!(dialog.extra_write_paths, "/tmp/build");
        assert_eq!(dialog.extra_deny_read, "/secrets");

        let blank = RepoCommandsDialog::open("github.com/acme/api", None);
        assert!(blank.commands.is_empty() && blank.prepare.is_empty());
        assert!(blank.extra_write_paths.is_empty() && blank.extra_deny_read.is_empty());
    }

    /// Blank and padded lines are dropped; the rest survive trimmed.
    #[test]
    fn split_lines_trims_and_drops_empties() {
        assert_eq!(
            split_lines("  cargo test\n\n  cargo clippy  \n \n"),
            vec!["cargo test".to_string(), "cargo clippy".to_string()]
        );
    }

    /// Saving patches the lists onto the existing entry and keeps its
    /// folder — the dialog edits commands, not the link.
    #[test]
    fn save_patches_the_entry_and_keeps_the_path() {
        let mut ai = AiSettings {
            repos: vec![entry()],
            ..Default::default()
        };
        let lists = RepoCommands {
            commands: vec!["npm test".to_string()],
            prepare: Vec::new(),
            extra_write_paths: vec![PathBuf::from("/tmp/x")],
            extra_deny_read: Vec::new(),
        };
        put_repo_commands(&mut ai, "GitHub.com/Acme/Widgets", &lists);
        assert_eq!(ai.repos.len(), 1, "the row is updated in place");
        assert_eq!(ai.repos[0].path, PathBuf::from("/src/widgets"));
        assert_eq!(ai.repos[0].commands, vec!["npm test".to_string()]);
        assert!(ai.repos[0].prepare.is_empty());
        assert_eq!(ai.repos[0].extra_write_paths, vec![PathBuf::from("/tmp/x")]);
        assert!(ai.repos[0].extra_deny_read.is_empty());
    }

    /// A repo with no entry — or an entry whose path was cleared — gets its
    /// commands saved anyway: they are editable before the folder is
    /// linked, and unlinking keeps them.
    #[test]
    fn save_creates_an_entry_for_a_repo_without_one() {
        let mut ai = AiSettings::default();
        let lists = RepoCommands {
            commands: vec!["make check".to_string()],
            prepare: Vec::new(),
            extra_write_paths: Vec::new(),
            extra_deny_read: Vec::new(),
        };
        put_repo_commands(&mut ai, "github.com/acme/api", &lists);
        assert_eq!(ai.repos.len(), 1);
        assert_eq!(ai.repos[0].repo, "github.com/acme/api");
        assert!(ai.repos[0].path.as_os_str().is_empty(), "not linked yet");
        assert_eq!(ai.repos[0].commands, vec!["make check".to_string()]);

        // An unlinked entry (empty path) is patched, not duplicated.
        put_repo_commands(&mut ai, "github.com/acme/api", &RepoCommands::default());
        assert_eq!(ai.repos.len(), 1);
        assert!(ai.repos[0].commands.is_empty());
    }

    /// The dialog paints over a bare context and reports nothing until the
    /// reviewer answers it; Save and Cancel report their actions.
    #[test]
    fn the_dialog_paints_and_reports_only_when_answered() {
        use egui_kittest::kittest::Queryable;

        let paint_once = |actions: &mut Vec<RepoDialogAction>| {
            let mut dialog = RepoCommandsDialog::open("github.com/acme/widgets", Some(&entry()));
            egui::__run_test_ui(|ui| {
                render(ui.ctx(), &mut dialog, actions);
            });
        };
        let mut actions = Vec::new();
        paint_once(&mut actions);
        assert!(actions.is_empty(), "painting alone answers nothing");

        for (label, want) in [
            ("Save", RepoDialogAction::Save),
            ("Cancel", RepoDialogAction::Cancel),
        ] {
            let mut harness = egui_kittest::Harness::builder()
                .with_size([700.0, 700.0])
                .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
                .build_ui_state(
                    move |ui, actions: &mut Vec<RepoDialogAction>| {
                        let mut dialog =
                            RepoCommandsDialog::open("github.com/acme/widgets", Some(&entry()));
                        render(ui.ctx(), &mut dialog, actions);
                    },
                    Vec::new(),
                );
            harness.get_by_label(label).click();
            harness.run();
            assert_eq!(*harness.state(), vec![want], "the {label} button");
        }
    }
}
