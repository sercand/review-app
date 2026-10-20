//! The PR list screen: scope chips with counts, the repository combobox,
//! the filter field, the sort menu, and the 52 pt result rows (Task 15).
//!
//! The screen owns no state of its own beyond the text the user is typing: it
//! turns input into [`PrListMsg`]s (which the app folds in with
//! [`crate::state::pr_list::reduce`]) and into [`PrListAction`]s (which the app
//! turns into commands).

use eframe::egui;
use egui::{Popup, PopupCloseBehavior, RichText, UiBuilder};

use crate::github::search::{Scope, ScopeConstraint, Sort};
use crate::model::RepoRef;
use crate::state::pr_list::{PrListMsg, PrListState, counter_label, visible_items};
use crate::ui::widgets::pr_row;
use crate::ui::{icons, theme};

/// The screen's editable fields, owned across frames by the app.
#[derive(Clone, Debug, Default)]
pub struct PrListForm {
    /// The repository combobox's search text.
    pub repo_query: String,
    /// Whether the repository combobox's popup is open.
    pub repo_open: bool,
    /// Whether the sort menu's popup is open.
    pub sort_open: bool,
    /// The paging key already auto-loaded, so the "Load more" row fires once
    /// per page rather than once per frame.
    pub last_auto: Option<String>,
}

/// What the user asked of the PR list beyond a state message.
#[derive(Clone, Debug, PartialEq)]
pub enum PrListAction {
    /// Fetch the next page of every query that has one.
    LoadMore,
    /// Open one PR.
    Open {
        /// The PR's repository.
        repo: RepoRef,
        /// The PR number.
        number: u64,
    },
    /// Open the settings window.
    Settings,
    /// Open the command palette.
    Palette,
    /// Switch between the dark and light themes.
    ToggleTheme,
}

/// The list's title bar (mockup `prs.html` `.titlebar`): the screen's name,
/// then Commands, refresh, theme and settings on the right.
pub fn render_title_bar(
    ui: &mut egui::Ui,
    msgs: &mut Vec<PrListMsg>,
    actions: &mut Vec<PrListAction>,
) {
    crate::ui::widgets::title_bar::show(ui, "pr-list-title", |ui| {
        ui.label(RichText::new("Pull requests").font(crate::ui::fonts::semibold(13.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icons::icon_button(ui, icons::SETTINGS, "Settings").clicked() {
                actions.push(PrListAction::Settings);
            }
            let glyph = if ui.visuals().dark_mode {
                icons::SUN
            } else {
                icons::MOON
            };
            if icons::icon_button(ui, glyph, "Toggle theme").clicked() {
                actions.push(PrListAction::ToggleTheme);
            }
            if icons::icon_button(ui, icons::REFRESH_CW, "Refresh").clicked() {
                msgs.push(PrListMsg::Refresh);
            }
            let commands =
                crate::ui::components::Btn::new(crate::ui::components::BtnKind::Ghost, "Commands")
                    .icon(icons::COMMAND)
                    .kbd("⌘K")
                    .show(ui);
            if commands.clicked() {
                actions.push(PrListAction::Palette);
            }
        });
    });
}

/// The dropdown label for a sort.
fn sort_label(sort: Sort) -> &'static str {
    match sort {
        Sort::Updated => "Updated",
        Sort::Created => "Created",
        Sort::Comments => "Comments",
    }
}

/// Parse the constraint box: `owner/repo` restricts to a repo, any other
/// non-empty text restricts to an org, and an empty box clears the constraint.
pub fn parse_constraint(text: &str) -> ScopeConstraint {
    let text = text.trim();
    if text.is_empty() {
        return ScopeConstraint::None;
    }
    match text.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() => {
            ScopeConstraint::Repo(RepoRef::new(owner, name))
        }
        Some(_) => ScopeConstraint::None,
        None => ScopeConstraint::Org(text.to_owned()),
    }
}

