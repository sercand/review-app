//! Token storage.
//!
//! Access tokens live only in the OS keychain — never in `settings.toml`,
//! session files, or logs. [`MemoryStore`] exists so tests can exercise code
//! that needs a [`TokenStore`] without touching the keychain.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use keyring::Entry;

/// The keychain service every entry is stored under.
const SERVICE: &str = "gh-review-desktop";

/// Read and write access tokens, keyed by host and login.
pub trait TokenStore: Send + Sync {
    /// The stored token, or `None` when there is no entry for this account.
    fn get(&self, host: &str, login: &str) -> anyhow::Result<Option<String>>;
    /// Store (or replace) the token for this account.
    fn set(&self, host: &str, login: &str, token: &str) -> anyhow::Result<()>;
    /// Remove the token for this account. Clearing an absent entry succeeds.
    fn clear(&self, host: &str, login: &str) -> anyhow::Result<()>;
}

/// The keychain account name for `host` and `login`: `{host}:{login}`.
fn account(host: &str, login: &str) -> String {
    format!("{host}:{login}")
}

/// Wrap a keyring failure in `anyhow` (keyring's error is not `Send + Sync`).
fn keyring_err(error: keyring::Error) -> anyhow::Error {
    anyhow::anyhow!("keyring error: {error}")
}

/// The real store: the platform keychain.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringStore;

impl TokenStore for KeyringStore {
    fn get(&self, host: &str, login: &str) -> anyhow::Result<Option<String>> {
        let entry = Entry::new(SERVICE, &account(host, login)).map_err(keyring_err)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keyring_err(error)),
        }
    }

    fn set(&self, host: &str, login: &str, token: &str) -> anyhow::Result<()> {
        let entry = Entry::new(SERVICE, &account(host, login)).map_err(keyring_err)?;
        entry.set_password(token).map_err(keyring_err)
    }

    fn clear(&self, host: &str, login: &str) -> anyhow::Result<()> {
        let entry = Entry::new(SERVICE, &account(host, login)).map_err(keyring_err)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keyring_err(error)),
        }
    }
}

/// An in-memory [`TokenStore`] for tests.
#[derive(Default)]
pub struct MemoryStore {
    tokens: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    /// The map, recovered even if a previous holder panicked while locked.
    fn tokens(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.tokens
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

impl TokenStore for MemoryStore {
    fn get(&self, host: &str, login: &str) -> anyhow::Result<Option<String>> {
        Ok(self.tokens().get(&account(host, login)).cloned())
    }

    fn set(&self, host: &str, login: &str, token: &str) -> anyhow::Result<()> {
        self.tokens().insert(account(host, login), token.to_owned());
        Ok(())
    }

    fn clear(&self, host: &str, login: &str) -> anyhow::Result<()> {
        self.tokens().remove(&account(host, login));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_and_clears() {
        let s = MemoryStore::default();
        assert_eq!(s.get("github.com", "me").unwrap(), None);
        s.set("github.com", "me", "tok").unwrap();
        assert_eq!(s.get("github.com", "me").unwrap().as_deref(), Some("tok"));
        s.clear("github.com", "me").unwrap();
        s.clear("github.com", "me").unwrap(); // clearing twice is fine
        assert_eq!(s.get("github.com", "me").unwrap(), None);
    }
}
