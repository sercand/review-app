//! The PR list's Local repos tab: the linked checkouts as an accordion
//! (AI settings Task 9, mockup `ai-settings/local-repos-accordion.html`).
//!
//! Rows are the repositories linked to a local folder on the signed-in host
//! ([`linked_repos`]); clicking a row expands its open PRs, rendered with
//! [`pr_row::render_row`] exactly as the All PRs tab draws them. The widget
//! owns no state: the rows come from the pure [`rows`] view-model over
//! [`crate::state::local_repos`]'s reducer state, and every click leaves as a
//! [`LocalReposAction`] for `app.rs` to apply. The ⋯ menu's Change folder and
//! Unlink are disabled with Task 7's `change_blocked` reason, and the folder
//! probe's verdict folds in through the pure [`apply_folder_answer`].
//!
//! Unix-only with [`crate::ai`], whose linked checkouts the tab lists.

use std::path::PathBuf;

use eframe::egui;
use egui::{Popup, PopupCloseBehavior, RichText};

use crate::ai::checkout::RemoteRepo;
use crate::ai::repo_link::{FolderStatus, folder_status, link_repo, linked_repos};
use crate::model::RepoRef;
use crate::persist::settings::AiSettings;
use crate::state::local_repos::{LocalReposState, RepoPrs};
use crate::ui::widgets::pr_row;
use crate::ui::{components, icons, theme};

/// What a folder pick from this tab is for: link a repository not linked yet
/// ([`LocalPicker::Add`]) or replace the folder of the key named by
/// [`LocalPicker::Change`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalPicker {
    /// "+ Add local repo…": the probed folder links whatever repository it
    /// is a clone of.
    Add,
    /// The ⋯ menu's "Change folder" for this `host/owner/name` key: the
    /// folder is replaced only when it is a clone of that same repository.
    Change(String),
}

/// What the user asked of the Local repos tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalReposAction {
    /// Expand or collapse one repository's PRs (the `host/owner/name` key).
    Toggle(String),
    /// Open one PR of a repository, as the main list does.
    OpenPr {
        /// The repository key, `host/owner/name`.
        repo: String,
        /// The PR number.
        number: u64,
    },
    /// "+ Add local repo…": open the OS folder picker.
    AddRepo,
    /// The ⋯ menu's "Change folder" for this key.
    ChangeFolder(String),
    /// The ⋯ menu's "Unlink" for this key.
    Unlink(String),
}

/// Everything one Local repos row draws.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalRepoRowVm {
    /// The repository key, `host/owner/name`.
    pub repo: String,
    /// The linked folder.
    pub path: PathBuf,
    /// Whether that folder still stands.
    pub status: FolderStatus,
    /// The open PR count GitHub reported, or `None` while it is loading or
    /// after a failure (the row shows `—`).
    pub open_prs: Option<u32>,
    /// Whether the row's PRs are expanded.
    pub expanded: bool,
}

/// The table's rows: the repositories linked to a folder on `host`, sorted
/// by key ([`linked_repos`]), each with its folder status, its open PR count
/// once the search has answered, and its expanded flag.
pub fn rows(ai: &AiSettings, host: &str, state: &LocalReposState) -> Vec<LocalRepoRowVm> {
    linked_repos(ai, host)
        .into_iter()
        .map(|entry| {
            let key = entry.repo.to_lowercase();
            LocalRepoRowVm {
                repo: entry.repo.clone(),
                path: entry.path.clone(),
                status: folder_status(&entry.path),
                open_prs: match state.repos.get(&key) {
                    Some(RepoPrs::Loaded { total, .. }) => Some(*total),
                    Some(RepoPrs::Loading) | Some(RepoPrs::Failed(_)) | None => None,
                },
                expanded: state.expanded.contains(&key),
            }
        })
        .collect()
}

/// The `owner/name` repository a `host/owner/name` key names, or `None` when
/// the key names none.
pub fn repo_ref_from_key(key: &str) -> Option<RepoRef> {
    let mut parts = key.split('/');
    let _host = parts.next()?;
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(RepoRef::new(owner, name))
}

