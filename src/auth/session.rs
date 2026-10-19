//! Sign-in bookkeeping: persist a token, restore it at startup, and clear it on
//! sign-out or a 401.
//!
//! These helpers are deliberately tiny and work only over a [`TokenStore`] plus
//! [`Settings`]: they hold no UI state and **never** touch drafts or composer
//! text (spec §6). Drafts are keyed by host/repo/PR, not by token, so they must
//! survive a 401 and a sign-out.

use crate::auth::store::TokenStore;
use crate::github::client::Me;
use crate::persist::settings::{Account, Settings};

/// Persist a completed sign-in: store the token and cache the account.
///
/// The token goes under the keychain account `{settings.host}:{login}`, and the
/// identity (login, display name, avatar, granted scopes) is cached in
/// `settings` so that the next launch can locate the keychain entry.
pub fn complete_login(
    store: &dyn TokenStore,
    settings: &mut Settings,
    me: &Me,
    token: &str,
) -> anyhow::Result<()> {
    store.set(&settings.host, &me.user.login, token)?;
    settings.account = Some(Account {
        login: me.user.login.clone(),
        name: me.user.name.clone(),
        avatar_url: me.user.avatar_url.clone(),
        scopes: me.scopes.clone(),
    });
    Ok(())
}

/// The stored token for the cached account, or `None` when signed out.
///
/// A missing `account` means there is nothing to restore; a missing keychain
/// entry means the token was revoked or removed outside the app.
pub fn restore_token(
    store: &dyn TokenStore,
    settings: &Settings,
) -> anyhow::Result<Option<String>> {
    let Some(account) = &settings.account else {
        return Ok(None);
    };
    store.get(&settings.host, &account.login)
}

/// A 401 arrived: forget the token, keep everything else.
///
/// Only the keychain entry is cleared. `settings.account` is kept so the login
/// screen can prefill the known identity, and drafts/composer text are
/// untouched (spec §6).
pub fn handle_unauthorized(store: &dyn TokenStore, settings: &Settings) -> anyhow::Result<()> {
    if let Some(account) = &settings.account {
        store.clear(&settings.host, &account.login)?;
    }
    Ok(())
}

/// Sign out: forget the token **and** the cached account.
///
/// Drafts and composer text are never deleted by sign-out (spec §6).
pub fn sign_out(store: &dyn TokenStore, settings: &mut Settings) -> anyhow::Result<()> {
    handle_unauthorized(store, settings)?;
    settings.account = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::{MemoryStore, TokenStore};
    use crate::github::client::{GitHubUser, Me};
    use crate::persist::settings::Settings;

    fn me(login: &str) -> Me {
        Me {
            user: GitHubUser {
                login: login.into(),
                name: None,
                avatar_url: None,
            },
            scopes: vec!["repo".into()],
        }
    }

    #[test]
    fn login_persists_token_and_account_then_restores() {
        let store = MemoryStore::default();
        let mut settings = Settings::default();
        complete_login(&store, &mut settings, &me("octocat"), "tok").unwrap();
        assert_eq!(
            store.get("github.com", "octocat").unwrap().as_deref(),
            Some("tok")
        );
        assert_eq!(settings.account.as_ref().unwrap().login, "octocat");
        assert_eq!(
            restore_token(&store, &settings).unwrap().as_deref(),
            Some("tok")
        );
    }

    #[test]
    fn unauthorized_and_sign_out_only_touch_the_token_and_account() {
        let store = MemoryStore::default();
        let mut settings = Settings::default();
        complete_login(&store, &mut settings, &me("octocat"), "tok").unwrap();

        handle_unauthorized(&store, &settings).unwrap();
        assert_eq!(store.get("github.com", "octocat").unwrap(), None);
        assert!(settings.account.is_some()); // prefill for re-login

        complete_login(&store, &mut settings, &me("octocat"), "tok2").unwrap();
        sign_out(&store, &mut settings).unwrap();
        assert_eq!(store.get("github.com", "octocat").unwrap(), None);
        assert!(settings.account.is_none());
    }

    #[test]
    fn restore_token_without_an_account_is_none() {
        let store = MemoryStore::default();
        let settings = Settings::default();
        assert_eq!(restore_token(&store, &settings).unwrap(), None);
    }

    #[test]
    fn oauth_default_scope_is_pinned() {
        assert_eq!(crate::auth::DEFAULT_SCOPE, "repo read:org");
    }
}
