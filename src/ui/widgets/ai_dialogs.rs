//! The AI review dialogs (spec §4 step 2, §7.3): the folder pick, the
//! checkout confirmation, the restore decision, crash recovery, and the
//! repo-held notice.
//!
//! Like every widget here these never mutate state: each control reports an
//! [`AiDialogAction`] for `app.rs` to apply, and the folder picker itself is
//! the app's — the widget only asks for it and receives the chosen path.
//! Unix-only with [`crate::ai`].

use std::path::{Path, PathBuf};

use eframe::egui::{self, Frame, Margin, RichText};

use crate::ai::checkout::{CheckoutPlan, PrTarget, RemoteRepo, find_remote};
use crate::ai::repo_link::link_repo;
use crate::ai::store::{CheckoutRecord, OriginalRef, PrKey};
use crate::persist::settings::AiSettings;
use crate::ui::components::{Btn, BtnKind};
use crate::ui::{fonts, icons, theme};

/// The one AI dialog the app shows at a time (spec §4, §7.3).
#[derive(Clone, Debug, PartialEq)]
pub enum AiDialog {
    /// No folder is configured for this repo; ask for one (spec §4 step 2).
    PickFolder {
        /// The PR whose repository needs a folder.
        key: PrKey,
    },
    /// The planned checkout, awaiting the user's confirmation (spec §4 step 2).
    ConfirmCheckout(CheckoutPlan),
    /// Finish found changes the review made; discard or leave as is (§7.3).
    Restore {
        /// The `git status --porcelain` lines for the changes.
        lines: Vec<String>,
    },
    /// Checkouts a previous session never restored (spec §7.3).
    Recovery(Vec<CheckoutRecord>),
    /// Another PR still holds this repository's checkout (spec §4 step 7).
    Held {
        /// The holding PR's number.
        pr_number: u64,
        /// The holding record's id when its review has no live run to
        /// finish; the dialog then offers to restore it right here instead
        /// of pointing at a Finish that would toast "no review is running".
        restore: Option<i64>,
    },
}

/// A control one of the dialogs reported.
#[derive(Clone, Debug, PartialEq)]
pub enum AiDialogAction {
    /// Open the OS folder picker (the pick dialog's button).
    PickFolder,
    /// The folder picker answered with a path; a cancelled picker sends
    /// nothing and the dialog stays open.
    FolderChosen(PathBuf),
    /// Run the confirmed checkout plan (spec §4 step 3).
    ConfirmCheckout,
    /// Abandon the checkout attempt; also dismisses the held notice.
    CancelCheckout,
    /// Discard the review's changes and restore (spec §7.3 step 1).
    RestoreDiscard,
    /// Stop: leave the checkout exactly as it is (spec §7.3 step 1).
    RestoreKeep,
    /// Restore one unrestored checkout, discarding stray changes (§7.3).
    RecoverRestore(i64),
    /// Close the recovery dialog; the checkouts stay as they are.
    RecoverLater,
}

/// Why `path` cannot serve as the checkout folder for `key`, or `None` when
/// it can: a pure wrapper over [`find_remote`] returning the message the
/// pick dialog shows when the folder is not a clone of the PR's repository
/// (spec §4 step 2).
pub fn folder_error(path: &Path, key: &PrKey) -> Option<String> {
    find_remote(path, &key.host, &key.owner, &key.repo)
        .err()
        .map(|error| error.to_string())
}

/// The display text for a recorded original ref: the branch name, or the
/// detached SHA's short form.
pub fn original_ref_label(original: &OriginalRef) -> String {
    match original {
        OriginalRef::Branch(name) => name.clone(),
        OriginalRef::Detached(sha) => short_sha(sha),
    }
}

/// The first seven characters of a SHA, the usual short form.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Save `path` as the checkout folder for `key` (spec §4 step 2): a re-pick
/// moves the folder without losing the commands the user configured or
/// accepted from the scout, and a new repo gets a fresh row with no commands.
/// The link itself is [`link_repo`]'s; this adapts the PR key to it.
pub fn save_folder(ai: &mut AiSettings, key: &PrKey, path: PathBuf) {
    let repo = RemoteRepo {
        host: key.host.clone(),
        owner: key.owner.clone(),
        name: key.repo.clone(),
    };
    link_repo(ai, &repo, path);
}