/// A key identifying the page every query is currently waiting for.
///
/// When it changes, the "Load more" row may load again; while it stays the
/// same, scrolling over the row must not fire duplicate requests.
fn auto_key(state: &PrListState) -> Option<String> {
    let mut parts: Vec<String> = state
        .queries
        .iter()
        .filter(|cursor| cursor.has_next)
        .filter_map(|cursor| {
            cursor
                .end_cursor
                .as_ref()
                .map(|after| format!("{}@{after}", cursor.query))
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    Some(parts.join("|"))
}

/// Paint the whole screen, recording what the user asked for; `now` is the
/// wall-clock ISO time the relative ages are measured against.
pub fn render(
    ui: &mut egui::Ui,
    state: &mut PrListState,
    form: &mut PrListForm,
    msgs: &mut Vec<PrListMsg>,
    actions: &mut Vec<PrListAction>,
    now: &str,
) {
    let pal = theme::palette_of(ui);
    let filter_id = ui.make_persistent_id("pr_filter");
    handle_shortcuts(ui, state, form, actions, filter_id);

    render_toolbar(ui, state, form, msgs, filter_id);
    if let Some(error) = &state.error {
        ui.colored_label(pal.bad, error.to_string());
    }

    let rows = visible_items(state);
    let pending_key = auto_key(state);
    pr_row::render_header(ui);
    let mut opened: Option<(RepoRef, u64)> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if rows.is_empty() && !state.loading {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(
                            "No open pull requests match. Turn on another scope or pick a different repository.",
                        )
                        .color(pal.muted),
                    );
                });
            }
            let mut last_repo: Option<String> = None;
            for pr in &rows {
                if state.group_by_repo {
                    let id = pr.repo.id();
                    if last_repo.as_deref() != Some(id.as_str()) {
                        last_repo = Some(id.clone());
                        repo_header(ui, &pr.repo.to_string());
                    }
                }
                let selected = state
                    .selected
                    .as_ref()
                    .is_some_and(|(repo, number)| *repo == pr.repo.id() && *number == pr.number);
                if pr_row::render_row(ui, pr, now, selected).clicked() {
                    opened = Some((pr.repo.clone(), pr.number));
                }
            }
            if let Some(key) = &pending_key {
                let response = ui.horizontal(|ui| {
                    ui.add_sized(
                        [ui.available_width(), pr_row::ROW_H],
                        egui::Button::new(RichText::new("Load more").size(12.0)),
                    )
                });
                let response = response.inner;
                let visible = ui.is_rect_visible(response.rect);
                if (response.clicked() || visible)
                    && form.last_auto.as_deref() != Some(key.as_str())
                {
                    form.last_auto = Some(key.clone());
                    actions.push(PrListAction::LoadMore);
                }
            }
        });

    if let Some((repo, number)) = opened {
        state.selected = Some((repo.id(), number));
        actions.push(PrListAction::Open { repo, number });
    }

    // The 26 pt status bar.
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.min.y),
            egui::pos2(rect.max.x, rect.min.y),
        ],
        egui::Stroke::new(1.0, pal.line_soft),
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.label(
                RichText::new(counter_label(state))
                    .size(11.5)
                    .color(pal.muted),
            );
            if state.loading {
                ui.spinner();
            }
        },
    );
}

/// `/` focuses the filter field (unless something already has focus) and
/// `Enter` opens the selected row.
fn handle_shortcuts(
    ui: &mut egui::Ui,
    state: &mut PrListState,
    form: &mut PrListForm,
    actions: &mut Vec<PrListAction>,
    filter_id: egui::Id,
) {
    let focused = ui.memory(|mem| mem.focused());
    let (slash, enter) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::Slash),
            i.key_pressed(egui::Key::Enter),
        )
    });
    if slash && focused.is_none() {
        form.repo_open = false;
        form.sort_open = false;
        ui.memory_mut(|mem| mem.request_focus(filter_id));
    }
    if enter
        && !form.repo_open
        && !form.sort_open
        && focused != Some(filter_id)
        && let Some((repo_id, number)) = state.selected.clone()
        && let Some(pr) = state
            .items
            .iter()
            .find(|pr| pr.repo.id() == repo_id && pr.number == number)
    {
        actions.push(PrListAction::Open {
            repo: pr.repo.clone(),
            number,
        });
    }
}

/// The two toolbar rows: scope chips with counts on top, then the repository
/// combobox, the filter field, and the sort menu.
fn render_toolbar(
    ui: &mut egui::Ui,
    state: &mut PrListState,
    form: &mut PrListForm,
    msgs: &mut Vec<PrListMsg>,
    filter_id: egui::Id,
) {
    let pal = theme::palette_of(ui);
    egui::Frame::new()
        .fill(pal.bg)
        .inner_margin(egui::Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 8.0);
            ui.horizontal(|ui| {
                let mut toggled = state.scopes.clone();
                for scope in SCOPE_ORDER {
                    let on = toggled.contains(&scope);
                    let count = state.scope_count(scope.name()) as u32;
                    let chip = crate::ui::components::toggle_chip(
                        ui,
                        on,
                        Some(pr_row::scope_icon(scope.name())),
                        scope.label(),
                        Some(count),
                    )
                    .on_hover_text(format!(
                        "{} pull requests where you're {}",
                        if on { "Hide" } else { "Show" },
                        scope.label().to_lowercase()
                    ));
                    if chip.clicked() {
                        if on {
                            toggled.retain(|selected| *selected != scope);
                        } else {
                            toggled.push(scope);
                        }
                    }
                }
                if toggled != state.scopes {
                    msgs.push(PrListMsg::SetScopes(toggled));
                }
            });

            ui.horizontal(|ui| {
                repo_combobox(ui, state, form, msgs);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    sort_menu(ui, state, form, msgs);
                    let field = crate::ui::components::Field::new(&mut state.search)
                        .icon(icons::SEARCH)
                        .hint("Filter title, repo or author")
                        .trailing_kbd("/")
                        .width(ui.available_width().min(320.0))
                        .id(filter_id)
                        .show(ui);
                    if field.changed() {
                        let text = state.search.clone();
                        msgs.push(PrListMsg::SetSearch(text));
                    }
                });
            });
        });
}

