//! The PR list's state and its pure reducer.
//!
//! The screen owns no logic: it turns user input into [`PrListMsg`]s and the
//! network's replies into [`Command`]s via [`reduce`]. A refresh starts one
//! search per scope (spec §7.2) and each reply is folded back in here, so the
//! merge, the local filter, and the counter are all unit-testable.

use std::collections::HashSet;

use crate::github::error::GhError;
use crate::github::graphql::SearchPage;
use crate::github::search::{build_queries, Scope, ScopeConstraint, Sort};
use crate::model::pr::PrSummary;
use crate::runtime::Command;

/// The paging position of one in-flight search query.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryCursor {
    /// The search query this cursor tracks.
    pub query: String,
    /// The `endCursor` of the last page, when there is another page.
    pub end_cursor: Option<String>,
    /// Whether GitHub reported another page.
    pub has_next: bool,
    /// The query's total `issueCount`.
    pub total: u32,
}

/// Everything the PR list screen draws from.
#[derive(Default)]
pub struct PrListState {
    /// The selected personal scopes (one query each).
    pub scopes: Vec<Scope>,
    /// The repo/org every query is restricted to.
    pub constraint: ScopeConstraint,
    /// The local text filter.
    pub search: String,
    /// The sort applied to every query and to the loaded rows.
    pub sort: Sort,
    /// Every loaded row, in load order.
    pub items: Vec<PrSummary>,
    /// One cursor per query of the current request.
    pub queries: Vec<QueryCursor>,
    /// The current request generation; older replies are dropped.
    pub request: u64,
    /// Whether any query of the current request is still outstanding.
    pub loading: bool,
    /// The failure of the most recent request, if any.
    pub error: Option<GhError>,
    /// The queries of the current request that have not replied yet.
    awaiting: HashSet<String>,
}

/// Something that happened to the PR list.
#[derive(Clone, Debug, PartialEq)]
pub enum PrListMsg {
    /// The selected scopes changed.
    SetScopes(Vec<Scope>),
    /// The repo/org constraint changed.
    SetConstraint(ScopeConstraint),
    /// The sort changed.
    SetSort(Sort),
    /// The local text filter changed. Local only — no network.
    SetSearch(String),
    /// Re-run every query for the current state.
    Refresh,
    /// A search page arrived.
    PageLoaded {
        /// The request generation that issued the search.
        request: u64,
        /// The query the page answers.
        query: String,
        /// The parsed page.
        page: SearchPage,
    },
    /// A search failed.
    LoadFailed {
        /// The request generation that issued the search.
        request: u64,
        /// The classified failure.
        error: GhError,
    },
}

/// Fold one message into the state, returning the commands it implies.
pub fn reduce(state: &mut PrListState, msg: PrListMsg, login: &str) -> Vec<Command> {
    match msg {
        PrListMsg::SetSearch(text) => {
            state.search = text;
            Vec::new()
        }
        PrListMsg::SetScopes(scopes) => {
            state.scopes = scopes;
            restart(state, login)
        }
        PrListMsg::SetConstraint(constraint) => {
            state.constraint = constraint;
            restart(state, login)
        }
        PrListMsg::SetSort(sort) => {
            state.sort = sort;
            restart(state, login)
        }
        PrListMsg::Refresh => restart(state, login),
        PrListMsg::PageLoaded {
            request,
            query,
            page,
        } => {
            if request == state.request {
                accept(state, &query, page);
            }
            Vec::new()
        }
        PrListMsg::LoadFailed { request, error } => {
            if request == state.request {
                state.error = Some(error);
                // A failure has no query attached, so the whole request is
                // considered answered; `loading` stops either way.
                state.awaiting.clear();
                state.loading = false;
            }
            Vec::new()
        }
    }
}

/// Start a fresh request: bump the generation, drop the rows, rebuild the
/// queries, and return one search per query.
fn restart(state: &mut PrListState, login: &str) -> Vec<Command> {
    state.request += 1;
    state.items.clear();
    state.error = None;
    state.queries = build_queries(&state.scopes, &state.constraint, login, state.sort)
        .into_iter()
        .map(|query| QueryCursor {
            query,
            end_cursor: None,
            has_next: false,
            total: 0,
        })
        .collect();
    state.awaiting = state
        .queries
        .iter()
        .map(|cursor| cursor.query.clone())
        .collect();
    state.loading = !state.awaiting.is_empty();

    state
        .queries
        .iter()
        .map(|cursor| Command::SearchPrs {
            request: state.request,
            query: cursor.query.clone(),
            after: None,
        })
        .collect()
}

/// Fold one page of one query into the state.
fn accept(state: &mut PrListState, query: &str, page: SearchPage) {
    if let Some(cursor) = state
        .queries
        .iter_mut()
        .find(|cursor| cursor.query == query)
    {
        cursor.end_cursor = page.end_cursor;
        cursor.has_next = page.has_next;
        cursor.total = page.issue_count;
    }

    // De-duplicate by `(repo.id(), number)`: the same PR can be matched by
    // several scopes, and by several pages of the same scope.
    let mut seen: HashSet<(String, u64)> = state
        .items
        .iter()
        .map(|item| (item.repo.id(), item.number))
        .collect();
    for item in page.items {
        if seen.insert((item.repo.id(), item.number)) {
            state.items.push(item);
        }
    }

    state.awaiting.remove(query);
    state.loading = !state.awaiting.is_empty();
}