/// Window-level context the dialogs display but do not own.
pub struct DialogInputs<'a> {
    /// The PR the dialog is about, for the confirm dialog's target line.
    pub pr: Option<&'a PrTarget>,
    /// The repo's configured prepare commands (spec §8.5), listed in the
    /// confirm dialog beside the git commands.
    pub prepare: &'a [String],
    /// The folder validation error to show in the pick dialog.
    pub error: Option<&'a str>,
}

/// Paint the dialog over every screen; `ctx` hosts the window the way the
/// settings window and palette do. The frame, header and width follow those
/// modals. The X reports [`dismiss_action`]; `Esc` is the app's
/// `dispatch_close`, which reports the same action — one owner, so the key
/// can never dismiss the dialog *and* leave the PR behind it.
pub fn render(
    ctx: &egui::Context,
    dialog: &AiDialog,
    inputs: &DialogInputs,
    actions: &mut Vec<AiDialogAction>,
) {
    let p = theme::palette(ctx.theme() == egui::Theme::Dark);
    let (title, width, dismiss) = dialog_meta(dialog, inputs);
    let mut closed = false;
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
            ui.set_width(width);
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            closed = header(ui, &title, dismiss.is_some());
            match dialog {
                AiDialog::PickFolder { key } => pick_folder(ui, key, inputs, actions),
                AiDialog::ConfirmCheckout(plan) => confirm_checkout(ui, plan, inputs, actions),
                AiDialog::Restore { lines } => restore(ui, lines, actions),
                AiDialog::Recovery(records) => recovery(ui, records, actions),
                AiDialog::Held { pr_number, restore } => held(ui, *pr_number, *restore, actions),
            }
        });
    // The X reports the same answer Esc does; Esc itself is the app's
    // `dispatch_close`, which reads [`dismiss_action`] — one owner for the
    // key, so it cannot both dismiss the dialog and leave the PR.
    if let Some(action) = dismiss
        && closed
    {
        actions.push(action);
    }
}

/// What the X (and `Esc`, via the app's close chain) does to `dialog`: its
/// non-destructive answer, or `None` when the user must answer it — the
/// restore decision cannot be silently dropped.
pub fn dismiss_action(dialog: &AiDialog) -> Option<AiDialogAction> {
    match dialog {
        AiDialog::PickFolder { .. } | AiDialog::ConfirmCheckout(_) | AiDialog::Held { .. } => {
            Some(AiDialogAction::CancelCheckout)
        }
        AiDialog::Restore { .. } => None,
        AiDialog::Recovery(_) => Some(AiDialogAction::RecoverLater),
    }
}

/// Whether a dialog outlives the PR screen it opened over: only the app-wide
/// recovery list does. The rest are PR-scoped, and a flow dialog left
/// floating over the list could act on the session the user just left
/// (Task 4 review finding 1).
pub fn survives_navigation(dialog: &AiDialog) -> bool {
    matches!(dialog, AiDialog::Recovery(_))
}

/// A dialog's title, width, and what its X/Esc means (`None`: it must be
/// answered).
fn dialog_meta(dialog: &AiDialog, inputs: &DialogInputs) -> (String, f32, Option<AiDialogAction>) {
    let dismiss = dismiss_action(dialog);
    match dialog {
        AiDialog::PickFolder { .. } => ("Pick the repository folder".to_string(), 440.0, dismiss),
        AiDialog::ConfirmCheckout(_) => (checkout_title(inputs.pr), 500.0, dismiss),
        AiDialog::Restore { .. } => ("Changes made during the review".to_string(), 500.0, dismiss),
        AiDialog::Recovery(_) => ("Unrestored AI checkouts".to_string(), 580.0, dismiss),
        AiDialog::Held { .. } => ("Repository in use".to_string(), 400.0, dismiss),
    }
}

/// The confirm dialog's title: the PR's number once the app has metadata.
fn checkout_title(pr: Option<&PrTarget>) -> String {
    match pr {
        Some(pr) => format!("Check out PR #{}?", pr.number),
        None => "Check out this pull request?".to_string(),
    }
}