/// The scope chips' order (mockup `prs.html`): the review queue first.
const SCOPE_ORDER: [Scope; 5] = [
    Scope::ReviewRequested,
    Scope::Assigned,
    Scope::Authored,
    Scope::Involves,
    Scope::Mentions,
];

/// The repository combobox: current selection as its label; the popup offers
/// a search field, the recent constraints, and the repositories already
/// loaded. Enter in the search field applies it as a repo/org constraint.
fn repo_combobox(
    ui: &mut egui::Ui,
    state: &mut PrListState,
    form: &mut PrListForm,
    msgs: &mut Vec<PrListMsg>,
) {
    let pal = theme::palette_of(ui);
    let label = state
        .repo_filter
        .clone()
        .unwrap_or_else(|| match &state.constraint {
            ScopeConstraint::None => "All repositories".to_owned(),
            ScopeConstraint::Repo(repo) => repo.to_string(),
            ScopeConstraint::Org(org) => format!("org: {org}"),
        });
    let button = combo_button(
        ui,
        icons::BOOK_MARKED,
        &label,
        label == "All repositories",
        220.0,
    );
    if button.clicked() {
        form.repo_open = !form.repo_open;
    }
    if form.repo_open {
        let search_id = ui.make_persistent_id("repo_search");
        let mut open = form.repo_open;
        let mut close = false;
        Popup::from_response(&button)
            .open_bool(&mut open)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(280.0);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut form.repo_query)
                        .hint_text("owner/repo or org, Enter to apply")
                        .desired_width(260.0)
                        .id(search_id),
                );
                ui.memory_mut(|mem| mem.request_focus(search_id));
                let query = form.repo_query.clone();
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let constraint = parse_constraint(&query);
                    if !query.trim().is_empty() {
                        msgs.push(PrListMsg::PushRecent(query.trim().to_owned()));
                    }
                    msgs.push(PrListMsg::SetConstraint(constraint));
                    msgs.push(PrListMsg::SetRepoFilter(None));
                    close = true;
                    return;
                }
                ui.separator();
                let needle = query.to_lowercase();
                let matches =
                    |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle);
                if !state.recent_constraints.is_empty() {
                    ui.label(RichText::new("Recent").size(11.0).color(pal.faint));
                    for recent in state.recent_constraints.clone() {
                        if !matches(&recent) {
                            continue;
                        }
                        if ui
                            .selectable_label(false, RichText::new(&recent).size(12.0))
                            .clicked()
                        {
                            let constraint = parse_constraint(&recent);
                            msgs.push(PrListMsg::PushRecent(recent));
                            msgs.push(PrListMsg::SetConstraint(constraint));
                            msgs.push(PrListMsg::SetRepoFilter(None));
                            close = true;
                        }
                    }
                }
                ui.label(RichText::new("Repositories").size(11.0).color(pal.faint));
                let mut repos: Vec<String> =
                    state.items.iter().map(|pr| pr.repo.to_string()).collect();
                repos.sort();
                repos.dedup();
                for repo in repos {
                    if !matches(&repo) {
                        continue;
                    }
                    let on = state.repo_filter.as_deref() == Some(repo.as_str());
                    if ui
                        .selectable_label(on, RichText::new(&repo).size(12.0))
                        .clicked()
                    {
                        msgs.push(PrListMsg::SetRepoFilter(if on {
                            None
                        } else {
                            Some(repo.clone())
                        }));
                        close = true;
                    }
                }
            });
        form.repo_open = open && !close;
    }
}