/// Fold a folder probe's verdict for a Local repos pick into `ai`, returning
/// the toast to show — `None` when the link landed and only a settings save
/// is owed. A folder that is not a clone of the right repository on the
/// signed-in host links nothing and says so (spec: "not on the signed-in
/// host, say so and add nothing"), and an Add of an already-linked
/// repository changes nothing.
pub fn apply_folder_answer(
    ai: &mut AiSettings,
    host: &str,
    picker: &LocalPicker,
    path: PathBuf,
    result: Result<RemoteRepo, String>,
) -> Option<String> {
    let remote = match result {
        Ok(remote) => remote,
        Err(reason) => return Some(reason),
    };
    match picker {
        // A change replaces the folder only when the picked folder is a
        // clone of the same repository, like the PR AI tab's row (Task 7).
        LocalPicker::Change(key) => {
            if remote.key().eq_ignore_ascii_case(key) {
                link_repo(ai, &remote, path);
                None
            } else {
                Some(format!(
                    "that folder is a clone of {}, not {key}",
                    remote.key()
                ))
            }
        }
        LocalPicker::Add => {
            if !remote.host.eq_ignore_ascii_case(host) {
                return Some(format!(
                    "that folder is a clone of {}, which is not on the signed-in host ({host})",
                    remote.key()
                ));
            }
            let key = remote.key().to_lowercase();
            let already_linked = ai.repos.iter().any(|entry| {
                entry.repo.to_lowercase() == key && !entry.path.as_os_str().is_empty()
            });
            if already_linked {
                return Some("already linked".to_owned());
            }
            link_repo(ai, &remote, path);
            None
        }
    }
}

/// The row height of the table's header and repo rows.
const ROW_H: f32 = 30.0;

/// The `owner/name` a `host/owner/name` key shows in the Repository column.
fn slug(key: &str) -> &str {
    key.split_once('/').map_or(key, |(_, slug)| slug)
}

/// The x ranges of the chevron, repository, folder, open-PRs and menu cells
/// in a table row spanning `rect` (mockup header: chevron | Repository |
/// Folder | Open PRs | ⋯).
fn columns(
    rect: egui::Rect,
) -> (
    egui::Rangef,
    egui::Rangef,
    egui::Rangef,
    egui::Rangef,
    egui::Rangef,
) {
    let chevron = egui::Rangef::new(rect.left() + 8.0, rect.left() + 26.0);
    let menu = egui::Rangef::new(rect.right() - 30.0, rect.right() - 6.0);
    let open = egui::Rangef::new(rect.right() - 110.0, rect.right() - 42.0);
    let gap = 14.0;
    let rest_start = chevron.max + gap;
    let rest_end = open.min - gap;
    let repo = egui::Rangef::new(rest_start, rest_start + (rest_end - rest_start) * 0.42);
    let folder = egui::Rangef::new(repo.max + gap, rest_end);
    (chevron, repo, folder, open, menu)
}

/// The table header (mockup `local-repos-accordion.html`): "Repository",
/// "Folder" and "Open PRs" in `faint` 11.5 pt over a `line` rule.
fn render_header(ui: &mut egui::Ui) {
    let pal = theme::palette_of(ui);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_H),
        egui::Sense::hover(),
    );
    ui.painter().rect_filled(rect, 0.0, pal.bg);
    ui.painter()
        .hline(rect.x_range(), rect.bottom() - 0.5, (1.0, pal.line));
    let (_, repo, folder, open, _) = columns(rect);
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
    left(repo.min, "Repository");
    left(folder.min, "Folder");
    ui.painter().text(
        egui::pos2(open.max, cy),
        egui::Align2::RIGHT_CENTER,
        "Open PRs",
        font,
        pal.faint,
    );
}

/// One disabled-with-reason row of the ⋯ menu (Task 7's `change_blocked`).
fn menu_item(ui: &mut egui::Ui, label: &str, blocked: Option<&str>) -> egui::Response {
    let response = ui.add_enabled(
        blocked.is_none(),
        egui::Button::new(RichText::new(label).size(12.5))
            .min_size(egui::vec2(ui.available_width(), 24.0)),
    );
    match blocked {
        Some(reason) => response.on_hover_text(reason),
        None => response,
    }
}

/// Paint the tab's content below the screen's tab strip: the table header,
/// one accordion row per linked repository, and the empty-state line.
/// `blocked` names why a repository's folder cannot change right now
/// (Task 7's `change_blocked`), and `now` is the wall-clock ISO time the
/// PR rows' ages are measured against.
pub fn render(
    ui: &mut egui::Ui,
    ai: &AiSettings,
    host: &str,
    state: &LocalReposState,
    blocked: &dyn Fn(&str) -> Option<String>,
    now: &str,
    actions: &mut Vec<LocalReposAction>,
) {
    let pal = theme::palette_of(ui);
    let rows = rows(ai, host, state);
    if rows.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label(
                RichText::new(
                    "No local repos linked yet. Use “+ Add local repo…” to link a folder that is a clone of a repository on this host, or open a PR and use its AI tab.",
                )
                .color(pal.muted),
            );
        });
        return;
    }

    render_header(ui);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in &rows {
                render_row(ui, row, state, blocked, now, actions);
            }
        });
}

