//! The PR list's Local tab: linked checkouts and their open PRs.
//!
//! The tab lists every repository linked to a local folder (spec §4) with
//! its open PRs underneath. The screen owns no logic: it turns user input
//! into [`LocalReposMsg`]s and the worker's replies into messages folded back
//! through [`reduce`]. One search per repository runs through
//! [`Command::SearchRepoPrs`]; at most [`MAX_IN_FLIGHT`] are outstanding at a
//! time, and a [`LocalReposMsg::Refresh`] bumps the generation so replies to
//! a superseded round are dropped (the tab is not PR-scoped, so the
//! generation counter stands in for the session rule).
//!
//! [`ListTab`] is the only cross-platform item: `Layout` persists it on every
//! platform. The loading state drives [`Command::SearchRepoPrs`], which is
//! unix-only with [`crate::ai`], so it is too.

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[cfg(unix)]
use crate::github::error::GhError;
#[cfg(unix)]
use crate::github::graphql::SearchPage;
#[cfg(unix)]
use crate::github::search::ScopeConstraint;
#[cfg(unix)]
use crate::model::RepoRef;
#[cfg(unix)]
use crate::model::pr::PrSummary;
#[cfg(unix)]
use crate::runtime::Command;

/// Which PR-list tab the app last showed, persisted in
/// [`Layout::pr_list_tab`](crate::persist::settings::Layout::pr_list_tab).
///
/// The `Local` tab lists the [`crate::ai`] checkouts and is unix-only, but
/// the enum is compiled everywhere because `Layout` is serialized on every
/// platform; on Windows a saved `Local` selection loads as `All` — the same
/// rule as
/// [`DetailsTab::for_platform`](crate::ui::widgets::metadata_panel::DetailsTab::for_platform).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListTab {
    /// The scope-based PR list.
    #[default]
    All,
    /// The linked local checkouts and their open PRs (unix-only).
    Local,
}

impl ListTab {
    /// The tab this selection may show on the current platform: the Local
    /// tab is unix-only, so on Windows a saved (or hand-edited) `Local`
    /// selection loads as `All`.
    pub fn for_platform(self) -> ListTab {
        #[cfg(not(unix))]
        if self == Self::Local {
            return Self::All;
        }
        self
    }
}

/// The most repo PR searches the worker may run at once; the rest wait in
/// [`LocalReposState`]'s queue.
#[cfg(unix)]
pub const MAX_IN_FLIGHT: usize = 4;

/// One linked repository's open PRs, as the worker last reported them.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq)]
pub enum RepoPrs {
    /// A search is outstanding.
    Loading,
    /// The list arrived; `total` is GitHub's `issueCount`.
    Loaded {
        /// The total number of open PRs GitHub counted.
        total: u32,
        /// The first page's rows, in GitHub's order.
        items: Vec<PrSummary>,
    },
    /// The search failed; the row shows the error and a refresh retries.
    Failed(GhError),
}

/// Everything the Local tab draws from.
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct LocalReposState {
    /// The rows the user has expanded, by lowercased repo key.
    pub expanded: BTreeSet<String>,
    /// The current refresh generation; older replies are dropped.
    pub generation: u64,
    /// Each repo's list, keyed by the lowercased `host/owner/name`.
    pub repos: BTreeMap<String, RepoPrs>,
    /// Keys waiting for a free search slot.
    queue: VecDeque<String>,
    /// Searches currently outstanding.
    in_flight: usize,
}

/// Something that happened to the Local tab.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq)]
pub enum LocalReposMsg {
    /// The linked repos changed, or the user asked to refresh: restart
    /// loading for exactly these `host/owner/name` keys at a new generation.
    Refresh(Vec<String>),
    /// A row was expanded or collapsed. The first expand of a repo nobody
    /// has looked at yet fetches it; a known one never refetches.
    Toggle(String),
    /// A repo's open PRs arrived.
    Loaded {
        /// The generation that issued the search.
        generation: u64,
        /// The repo key the search was for.
        repo: String,
        /// The parsed page.
        page: SearchPage,
    },
    /// A repo's search failed.
    Failed {
        /// The generation that issued the search.
        generation: u64,
        /// The repo key the search was for.
        repo: String,
        /// The classified failure.
        error: GhError,
    },
}

