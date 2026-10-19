//! The per-scope PR search-query builder.
//!
//! GitHub search ANDs every qualifier, so scopes can never be joined into one
//! query (spec §7.2). Each selected scope becomes its own query; the caller
//! merges and de-duplicates the results.

use serde::{Deserialize, Serialize};

use crate::model::RepoRef;

/// One of the personal scopes the PR list can be filtered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// PRs assigned to the signed-in user.
    Assigned,
    /// PRs the signed-in user authored.
    Authored,
    /// PRs whose review was requested from the signed-in user.
    ReviewRequested,
    /// PRs the signed-in user is involved in.
    Involves,
    /// PRs the signed-in user was mentioned in.
    Mentions,
}

/// The repository or organization every query is restricted to.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum ScopeConstraint {
    /// No restriction.
    #[default]
    None,
    /// Only this repository.
    Repo(RepoRef),
    /// Only this organization.
    Org(String),
}

/// The sort applied server-side to every query (and locally after merging).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Sort {
    /// Most recently updated first.
    #[default]
    Updated,
    /// Most recently created first.
    Created,
    /// Most commented first.
    Comments,
}

impl Scope {
    /// Every scope, in the canonical order merged rows list their scopes in.
    pub const ALL: [Scope; 5] = [
        Scope::Assigned,
        Scope::Authored,
        Scope::ReviewRequested,
        Scope::Involves,
        Scope::Mentions,
    ];

    /// The canonical name recorded on every row a scope's search matched
    /// (redesign Task 15); stable across logins, unlike the qualifier.
    pub fn name(self) -> &'static str {
        match self {
            Scope::Assigned => "assigned",
            Scope::Authored => "authored",
            Scope::ReviewRequested => "review-requested",
            Scope::Involves => "involves",
            Scope::Mentions => "mentions",
        }
    }

    /// The chip label for this scope.
    pub fn label(self) -> &'static str {
        match self {
            Scope::Assigned => "Assigned",
            Scope::Authored => "Authored",
            Scope::ReviewRequested => "Review requested",
            Scope::Involves => "Involves",
            Scope::Mentions => "Mentions",
        }
    }

    /// The search qualifier for this scope, given the signed-in user's login.
    pub fn qualifier(self, login: &str) -> String {
        match self {
            Scope::Assigned => format!("assignee:{login}"),
            Scope::Authored => format!("author:{login}"),
            Scope::ReviewRequested => format!("review-requested:{login}"),
            Scope::Involves => format!("involves:{login}"),
            Scope::Mentions => format!("mentions:{login}"),
        }
    }
}

impl ScopeConstraint {
    /// The search qualifier for this constraint, if any.
    fn qualifier(&self) -> Option<String> {
        match self {
            ScopeConstraint::None => None,
            ScopeConstraint::Repo(repo) => Some(format!("repo:{repo}")),
            ScopeConstraint::Org(org) => Some(format!("org:{org}")),
        }
    }
}

impl Sort {
    /// The search qualifier for this sort.
    fn qualifier(self) -> &'static str {
        match self {
            Sort::Updated => "sort:updated-desc",
            Sort::Created => "sort:created-desc",
            Sort::Comments => "sort:comments-desc",
        }
    }
}

/// Build one search query per selected scope (spec §7.2).
///
/// With no scope selected but a constraint set, a single "all open PRs there"
/// query is returned. With neither, the result is empty.
pub fn build_queries(
    scopes: &[Scope],
    constraint: &ScopeConstraint,
    login: &str,
    sort: Sort,
) -> Vec<String> {
    let constraint = constraint.qualifier();
    let sort = sort.qualifier();

    if scopes.is_empty() {
        return match constraint {
            Some(constraint) => vec![format!("is:pr is:open {constraint} {sort}")],
            None => Vec::new(),
        };
    }

    scopes
        .iter()
        .map(|scope| {
            let mut parts = vec![
                "is:pr".to_owned(),
                "is:open".to_owned(),
                scope.qualifier(login),
            ];
            if let Some(constraint) = &constraint {
                parts.push(constraint.clone());
            }
            parts.push(sort.to_owned());
            parts.join(" ")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_query_per_scope_never_joined() {
        use ScopeConstraint as C;
        assert_eq!(
            build_queries(&[Scope::Assigned], &C::None, "mona", Sort::Updated),
            vec!["is:pr is:open assignee:mona sort:updated-desc"]
        );
        assert_eq!(
            build_queries(
                &[Scope::Involves, Scope::Mentions],
                &C::None,
                "mona",
                Sort::Updated
            ),
            vec![
                "is:pr is:open involves:mona sort:updated-desc",
                "is:pr is:open mentions:mona sort:updated-desc"
            ]
        );
        assert_eq!(
            build_queries(
                &[Scope::ReviewRequested],
                &C::Repo(RepoRef::new("Octo", "Repo")),
                "mona",
                Sort::Comments
            ),
            vec!["is:pr is:open review-requested:mona repo:Octo/Repo sort:comments-desc"]
        );
        assert_eq!(
            build_queries(
                &[Scope::Authored],
                &C::Org("acme".into()),
                "mona",
                Sort::Created
            ),
            vec!["is:pr is:open author:mona org:acme sort:created-desc"]
        );
        assert_eq!(
            build_queries(&[], &C::Org("acme".into()), "mona", Sort::Updated),
            vec!["is:pr is:open org:acme sort:updated-desc"]
        );
        assert!(build_queries(&[], &C::None, "mona", Sort::Updated).is_empty());
    }
}