/// The next page of every query that GitHub says has one.
pub fn load_more(state: &PrListState) -> Vec<Command> {
    state
        .queries
        .iter()
        .filter(|cursor| cursor.has_next)
        .filter_map(|cursor| {
            let after = cursor.end_cursor.clone()?;
            Some(Command::SearchPrs {
                request: state.request,
                query: cursor.query.clone(),
                after: Some(after),
            })
        })
        .collect()
}

/// The rows to draw: the local text filter applied, then sorted descending.
///
/// The filter matches the title, the `owner/name` repo, or the author, case
/// insensitively.
pub fn visible_items(state: &PrListState) -> Vec<&PrSummary> {
    let needle = state.search.to_lowercase();
    let mut items: Vec<&PrSummary> = state
        .items
        .iter()
        .filter(|pr| {
            needle.is_empty()
                || pr.title.to_lowercase().contains(&needle)
                || pr.repo.to_string().to_lowercase().contains(&needle)
                || pr.author.to_lowercase().contains(&needle)
        })
        .collect();

    items.sort_by(|a, b| match state.sort {
        Sort::Updated => b.updated_at.cmp(&a.updated_at),
        Sort::Created => b.created_at.cmp(&a.created_at),
        Sort::Comments => b.comments.cmp(&a.comments),
    });
    items
}

/// The `"N of M"` counter, `"≈"`-prefixed when more than one query is merged.
///
/// Duplicates across scopes are counted once in `N` but still counted in every
/// `issueCount`, so the sum is an upper bound.
pub fn counter_label(state: &PrListState) -> String {
    let total: u32 = state.queries.iter().map(|cursor| cursor.total).sum();
    if state.queries.len() > 1 {
        format!("{} of ≈{}", state.items.len(), total)
    } else {
        format!("{} of {}", state.items.len(), total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pr_summary;

    fn page(numbers: &[u64], total: u32, cursor: Option<&str>) -> SearchPage {
        SearchPage {
            items: numbers.iter().map(|n| pr_summary(*n, "t", 0)).collect(),
            issue_count: total,
            end_cursor: cursor.map(Into::into),
            has_next: cursor.is_some(),
        }
    }

    #[test]
    fn scope_change_starts_one_search_per_scope() {
        let mut s = PrListState::default();
        let cmds = reduce(
            &mut s,
            PrListMsg::SetScopes(vec![Scope::Assigned, Scope::Authored]),
            "mona",
        );
        assert_eq!(cmds.len(), 2);
        assert!(s.loading && s.items.is_empty() && s.error.is_none());
    }

    #[test]
    fn pages_merge_dedupe_and_ignore_stale_requests() {
        let mut s = PrListState::default();
        reduce(
            &mut s,
            PrListMsg::SetScopes(vec![Scope::Assigned, Scope::Authored]),
            "mona",
        );
        let (req, q0, q1) = (
            s.request,
            s.queries[0].query.clone(),
            s.queries[1].query.clone(),
        );
        reduce(
            &mut s,
            PrListMsg::PageLoaded {
                request: req,
                query: q0.clone(),
                page: page(&[1, 2], 2, Some("c1")),
            },
            "mona",
        );
        reduce(
            &mut s,
            PrListMsg::PageLoaded {
                request: req,
                query: q1,
                page: page(&[2, 3], 2, None),
            },
            "mona",
        );
        assert_eq!(s.items.len(), 3); // #2 appears once
        assert_eq!(counter_label(&s), "3 of ≈4");
        let more = load_more(&s);
        assert!(
            matches!(&more[..], [Command::SearchPrs { query, after: Some(c), .. }] if *query == q0 && c == "c1")
        );

        reduce(
            &mut s,
            PrListMsg::PageLoaded {
                request: req - 1,
                query: "old".into(),
                page: page(&[9], 1, None),
            },
            "mona",
        );
        assert_eq!(s.items.len(), 3);
    }

    #[test]
    fn search_text_filters_locally_and_sort_reorders() {
        let mut s = PrListState::default();
        reduce(&mut s, PrListMsg::SetScopes(vec![Scope::Assigned]), "mona");
        let (req, q) = (s.request, s.queries[0].query.clone());
        let mut p = page(&[], 2, None);
        p.items = vec![pr_summary(1, "fix bug", 5), pr_summary(2, "Add search", 9)];
        reduce(
            &mut s,
            PrListMsg::PageLoaded {
                request: req,
                query: q,
                page: p,
            },
            "mona",
        );

        assert!(reduce(&mut s, PrListMsg::SetSearch("SEARCH".into()), "mona").is_empty());
        assert_eq!(visible_items(&s).len(), 1);
        reduce(&mut s, PrListMsg::SetSearch(String::new()), "mona");
        s.sort = Sort::Comments; // local re-sort of loaded rows
        assert_eq!(visible_items(&s)[0].number, 2);
    }
}