/// Fold one message into the state, returning the commands it implies.
#[cfg(unix)]
pub fn reduce(state: &mut LocalReposState, msg: LocalReposMsg) -> Vec<Command> {
    match msg {
        LocalReposMsg::Refresh(keys) => {
            // A new generation supersedes every outstanding search; its
            // replies arrive later and are dropped on arrival.
            state.generation += 1;
            state.repos.clear();
            state.queue.clear();
            state.in_flight = 0;
            for key in keys {
                enqueue(state, key);
            }
            dispatch(state)
        }
        LocalReposMsg::Toggle(key) => {
            let key = key.to_lowercase();
            if !state.expanded.remove(&key) {
                state.expanded.insert(key.clone());
            }
            // The first expand of a repo nobody has looked at yet fetches
            // it; a known one — loaded, loading, or failed — stays local.
            if state.repos.contains_key(&key) {
                Vec::new()
            } else {
                enqueue(state, key);
                dispatch(state)
            }
        }
        LocalReposMsg::Loaded {
            generation,
            repo,
            page,
        } => {
            if generation != state.generation {
                return Vec::new();
            }
            state.in_flight = state.in_flight.saturating_sub(1);
            state.repos.insert(
                repo,
                RepoPrs::Loaded {
                    total: page.issue_count,
                    items: page.items,
                },
            );
            dispatch(state)
        }
        LocalReposMsg::Failed {
            generation,
            repo,
            error,
        } => {
            if generation != state.generation {
                return Vec::new();
            }
            state.in_flight = state.in_flight.saturating_sub(1);
            state.repos.insert(repo, RepoPrs::Failed(error));
            dispatch(state)
        }
    }
}

/// Queue `key` for a search unless it is queued or searching already. A key
/// that is not `host/owner/name` names no repository and is dropped.
#[cfg(unix)]
fn enqueue(state: &mut LocalReposState, key: String) {
    let key = key.to_lowercase();
    if repo_query(&key).is_none() || state.repos.contains_key(&key) || state.queue.contains(&key) {
        return;
    }
    state.queue.push_back(key);
}

/// Start queued searches while a slot is free, marking each repo Loading.
#[cfg(unix)]
fn dispatch(state: &mut LocalReposState) -> Vec<Command> {
    let mut commands = Vec::new();
    while state.in_flight < MAX_IN_FLIGHT {
        let Some(key) = state.queue.pop_front() else {
            break;
        };
        let Some(query) = repo_query(&key) else {
            continue;
        };
        state.repos.insert(key.clone(), RepoPrs::Loading);
        state.in_flight += 1;
        commands.push(Command::SearchRepoPrs {
            generation: state.generation,
            repo: key,
            query,
        });
    }
    commands
}