/// One accordion row: chevron, repository, folder status, open-PR count and
/// the ⋯ menu; expanded, the repository's PRs follow under an accent rail.
fn render_row(
    ui: &mut egui::Ui,
    row: &LocalRepoRowVm,
    state: &LocalReposState,
    blocked: &dyn Fn(&str) -> Option<String>,
    now: &str,
    actions: &mut Vec<LocalReposAction>,
) {
    let pal = theme::palette_of(ui);
    let key = row.repo.to_lowercase();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_H),
        egui::Sense::click(),
    );
    let (chevron, repo, folder, open, menu) = columns(rect);

    // The ⋯ button rides on top of the row's click area.
    let menu_id = ui.make_persistent_id(("local-repo-menu", key.clone()));
    let menu_rect = egui::Rect::from_min_max(
        egui::pos2(menu.min, rect.top()),
        egui::pos2(menu.max, rect.bottom()),
    );
    let menu_response = ui.interact(menu_rect, menu_id, egui::Sense::click());

    // The open-PRs cell hovers on its own so a failure can explain itself.
    let open_rect = egui::Rect::from_min_max(
        egui::pos2(open.min, rect.top()),
        egui::pos2(open.max, rect.bottom()),
    );
    let open_response = ui.interact(
        open_rect,
        ui.make_persistent_id(("local-repo-open", key.clone())),
        egui::Sense::hover(),
    );

    if row.expanded {
        ui.painter()
            .rect_filled(rect, 0.0, components::tint(pal.accent, 28));
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 0.0, pal.surface);
    }
    ui.painter()
        .hline(rect.x_range(), rect.bottom() - 0.5, (1.0, pal.line_soft));

    let cy = rect.center().y;
    let glyph = if row.expanded {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    ui.painter().text(
        egui::pos2(chevron.center(), cy),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(13.0),
        pal.muted,
    );

    let repo_galley = components::elided_galley(
        ui,
        slug(&row.repo),
        crate::ui::fonts::semibold(12.5),
        pal.text,
        repo.span(),
    );
    ui.painter().galley(
        egui::pos2(repo.min, cy - repo_galley.size().y / 2.0),
        repo_galley,
        pal.text,
    );

    // The Folder column: the path, or why the folder cannot be used. A
    // missing folder still lists its PRs (spec), so only the cell complains.
    match row.status {
        FolderStatus::Ok => {
            let text = row.path.display().to_string();
            let galley = components::elided_galley(
                ui,
                &text,
                egui::FontId::proportional(12.0),
                pal.muted,
                folder.span(),
            );
            let elided = galley.elided;
            ui.painter().galley(
                egui::pos2(folder.min, cy - galley.size().y / 2.0),
                galley,
                pal.muted,
            );
            if elided {
                response.clone().on_hover_text(&text);
            }
        }
        FolderStatus::Missing => {
            ui.painter().text(
                egui::pos2(folder.min, cy),
                egui::Align2::LEFT_CENTER,
                "folder missing",
                egui::FontId::proportional(12.0),
                pal.bad,
            );
        }
        FolderStatus::NotARepo => {
            ui.painter().text(
                egui::pos2(folder.min, cy),
                egui::Align2::LEFT_CENTER,
                "not a git repository",
                egui::FontId::proportional(12.0),
                pal.warn,
            );
        }
    }

    // The Open PRs column: the count, a spinner while loading, `—` when the
    // search failed (hover says why) or never ran.
    match state.repos.get(&key) {
        Some(RepoPrs::Loading) => {
            let box_rect = egui::Rect::from_center_size(
                egui::pos2(open.max - 8.0, cy),
                egui::vec2(16.0, 16.0),
            );
            ui.put(box_rect, egui::Spinner::new().size(14.0));
        }
        Some(RepoPrs::Loaded { total, .. }) => {
            ui.painter().text(
                egui::pos2(open.max, cy),
                egui::Align2::RIGHT_CENTER,
                pr_row::compact_count(*total),
                egui::FontId::proportional(12.0),
                pal.muted,
            );
        }
        Some(RepoPrs::Failed(error)) => {
            ui.painter().text(
                egui::pos2(open.max, cy),
                egui::Align2::RIGHT_CENTER,
                "—",
                egui::FontId::proportional(12.0),
                pal.faint,
            );
            open_response.on_hover_text(error.to_string());
        }
        None => {
            ui.painter().text(
                egui::pos2(open.max, cy),
                egui::Align2::RIGHT_CENTER,
                "—",
                egui::FontId::proportional(12.0),
                pal.faint,
            );
        }
    }

    ui.painter().text(
        egui::pos2(menu.center(), cy),
        egui::Align2::CENTER_CENTER,
        icons::ELLIPSIS,
        egui::FontId::proportional(15.0),
        if menu_response.hovered() {
            pal.text
        } else {
            pal.muted
        },
    );

    // The ⋯ menu (mockup footnote): Change folder, Unlink — both disabled
    // with Task 7's reason while a run or an unrestored checkout holds the
    // repository.
    Popup::menu(&menu_response)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(170.0);
            let reason = blocked(&key);
            if menu_item(ui, "Change folder", reason.as_deref()).clicked() {
                actions.push(LocalReposAction::ChangeFolder(row.repo.clone()));
            }
            if menu_item(ui, "Unlink", reason.as_deref()).clicked() {
                actions.push(LocalReposAction::Unlink(row.repo.clone()));
            }
        });

    if response.clicked() && !menu_response.clicked() {
        actions.push(LocalReposAction::Toggle(row.repo.clone()));
    }

    if row.expanded {
        let frame = egui::Frame::new().inner_margin(egui::Margin {
            left: 28,
            right: 10,
            top: 4,
            bottom: 8,
        });
        let inner = frame.show(ui, |ui| match state.repos.get(&key) {
            Some(RepoPrs::Loaded { items, .. }) => {
                if items.is_empty() {
                    ui.label(
                        RichText::new("No open pull requests.")
                            .size(12.0)
                            .color(pal.muted),
                    );
                }
                for pr in items {
                    if pr_row::render_row(ui, pr, now, false).clicked() {
                        actions.push(LocalReposAction::OpenPr {
                            repo: row.repo.clone(),
                            number: pr.number,
                        });
                    }
                }
            }
            Some(RepoPrs::Failed(error)) => {
                ui.colored_label(pal.bad, error.to_string());
            }
            Some(RepoPrs::Loading) | None => {
                ui.spinner();
            }
        });
        // The mockup's left accent rail beside the expanded PRs.
        let rect = inner.response.rect;
        ui.painter()
            .vline(rect.left() + 16.0, rect.y_range(), (2.0, pal.accent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(host: &str, owner: &str, name: &str) -> RemoteRepo {
        RemoteRepo {
            host: host.to_string(),
            owner: owner.to_string(),
            name: name.to_string(),
        }
    }

    fn entry(repo: &str, path: impl Into<PathBuf>) -> crate::persist::settings::RepoAiSettings {
        crate::persist::settings::RepoAiSettings {
            repo: repo.to_string(),
            path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn rows_list_only_linked_repos_on_this_host_sorted() {
        // A path that cannot exist: a child of a fresh temp dir that was
        // never created, so no machine can have it (final review).
        let dir = crate::test_support::tempdir();
        let gone = dir.path().join("no-such-folder");
        let ai = AiSettings {
            repos: vec![
                entry("github.com/acme/zeta", "/z"),
                entry("gitlab.com/acme/alpha", "/g"),
                entry("github.com/acme/beta", PathBuf::new()),
                entry("github.com/acme/alpha", gone.clone()),
            ],
            ..Default::default()
        };
        let mut state = LocalReposState::default();
        state.expanded.insert("github.com/acme/alpha".to_owned());
        state.repos.insert(
            "github.com/acme/alpha".to_owned(),
            RepoPrs::Loaded {
                total: 3,
                items: Vec::new(),
            },
        );
        state
            .repos
            .insert("github.com/acme/zeta".to_owned(), RepoPrs::Loading);

        let rows = rows(&ai, "GitHub.com", &state);
        let keys: Vec<&str> = rows.iter().map(|row| row.repo.as_str()).collect();
        assert_eq!(keys, ["github.com/acme/alpha", "github.com/acme/zeta"]);
        assert_eq!(rows[0].path, gone);
        assert_eq!(rows[0].open_prs, Some(3));
        assert!(rows[0].expanded);
        assert_eq!(rows[1].open_prs, None, "a loading search has no count yet");
        assert!(!rows[1].expanded);
        assert_eq!(
            rows[0].status,
            FolderStatus::Missing,
            "a child of a fresh temp dir names no folder anywhere"
        );
    }

    #[test]
    fn add_repo_links_on_ok_and_reports_errors() {
        let mut ai = AiSettings::default();
        let toast = apply_folder_answer(
            &mut ai,
            "github.com",
            &LocalPicker::Add,
            PathBuf::from("/widgets"),
            Ok(remote("github.com", "acme", "widgets")),
        );
        assert_eq!(toast, None, "a clean link needs no toast");
        assert_eq!(ai.repos.len(), 1);
        assert_eq!(ai.repos[0].repo, "github.com/acme/widgets");
        assert_eq!(ai.repos[0].path, PathBuf::from("/widgets"));

        let toast = apply_folder_answer(
            &mut ai,
            "github.com",
            &LocalPicker::Add,
            PathBuf::from("/plain"),
            Err("that folder is not a git repository".to_owned()),
        );
        assert_eq!(
            toast.as_deref(),
            Some("that folder is not a git repository"),
            "the probe's reason is the toast"
        );
        assert_eq!(ai.repos.len(), 1, "a failed probe links nothing");

        let toast = apply_folder_answer(
            &mut ai,
            "github.com",
            &LocalPicker::Add,
            PathBuf::from("/other"),
            Ok(remote("gitlab.com", "acme", "other")),
        );
        let reason = toast.expect("a folder on another host is refused");
        assert!(reason.contains("gitlab.com/acme/other"), "{reason}");
        assert!(reason.contains("github.com"), "{reason}");
        assert_eq!(ai.repos.len(), 1, "nothing is added for another host");
    }

    #[test]
    fn add_existing_repo_changes_nothing() {
        let mut ai = AiSettings {
            repos: vec![entry("github.com/acme/widgets", "/old")],
            ..Default::default()
        };
        let toast = apply_folder_answer(
            &mut ai,
            "github.com",
            &LocalPicker::Add,
            PathBuf::from("/new"),
            Ok(remote("GitHub.com", "Acme", "Widgets")),
        );
        assert_eq!(toast.as_deref(), Some("already linked"));
        assert_eq!(ai.repos[0].path, PathBuf::from("/old"));
        assert_eq!(ai.repos.len(), 1);
    }

    #[test]
    fn change_folder_replaces_the_path_and_rejects_another_repo() {
        let mut ai = AiSettings {
            repos: vec![entry("github.com/acme/widgets", "/old")],
            ..Default::default()
        };
        let change = LocalPicker::Change("github.com/acme/widgets".to_owned());
        let toast = apply_folder_answer(
            &mut ai,
            "github.com",
            &change,
            PathBuf::from("/new"),
            Ok(remote("github.com", "acme", "widgets")),
        );
        assert_eq!(toast, None);
        assert_eq!(ai.repos[0].path, PathBuf::from("/new"));

        let toast = apply_folder_answer(
            &mut ai,
            "github.com",
            &change,
            PathBuf::from("/elsewhere"),
            Ok(remote("github.com", "acme", "other")),
        );
        let reason = toast.expect("a clone of another repository is refused");
        assert!(
            reason.contains("acme/other") && reason.contains("acme/widgets"),
            "{reason}"
        );
        assert_eq!(ai.repos[0].path, PathBuf::from("/new"));

        let toast = apply_folder_answer(
            &mut ai,
            "github.com",
            &change,
            PathBuf::from("/gone"),
            Err("that folder is gone".to_owned()),
        );
        assert_eq!(toast.as_deref(), Some("that folder is gone"));
        assert_eq!(ai.repos[0].path, PathBuf::from("/new"));
    }

    #[test]
    fn repo_ref_from_key_parses_slugs_only() {
        assert_eq!(
            repo_ref_from_key("github.com/acme/widgets"),
            Some(RepoRef::new("acme", "widgets"))
        );
        assert_eq!(repo_ref_from_key("acme/widgets"), None);
        assert_eq!(repo_ref_from_key("github.com/acme/"), None);
        assert_eq!(repo_ref_from_key("github.com/a/b/c"), None);
    }
}
