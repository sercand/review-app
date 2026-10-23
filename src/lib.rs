//! `gh-review-desktop` — a desktop client for reviewing GitHub pull requests.
//!
//! The crate is split into a thin binary (`src/main.rs`) and this library, so
//! that integration tests can reach the public module tree.

/// The package version, as compiled from `Cargo.toml`.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(unix)]
pub mod ai;
pub mod app;
pub mod auth;
pub mod diff;
pub mod github;
pub mod model;
pub mod persist;
pub mod runtime;
pub mod state;
#[cfg(test)]
pub mod test_support;
pub mod ui;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_exposes_version() {
        assert!(!super::version().is_empty());
    }
}