/// The search query listing `key`'s open PRs: its `owner/name` as a
/// [`ScopeConstraint::Repo`] qualifier. `None` when the key is not a
/// `host/owner/name`.
#[cfg(unix)]
fn repo_query(key: &str) -> Option<String> {
    let mut parts = key.split('/');
    let host = parts.next()?;
    let owner = parts.next()?;
    let name = parts.next()?;
    if host.is_empty() || owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    let qualifier = ScopeConstraint::Repo(RepoRef::new(owner, name)).qualifier()?;
    Some(format!("is:pr is:open {qualifier}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::pr_summary;

    fn page(numbers: &[u64]) -> SearchPage {
        SearchPage {
            items: numbers.iter().map(|n| pr_summary(*n, "t", 0)).collect(),
            issue_count: numbers.len() as u32,
            end_cursor: None,
            has_next: false,
        }
    }

    fn key(i: usize) -> String {
        format!("github.com/acme/repo{i}")
    }

    #[test]
    fn refresh_starts_at_most_four_searches() {
        let mut s = LocalReposState::default();
        let keys: Vec<String> = (0..6).map(key).collect();
        let cmds = reduce(&mut s, LocalReposMsg::Refresh(keys));
        assert_eq!(cmds.len(), MAX_IN_FLIGHT);
        assert_eq!(s.in_flight, MAX_IN_FLIGHT);
        assert_eq!(s.queue.len(), 2);
        assert!(matches!(
            &cmds[0],
            Command::SearchRepoPrs { generation, repo, query }
                if *generation == s.generation
                    && repo == "github.com/acme/repo0"
                    && query == "is:pr is:open repo:acme/repo0"
        ));
        for i in 0..4 {
            assert!(matches!(s.repos.get(&key(i)), Some(RepoPrs::Loading)));
        }

        // A reply frees a slot for the next queued repo.
        let round = s.generation;
        let cmds = reduce(
            &mut s,
            LocalReposMsg::Loaded {
                generation: round,
                repo: key(0),
                page: page(&[1]),
            },
        );
        assert_eq!(cmds.len(), 1);
        assert!(matches!(&cmds[..], [Command::SearchRepoPrs { repo, .. }] if repo == &key(4)));
        assert_eq!(s.in_flight, MAX_IN_FLIGHT);
        assert_eq!(s.queue.len(), 1);
        assert!(matches!(
            s.repos.get(&key(0)),
            Some(RepoPrs::Loaded { total: 1, items }) if items.len() == 1
        ));
    }

    #[test]
    fn refresh_lowercases_keys_and_skips_malformed_ones() {
        let mut s = LocalReposState::default();
        let cmds = reduce(
            &mut s,
            LocalReposMsg::Refresh(vec![
                "GitHub.com/Acme/Widgets".into(),
                "not-a-key".into(),
                "GitHub.com/Acme/Widgets".into(),
            ]),
        );
        assert_eq!(cmds.len(), 1, "the malformed key is skipped, dupe once");
        assert!(matches!(
            &cmds[..],
            [Command::SearchRepoPrs { repo, query, .. }]
                if repo == "github.com/acme/widgets"
                    && query == "is:pr is:open repo:acme/widgets"
        ));
    }

    #[test]
    fn stale_generation_replies_are_dropped() {
        let mut s = LocalReposState::default();
        reduce(&mut s, LocalReposMsg::Refresh(vec![key(0)]));
        let stale = s.generation;
        reduce(&mut s, LocalReposMsg::Refresh(vec![key(1)]));
        let round = s.generation;
        assert_ne!(stale, round);

        // A page from the superseded round leaves no trace.
        let cmds = reduce(
            &mut s,
            LocalReposMsg::Loaded {
                generation: stale,
                repo: key(0),
                page: page(&[9]),
            },
        );
        assert!(cmds.is_empty());
        assert!(!s.repos.contains_key(&key(0)));
        assert!(matches!(s.repos.get(&key(1)), Some(RepoPrs::Loading)));

        // A stale failure is dropped too and does not free a slot.
        let cmds = reduce(
            &mut s,
            LocalReposMsg::Failed {
                generation: stale,
                repo: key(0),
                error: GhError::NotFound,
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(s.in_flight, 1);

        // The current generation's reply lands.
        let cmds = reduce(
            &mut s,
            LocalReposMsg::Loaded {
                generation: round,
                repo: key(1),
                page: page(&[3]),
            },
        );
        assert!(cmds.is_empty());
        assert!(
            matches!(s.repos.get(&key(1)), Some(RepoPrs::Loaded { total: 1, items })
                if items.len() == 1)
        );
        assert_eq!(s.in_flight, 0);
    }

    #[test]
    fn toggle_expands_and_collapses_without_refetching_loaded_repos() {
        let mut s = LocalReposState::default();
        reduce(&mut s, LocalReposMsg::Refresh(vec![key(0)]));
        let round = s.generation;
        reduce(
            &mut s,
            LocalReposMsg::Loaded {
                generation: round,
                repo: key(0),
                page: page(&[1, 2]),
            },
        );

        // Expanding a loaded repo is local.
        let cmds = reduce(&mut s, LocalReposMsg::Toggle(key(0)));
        assert!(cmds.is_empty());
        assert!(s.expanded.contains(&key(0)));

        // So is collapsing it again.
        let cmds = reduce(&mut s, LocalReposMsg::Toggle(key(0)));
        assert!(cmds.is_empty());
        assert!(!s.expanded.contains(&key(0)));

        // The first expand of a repo the refresh never covered fetches it.
        let cmds = reduce(&mut s, LocalReposMsg::Toggle(key(9)));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[..],
            [Command::SearchRepoPrs { repo, query, .. }]
                if repo == "github.com/acme/repo9"
                    && query == "is:pr is:open repo:acme/repo9"
        ));
        assert!(s.expanded.contains(&key(9)));
    }

    #[test]
    fn failed_repo_does_not_block_others() {
        let mut s = LocalReposState::default();
        let keys: Vec<String> = (0..5).map(key).collect();
        reduce(&mut s, LocalReposMsg::Refresh(keys));
        let round = s.generation;

        // repo4 is queued; failing repo0 must start it.
        let cmds = reduce(
            &mut s,
            LocalReposMsg::Failed {
                generation: round,
                repo: key(0),
                error: GhError::NotFound,
            },
        );
        assert_eq!(cmds.len(), 1);
        assert!(matches!(&cmds[..], [Command::SearchRepoPrs { repo, .. }] if repo == &key(4)));
        assert!(matches!(
            s.repos.get(&key(0)),
            Some(RepoPrs::Failed(GhError::NotFound))
        ));

        // The other in-flight repos are untouched.
        for i in 1..4 {
            assert!(matches!(s.repos.get(&key(i)), Some(RepoPrs::Loading)));
        }
    }
}