/// The modal's title row, with the X when the dialog can be dismissed.
/// Returns whether the X was clicked.
fn header(ui: &mut egui::Ui, title: &str, dismissible: bool) -> bool {
    let mut closed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(fonts::semibold(14.0)));
        if dismissible {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icons::icon_button(ui, icons::X, "Close  Esc").clicked() {
                    closed = true;
                }
            });
        }
    });
    closed
}

/// No folder is configured for this repo: ask for one (spec §4 step 2).
fn pick_folder(
    ui: &mut egui::Ui,
    key: &PrKey,
    inputs: &DialogInputs,
    actions: &mut Vec<AiDialogAction>,
) {
    ui.label(format!(
        "Which folder holds your local clone of {}/{}?",
        key.owner, key.repo
    ));
    ui.weak("The app checks that one of its git remotes points at the pull request's repository.");
    if let Some(error) = inputs.error {
        let p = theme::palette_of(ui);
        ui.colored_label(p.bad, error);
    }
    ui.horizontal(|ui| {
        if Btn::new(BtnKind::Primary, "Choose folder…")
            .show(ui)
            .clicked()
        {
            actions.push(AiDialogAction::PickFolder);
        }
        if Btn::new(BtnKind::Default, "Cancel").show(ui).clicked() {
            actions.push(AiDialogAction::CancelCheckout);
        }
    });
}

/// The plan awaiting confirmation: where the tree goes, what gets stashed,
/// and the exact commands (spec §4 step 2, §7.1).
fn confirm_checkout(
    ui: &mut egui::Ui,
    plan: &CheckoutPlan,
    inputs: &DialogInputs,
    actions: &mut Vec<AiDialogAction>,
) {
    let p = theme::palette_of(ui);
    if let Some(pr) = inputs.pr {
        ui.label(format!(
            "On {} → {} @ {}",
            original_ref_label(&plan.original),
            pr.head_branch,
            short_sha(&pr.head_sha),
        ));
    }
    ui.weak(plan.repo_path.display().to_string());
    if plan.dirty && !plan.skip_stash {
        ui.colored_label(
            p.pending,
            "Your uncommitted changes will be stashed and restored afterwards.",
        );
    }
    ui.label("The app will run:");
    for command in &plan.commands {
        mono_line(ui, command);
    }
    if !inputs.prepare.is_empty() {
        ui.label("Then the repo's prepare commands:");
        for command in inputs.prepare {
            mono_line(ui, command);
        }
    }
    ui.horizontal(|ui| {
        if Btn::new(BtnKind::Primary, "Check out").show(ui).clicked() {
            actions.push(AiDialogAction::ConfirmCheckout);
        }
        if Btn::new(BtnKind::Default, "Cancel").show(ui).clicked() {
            actions.push(AiDialogAction::CancelCheckout);
        }
    });
}

/// The finish found changes the review made: discard them, or stop and
/// leave the checkout as is (spec §7.3 step 1).
fn restore(ui: &mut egui::Ui, lines: &[String], actions: &mut Vec<AiDialogAction>) {
    ui.label("The review left these changes in the checkout:");
    for line in lines {
        mono_line(ui, line);
    }
    ui.label(
        "Discard them and return to your original ref, or stop and \
         leave the checkout as it is.",
    );
    ui.horizontal(|ui| {
        if Btn::new(BtnKind::Primary, "Discard and restore")
            .show(ui)
            .clicked()
        {
            actions.push(AiDialogAction::RestoreDiscard);
        }
        if Btn::new(BtnKind::Default, "Leave as is").show(ui).clicked() {
            actions.push(AiDialogAction::RestoreKeep);
        }
    });
}

