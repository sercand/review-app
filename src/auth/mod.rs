//! Authentication: OAuth device flow, token storage, sign-in bookkeeping, and
//! the HTTP driver.

pub mod device_flow;
pub mod device_http;
pub mod session;
pub mod store;

/// The OAuth scopes the app requests by default (spec §6).
///
/// `repo` is required to read private repositories and to post comments;
/// `read:org` is what the org-scoped search queries need. Spec §6 fixes the
/// default string exactly, and the set stays configurable, so this is only the
/// default — it is pinned by a test in [`session`] so it cannot drift.
pub const DEFAULT_SCOPE: &str = "repo read:org";

/// The public client ID of the compiled-in OAuth App (spec §4 prerequisite).
///
/// Device Flow needs no client secret, so this value is public by design. A
/// github.com client ID does not work on an Enterprise Server host, which is
/// why the login screen asks for an override there.
///
/// This placeholder stands in for the real ID of the OAuth App registered with
/// Device Flow enabled; replace it (or set `client_id` in settings) before
/// signing in to github.com.
pub const DEFAULT_CLIENT_ID: &str = "REPLACE_WITH_YOUR_OAUTH_APP_CLIENT_ID";