/// A field-styled combobox button (mockup `.combo`): a leading icon, the
/// current value (`faint` while it is the placeholder) and an up/down
/// chevron, `width` wide.
fn combo_button(
    ui: &mut egui::Ui,
    icon: char,
    value: &str,
    placeholder: bool,
    width: f32,
) -> egui::Response {
    let pal = theme::palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, crate::ui::components::CONTROL_H),
        egui::Sense::click(),
    );
    let stroke = if response.hovered() {
        pal.muted
    } else {
        pal.line
    };
    ui.painter().rect(
        rect,
        6.0,
        pal.bg,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    let cy = rect.center().y;
    ui.painter().text(
        egui::pos2(rect.left() + 8.0, cy),
        egui::Align2::LEFT_CENTER,
        icon,
        egui::FontId::proportional(15.0),
        pal.muted,
    );
    ui.painter().text(
        egui::pos2(rect.right() - 8.0, cy),
        egui::Align2::RIGHT_CENTER,
        icons::CHEVRONS_UP_DOWN,
        egui::FontId::proportional(14.0),
        pal.muted,
    );
    let color = if placeholder { pal.faint } else { pal.text };
    let galley = crate::ui::components::elided_galley(
        ui,
        value,
        egui::FontId::proportional(13.0),
        color,
        rect.width() - 60.0,
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 30.0, cy - galley.size().y / 2.0),
        galley,
        color,
    );
    response
}

/// The sort menu: the three sorts plus the "Group by repository" toggle.
fn sort_menu(
    ui: &mut egui::Ui,
    state: &mut PrListState,
    form: &mut PrListForm,
    msgs: &mut Vec<PrListMsg>,
) {
    let button = crate::ui::components::Btn::new(
        crate::ui::components::BtnKind::Default,
        sort_label(state.sort),
    )
    .icon(icons::ARROW_DOWN_WIDE_NARROW)
    .show(ui);
    if button.clicked() {
        form.sort_open = !form.sort_open;
    }
    if form.sort_open {
        let mut open = form.sort_open;
        let mut close = false;
        Popup::from_response(&button)
            .open_bool(&mut open)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_width(200.0);
                let mut sort = state.sort;
                for option in [Sort::Updated, Sort::Created, Sort::Comments] {
                    if ui
                        .radio_value(
                            &mut sort,
                            option,
                            RichText::new(sort_label(option)).size(12.0),
                        )
                        .changed()
                    {
                        msgs.push(PrListMsg::SetSort(sort));
                        close = true;
                    }
                }
                ui.separator();
                if ui
                    .checkbox(
                        &mut state.group_by_repo,
                        RichText::new("Group by repository").size(12.0),
                    )
                    .changed()
                {
                    msgs.push(PrListMsg::SetGroupByRepo(state.group_by_repo));
                }
            });
        form.sort_open = open && !close;
    }
}

/// The sticky-ish repository header drawn above a group's rows.
fn repo_header(ui: &mut egui::Ui, repo: &str) {
    let pal = theme::palette_of(ui);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.text(
        egui::pos2(rect.min.x + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        repo,
        egui::FontId::proportional(11.5),
        pal.muted,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::pr_list::QueryCursor;
    use crate::test_support::pr_summary;

    #[test]
    fn the_constraint_box_parses_repos_orgs_and_blanks() {
        assert_eq!(parse_constraint("  "), ScopeConstraint::None);
        assert_eq!(
            parse_constraint("octo/repo"),
            ScopeConstraint::Repo(RepoRef::new("octo", "repo"))
        );
        assert_eq!(
            parse_constraint("acme"),
            ScopeConstraint::Org("acme".to_owned())
        );
        assert_eq!(parse_constraint("octo/"), ScopeConstraint::None);
    }

    #[test]
    fn the_screen_paints_rows_and_the_load_more_row() {
        let mut state = PrListState::default();
        state.scopes = vec![Scope::Assigned];
        state.items = vec![pr_summary(1, "one", 2), pr_summary(2, "two", 0)];
        state.queries = vec![QueryCursor {
            query: "is:pr is:open assignee:mona sort:updated-desc".to_owned(),
            scope: "assigned".to_owned(),
            end_cursor: Some("c1".to_owned()),
            has_next: true,
            total: 9,
        }];
        let mut form = PrListForm::default();
        let mut msgs = Vec::new();
        let mut actions = Vec::new();

        egui::__run_test_ui(|ui| {
            render(
                ui,
                &mut state,
                &mut form,
                &mut msgs,
                &mut actions,
                "2026-10-19T12:00:00Z",
            );
        });

        assert!(msgs.is_empty());
        assert_eq!(
            auto_key(&state).as_deref(),
            Some("is:pr is:open assignee:mona sort:updated-desc@c1")
        );
    }
}