/// Checkouts a previous session never restored (spec §7.3): restore each,
/// or leave them all for later.
fn recovery(ui: &mut egui::Ui, records: &[CheckoutRecord], actions: &mut Vec<AiDialogAction>) {
    ui.label(
        "A previous session left these checkouts unrestored. Restoring \
         returns each repository to the ref it was on and re-applies \
         its stash; changes made since are discarded.",
    );
    for record in records {
        ui.horizontal(|ui| {
            ui.label(format!(
                "PR #{} · {} · was on {}",
                record.pr_number,
                record.repo_path.display(),
                original_ref_label(&record.original),
            ));
            if Btn::new(BtnKind::Default, "Restore").show(ui).clicked() {
                actions.push(AiDialogAction::RecoverRestore(record.id));
            }
        });
    }
    if Btn::new(BtnKind::Ghost, "Later").show(ui).clicked() {
        actions.push(AiDialogAction::RecoverLater);
    }
}

/// Another PR holds this repository's checkout (spec §4 step 7). When its
/// review has no live run, the dialog offers the restore the old advice
/// ("finish that review") could not deliver — the run is gone, so only a
/// restore unlocks the repo.
fn held(
    ui: &mut egui::Ui,
    pr_number: u64,
    restore: Option<i64>,
    actions: &mut Vec<AiDialogAction>,
) {
    match restore {
        Some(id) => {
            ui.label(format!(
                "PR #{pr_number} still holds this repository's checkout, but its review is \
                 over. Restore that checkout to start a review here."
            ));
            ui.horizontal(|ui| {
                if Btn::new(BtnKind::Primary, "Restore it now")
                    .show(ui)
                    .clicked()
                {
                    actions.push(AiDialogAction::RecoverRestore(id));
                }
                if Btn::new(BtnKind::Ghost, "Not now").show(ui).clicked() {
                    actions.push(AiDialogAction::CancelCheckout);
                }
            });
        }
        None => {
            ui.label(format!(
                "PR #{pr_number} is still checked out in this repository. \
                 Finish that review before starting one here."
            ));
            if Btn::new(BtnKind::Primary, "OK").show(ui).clicked() {
                actions.push(AiDialogAction::CancelCheckout);
            }
        }
    }
}

