//! The PR list screen: scope chips, the repo/org filter, and the results table.
//!
//! The screen owns no state of its own beyond the text the user is typing: it
//! turns input into [`PrListMsg`]s (which the app folds in with
//! [`crate::state::pr_list::reduce`]) and into [`PrListAction`]s (which the app
//! turns into commands).

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::github::search::{Scope, ScopeConstraint, Sort};
use crate::model::RepoRef;
use crate::state::pr_list::{counter_label, visible_items, PrListMsg, PrListState};
use crate::ui::widgets::pr_row::{self, pr_row_vm};

/// How many columns the results table has.
const COLUMNS: usize = 6;

/// The scope chips, in display order.
const SCOPE_CHIPS: [Scope; 5] = [
    Scope::ReviewRequested,
    Scope::Assigned,
    Scope::Authored,
    Scope::Involves,
    Scope::Mentions,
];

/// The screen's editable fields, owned across frames by the app.
#[derive(Clone, Debug, Default)]
pub struct PrListForm {
    /// The repo/org constraint text field.
    pub constraint_input: String,
    /// The paging key already auto-loaded, so the "Load more" row fires once
    /// per page rather than once per frame.
    pub last_auto: Option<String>,
}

/// What the user asked of the PR list beyond a state message.
#[derive(Clone, Debug, PartialEq)]
pub enum PrListAction {
    /// Fetch the next page of every query that has one.
    LoadMore,
    /// Open one PR. Wired into the PR session in Task 22.
    Open {
        /// The PR's repository.
        repo: RepoRef,
        /// The PR number.
        number: u64,
    },
    /// Open the settings window (Task 53).
    Settings,
}

/// The chip label for a scope.
fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Assigned => "Assigned",
        Scope::Authored => "Authored",
        Scope::ReviewRequested => "Review requested",
        Scope::Involves => "Involves",
        Scope::Mentions => "Mentions",
    }
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

/// Paint the whole screen, recording what the user asked for.
pub fn render(
    ui: &mut egui::Ui,
    state: &mut PrListState,
    form: &mut PrListForm,
    msgs: &mut Vec<PrListMsg>,
    actions: &mut Vec<PrListAction>,
) {
    render_controls(ui, state, form, msgs);

    ui.horizontal(|ui| {
        ui.label(counter_label(state));
        if state.loading {
            ui.spinner();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Settings").clicked() {
                actions.push(PrListAction::Settings);
            }
        });
    });
    if let Some(error) = &state.error {
        let color = ui.visuals().error_fg_color;
        ui.colored_label(color, error.to_string());
    }

    let rows = visible_items(state);
    if rows.is_empty() && !state.loading {
        ui.label("no pull requests to show — adjust the scopes or filter above");
    }

    let pending_key = auto_key(state);
    let total_rows = rows.len() + usize::from(pending_key.is_some());

    TableBuilder::new(ui)
        .striped(true)
        .column(Column::remainder().at_least(160.0))
        .column(Column::initial(120.0))
        .column(Column::initial(90.0))
        .column(Column::initial(140.0))
        .column(Column::initial(80.0))
        .column(Column::initial(150.0))
        .header(20.0, |mut header| {
            for title in ["Title", "Repo", "Author", "Status", "Diff", "Updated"] {
                header.col(|ui| {
                    ui.strong(title);
                });
            }
        })
        .body(|body| {
            body.rows(20.0, total_rows, |mut row| {
                let index = row.index();
                let Some(pr) = rows.get(index).copied() else {
                    // The trailing "Load more" row.
                    row.col(|ui| {
                        let response = ui.add(egui::Button::new("Load more"));
                        let visible = ui.is_rect_visible(response.rect);
                        if let Some(key) = &pending_key {
                            let wanted = response.clicked() || visible;
                            if wanted && form.last_auto.as_deref() != Some(key.as_str()) {
                                form.last_auto = Some(key.clone());
                                actions.push(PrListAction::LoadMore);
                            }
                        }
                    });
                    for _ in 1..COLUMNS {
                        row.col(|_ui| {});
                    }
                    return;
                };

                let vm = pr_row_vm(pr);
                let (repo, number) = (pr.repo.clone(), pr.number);
                row.col(|ui| {
                    let response = ui.add(
                        egui::Label::new(&vm.title)
                            .sense(egui::Sense::click())
                            .truncate(),
                    );
                    if response.clicked() {
                        actions.push(PrListAction::Open {
                            repo: repo.clone(),
                            number,
                        });
                    }
                });
                row.col(|ui| {
                    ui.label(&vm.repo_number);
                });
                row.col(|ui| {
                    ui.label(&vm.author);
                });
                row.col(|ui| {
                    pr_row::render_badges(ui, &vm);
                });
                row.col(|ui| {
                    ui.label(format!(
                        "+{} -{} · {}💬",
                        vm.additions, vm.deletions, vm.comments
                    ));
                });
                row.col(|ui| {
                    ui.label(&vm.updated_at);
                });
            });
        });
}

/// The scope chips and the filter row above the table.
fn render_controls(
    ui: &mut egui::Ui,
    state: &mut PrListState,
    form: &mut PrListForm,
    msgs: &mut Vec<PrListMsg>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Scopes:");
        let mut toggled = state.scopes.clone();
        for scope in SCOPE_CHIPS {
            let on = toggled.contains(&scope);
            if ui.selectable_label(on, scope_label(scope)).clicked() {
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

    ui.horizontal_wrapped(|ui| {
        ui.label("Filter:");
        if ui
            .add(
                egui::TextEdit::singleline(&mut state.search)
                    .hint_text("title, repo, or author")
                    .desired_width(200.0),
            )
            .changed()
        {
            msgs.push(PrListMsg::SetSearch(state.search.clone()));
        }

        ui.separator();

        ui.label("Repo or org:");
        ui.add(
            egui::TextEdit::singleline(&mut form.constraint_input)
                .hint_text("owner/repo or org")
                .desired_width(170.0),
        );
        if ui.button("Apply").clicked() {
            msgs.push(PrListMsg::SetConstraint(parse_constraint(
                &form.constraint_input,
            )));
        }
        if ui.button("Clear").clicked() {
            form.constraint_input.clear();
            msgs.push(PrListMsg::SetConstraint(ScopeConstraint::None));
        }
        if ui.button("Refresh").clicked() {
            msgs.push(PrListMsg::Refresh);
        }

        ui.separator();

        let mut sort = state.sort;
        egui::ComboBox::from_label("Sort")
            .selected_text(sort_label(state.sort))
            .show_ui(ui, |ui| {
                for option in [Sort::Updated, Sort::Created, Sort::Comments] {
                    ui.selectable_value(&mut sort, option, sort_label(option));
                }
            });
        if sort != state.sort {
            msgs.push(PrListMsg::SetSort(sort));
        }
    });
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
            end_cursor: Some("c1".to_owned()),
            has_next: true,
            total: 9,
        }];
        let mut form = PrListForm::default();
        let mut msgs = Vec::new();
        let mut actions = Vec::new();

        egui::__run_test_ui(|ui| {
            render(ui, &mut state, &mut form, &mut msgs, &mut actions);
        });

        assert!(msgs.is_empty());
        assert_eq!(
            auto_key(&state).as_deref(),
            Some("is:pr is:open assignee:mona sort:updated-desc@c1")
        );
    }
}
