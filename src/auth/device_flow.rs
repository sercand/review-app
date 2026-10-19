//! The pure device-flow state machine.
//!
//! This module holds no I/O: it maps a [`DeviceCode`] and a sequence of
//! [`PollOutcome`]s to an [`AuthState`]. The HTTP driver in
//! [`crate::auth::device_http`] feeds it.

use std::time::Duration;

/// A device-flow code, as returned by `POST /login/device/code`.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceCode {
    /// The opaque code the client polls with.
    pub device_code: String,
    /// The short code the user types at `verification_uri`.
    pub user_code: String,
    /// Where the user enters [`DeviceCode::user_code`].
    pub verification_uri: String,
    /// How long to wait between polls.
    pub interval: Duration,
    /// How long the code (and the whole flow) stays valid.
    pub expires_in: Duration,
}

/// What one poll of the token endpoint told us.
#[derive(Clone, Debug, PartialEq)]
pub enum PollOutcome {
    /// The user has not authorized yet; keep polling at the same interval.
    Pending,
    /// GitHub asked us to slow down (RFC 8628). A server-supplied interval, if
    /// present, replaces ours.
    SlowDown {
        /// The interval GitHub suggested, when it sent one.
        interval: Option<Duration>,
    },
    /// The user declined the request.
    Denied,
    /// The device code (or the whole flow) expired.
    Expired,
    /// A configuration problem that retrying cannot fix, carrying GitHub's
    /// `error` string (e.g. `device_flow_disabled`).
    Fatal(String),
    /// Success, carrying the access token.
    Token(String),
}

/// Why sign-in could not complete.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthFailure {
    /// The device code expired before the user authorized.
    Expired,
    /// The user declined the request.
    Denied,
    /// A configuration problem (bad client ID, device flow disabled, ...).
    Config(String),
    /// A transport or protocol failure while talking to GitHub.
    Http(String),
}

/// Where a sign-in attempt currently stands.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthState {
    /// Nothing started yet.
    Idle,
    /// A code has been shown and we are polling.
    Polling {
        /// The code being polled.
        code: DeviceCode,
        /// The current wait between polls.
        interval: Duration,
    },
    /// We have a token.
    Authenticated(String),
    /// The attempt ended in failure.
    Failed(AuthFailure),
}

/// Begin polling with `code`, adopting its server-provided interval.
pub fn start(code: DeviceCode) -> AuthState {
    let interval = code.interval;
    AuthState::Polling { code, interval }
}

/// Fold one poll result into the current state.
///
/// Only [`AuthState::Polling`] transitions; a terminal state stays put.
pub fn advance(state: AuthState, outcome: PollOutcome) -> AuthState {
    let AuthState::Polling { code, interval } = state else {
        return state;
    };

    match outcome {
        PollOutcome::Pending => AuthState::Polling { code, interval },
        PollOutcome::SlowDown { interval: new } => AuthState::Polling {
            code,
            interval: new.unwrap_or(interval + Duration::from_secs(5)),
        },
        PollOutcome::Denied => AuthState::Failed(AuthFailure::Denied),
        PollOutcome::Expired => AuthState::Failed(AuthFailure::Expired),
        PollOutcome::Fatal(message) => AuthState::Failed(AuthFailure::Config(message)),
        PollOutcome::Token(token) => AuthState::Authenticated(token),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn code(interval_secs: u64) -> DeviceCode {
        DeviceCode {
            device_code: "dc".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            interval: Duration::from_secs(interval_secs),
            expires_in: Duration::from_secs(900),
        }
    }

    fn interval(s: &AuthState) -> u64 {
        match s {
            AuthState::Polling { interval, .. } => interval.as_secs(),
            _ => panic!("not polling"),
        }
    }

    #[test]
    fn polling_progresses_and_backs_off_by_five_seconds() {
        let s = start(code(5));
        let s = advance(s, PollOutcome::Pending);
        assert_eq!(interval(&s), 5);
        let s = advance(s, PollOutcome::SlowDown { interval: None });
        assert_eq!(interval(&s), 10);
        let s = advance(s, PollOutcome::SlowDown { interval: None });
        assert_eq!(interval(&s), 15); // +5 s each time (RFC 8628), not doubling
        let s = advance(
            s,
            PollOutcome::SlowDown {
                interval: Some(Duration::from_secs(30)),
            },
        );
        assert_eq!(interval(&s), 30); // server-provided interval wins
        assert_eq!(
            advance(s, PollOutcome::Token("t".into())),
            AuthState::Authenticated("t".into())
        );
    }

    #[test]
    fn terminal_outcomes_fail() {
        assert_eq!(
            advance(start(code(5)), PollOutcome::Expired),
            AuthState::Failed(AuthFailure::Expired)
        );
        assert_eq!(
            advance(start(code(5)), PollOutcome::Denied),
            AuthState::Failed(AuthFailure::Denied)
        );
        assert!(matches!(
            advance(
                start(code(5)),
                PollOutcome::Fatal("device_flow_disabled".into())
            ),
            AuthState::Failed(AuthFailure::Config(_))
        ));
    }
}