/// A mono command line inside a dialog.
fn mono_line(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.surface)
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).monospace().size(11.5).color(p.text));
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::store::CheckoutState;
    use crate::test_support::tempdir;
    use std::process::Command;

    fn gitx(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git on PATH");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn key() -> PrKey {
        PrKey {
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            repo: "app".to_string(),
            number: 7,
        }
    }

    #[test]
    fn folder_error_for_non_repo_and_wrong_remote() {
        // A plain directory: not a git repository at all.
        let plain = tempdir();
        let error =
            folder_error(plain.path(), &key()).expect("a plain folder is not a checkout folder");
        assert!(error.contains("github.com/acme/app"), "{error}");

        // A repository whose remote points at another project.
        let other = tempdir();
        gitx(other.path(), &["init", "-b", "main"]);
        gitx(
            other.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/app.git",
            ],
        );
        let error = folder_error(other.path(), &key()).expect("a fork's clone is not the repo");
        assert!(error.contains("github.com/acme/app"), "{error}");

        // The right repository under an scp-form remote: no error.
        let right = tempdir();
        gitx(right.path(), &["init", "-b", "main"]);
        gitx(
            right.path(),
            &["remote", "add", "origin", "git@github.com:acme/app.git"],
        );
        assert_eq!(folder_error(right.path(), &key()), None);
    }

    #[test]
    fn original_ref_label_shows_branch_or_short_sha() {
        assert_eq!(
            original_ref_label(&OriginalRef::Branch("main".to_string())),
            "main"
        );
        assert_eq!(
            original_ref_label(&OriginalRef::Detached("0123456789abcdef".to_string())),
            "0123456"
        );
    }

    /// Picking a folder for a repo that already has one changes the folder
    /// only: the commands the user configured (or accepted from the scout)
    /// survive a re-pick, and a new repo gets a fresh entry (Task 4).
    #[test]
    fn picking_a_folder_keeps_the_repos_commands() {
        use crate::persist::settings::{AiSettings, RepoAiSettings};

        let mut ai = AiSettings {
            repos: vec![RepoAiSettings {
                repo: "github.com/acme/app".to_string(),
                path: PathBuf::from("/old"),
                commands: vec!["cargo test".to_string()],
                prepare: vec!["cargo fetch".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };
        save_folder(&mut ai, &key(), PathBuf::from("/new"));
        assert_eq!(ai.repos.len(), 1, "the existing row is updated in place");
        assert_eq!(ai.repos[0].path, PathBuf::from("/new"));
        assert_eq!(ai.repos[0].commands, vec!["cargo test".to_string()]);
        assert_eq!(ai.repos[0].prepare, vec!["cargo fetch".to_string()]);

        let other = PrKey {
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            repo: "other".to_string(),
            number: 8,
        };
        save_folder(&mut ai, &other, PathBuf::from("/other"));
        assert_eq!(ai.repos.len(), 2, "a new repo gets its own row");
        assert_eq!(ai.repos[1].repo, "github.com/acme/other");
        assert_eq!(ai.repos[1].path, PathBuf::from("/other"));
        assert!(ai.repos[1].commands.is_empty(), "nothing to keep yet");
    }

    /// The confirm dialog names the PR once metadata is in, and the X/Esc
    /// of each dialog resolves to its non-destructive answer — except the
    /// restore decision, which the user must make.
    #[test]
    fn dialog_titles_and_dismissals() {
        let inputs = DialogInputs {
            pr: None,
            prepare: &[],
            error: None,
        };
        assert_eq!(
            checkout_title(None),
            "Check out this pull request?",
            "no metadata yet"
        );
        let pr = PrTarget {
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            name: "app".to_string(),
            number: 7,
            head_sha: "0123456789abcdef".to_string(),
            head_branch: "feature/x".to_string(),
        };
        assert_eq!(checkout_title(Some(&pr)), "Check out PR #7?");

        let plan = CheckoutPlan {
            repo_path: PathBuf::from("/repo"),
            remote: "origin".to_string(),
            original: OriginalRef::Branch("main".to_string()),
            dirty: false,
            skip_stash: false,
            commands: Vec::new(),
        };
        assert_eq!(
            dialog_meta(&AiDialog::PickFolder { key: key() }, &inputs).2,
            Some(AiDialogAction::CancelCheckout)
        );
        assert_eq!(
            dialog_meta(&AiDialog::ConfirmCheckout(plan), &inputs).2,
            Some(AiDialogAction::CancelCheckout)
        );
        assert_eq!(
            dialog_meta(&AiDialog::Recovery(Vec::new()), &inputs).2,
            Some(AiDialogAction::RecoverLater),
            "closing the recovery list leaves the checkouts as they are"
        );
        assert_eq!(
            dialog_meta(
                &AiDialog::Held {
                    pr_number: 9,
                    restore: None,
                },
                &inputs
            )
            .2,
            Some(AiDialogAction::CancelCheckout)
        );
        assert_eq!(
            dialog_meta(
                &AiDialog::Restore {
                    lines: vec!["?? stray.txt".to_string()],
                },
                &inputs
            )
            .2,
            None,
            "the restore decision has no dismiss: it must be answered"
        );
    }

    /// Every dialog must paint over a bare context without reporting an
    /// action until the reviewer answers it.
    #[test]
    fn the_dialogs_paint_and_report_nothing_until_clicked() {
        let plan = CheckoutPlan {
            repo_path: PathBuf::from("/repo"),
            remote: "origin".to_string(),
            original: OriginalRef::Branch("main".to_string()),
            dirty: true,
            skip_stash: false,
            commands: vec!["git fetch origin pull/7/head".to_string()],
        };
        let pr = PrTarget {
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            name: "app".to_string(),
            number: 7,
            head_sha: "0123456789abcdef".to_string(),
            head_branch: "feature/x".to_string(),
        };
        let record = CheckoutRecord {
            id: 3,
            repo_path: PathBuf::from("/repo"),
            repo: "acme/app".to_string(),
            pr_number: 7,
            original: OriginalRef::Branch("main".to_string()),
            stash_sha: None,
            head_sha: "deadbeef".to_string(),
            state: CheckoutState::Active,
        };
        let prepare = vec!["cargo fetch".to_string()];
        for dialog in [
            AiDialog::PickFolder { key: key() },
            AiDialog::ConfirmCheckout(plan),
            AiDialog::Restore {
                lines: vec!["?? stray.txt".to_string()],
            },
            AiDialog::Recovery(vec![record]),
            AiDialog::Held {
                pr_number: 9,
                restore: None,
            },
            AiDialog::Held {
                pr_number: 9,
                restore: Some(3),
            },
        ] {
            let mut actions = Vec::new();
            let inputs = DialogInputs {
                pr: Some(&pr),
                prepare: &prepare,
                error: Some("no remote in this repository points at github.com/acme/app"),
            };
            egui::__run_test_ui(|ui| {
                render(ui.ctx(), &dialog, &inputs, &mut actions);
            });
            assert!(actions.is_empty(), "{dialog:?} reported {actions:?}");
        }
    }

    /// `Esc` (through the app's close chain) resolves to each dialog's safe
    /// answer, and the restore decision has none: it must be answered
    /// (Task 4 review finding 1).
    #[test]
    fn escaping_a_dialog_maps_to_its_safe_action() {
        let plan = CheckoutPlan {
            repo_path: PathBuf::from("/repo"),
            remote: "origin".to_string(),
            original: OriginalRef::Branch("main".to_string()),
            dirty: false,
            skip_stash: false,
            commands: Vec::new(),
        };
        assert_eq!(
            dismiss_action(&AiDialog::PickFolder { key: key() }),
            Some(AiDialogAction::CancelCheckout)
        );
        assert_eq!(
            dismiss_action(&AiDialog::ConfirmCheckout(plan)),
            Some(AiDialogAction::CancelCheckout)
        );
        assert_eq!(
            dismiss_action(&AiDialog::Held {
                pr_number: 9,
                restore: Some(3),
            }),
            Some(AiDialogAction::CancelCheckout),
            "the held notice's safe answer never restores by accident"
        );
        assert_eq!(
            dismiss_action(&AiDialog::Recovery(Vec::new())),
            Some(AiDialogAction::RecoverLater)
        );
        assert_eq!(
            dismiss_action(&AiDialog::Restore {
                lines: vec!["?? stray.txt".to_string()],
            }),
            None,
            "Esc cannot silently stop the restore"
        );
    }

    /// Leaving the PR closes the flow dialogs — a Confirm left floating over
    /// the list would check out and run against the session just left — but
    /// the app-wide recovery list stays (Task 4 review finding 1).
    #[test]
    fn only_the_recovery_list_survives_leaving_the_pr() {
        let plan = CheckoutPlan {
            repo_path: PathBuf::from("/repo"),
            remote: "origin".to_string(),
            original: OriginalRef::Branch("main".to_string()),
            dirty: false,
            skip_stash: false,
            commands: Vec::new(),
        };
        for dialog in [
            AiDialog::PickFolder { key: key() },
            AiDialog::ConfirmCheckout(plan),
            AiDialog::Restore {
                lines: vec!["?? stray.txt".to_string()],
            },
            AiDialog::Held {
                pr_number: 9,
                restore: Some(3),
            },
        ] {
            assert!(!survives_navigation(&dialog), "{dialog:?} is PR-scoped");
        }
        assert!(
            survives_navigation(&AiDialog::Recovery(Vec::new())),
            "the recovery list belongs to no PR"
        );
    }

    /// A held repository whose review is over offers the restore its old
    /// advice ("finish that review") could not deliver (Task 4 review
    /// finding 2).
    #[test]
    fn a_finished_reviews_held_checkout_offers_a_restore() {
        use egui_kittest::kittest::Queryable;

        let mut harness = egui_kittest::Harness::builder()
            .with_size([600.0, 400.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui_state(
                move |ui, actions: &mut Vec<AiDialogAction>| {
                    let inputs = DialogInputs {
                        pr: None,
                        prepare: &[],
                        error: None,
                    };
                    render(
                        ui.ctx(),
                        &AiDialog::Held {
                            pr_number: 7,
                            restore: Some(3),
                        },
                        &inputs,
                        actions,
                    );
                },
                Vec::new(),
            );
        harness.get_by_label("Restore it now").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiDialogAction::RecoverRestore(3)],
            "the button restores the record that holds the repo"
        );
    }
}
