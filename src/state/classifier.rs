//! Where the local command classifier is on its way from "switched on" to
//! "ready", and how Settings words it. Pure and build-independent: without the
//! `local-classifier` feature (or off Unix) nothing ever produces a state and
//! [`ClassifierState::text`] says the build has no classifier.

/// Lifecycle of the local classifier.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ClassifierState {
    /// The setting is off.
    #[default]
    Off,
    /// On, but the weights are not on disk.
    NotInstalled,
    Downloading {
        done: u64,
        total: u64,
    },
    /// Creating the backend and uploading the weights.
    Loading,
    Ready,
    /// No GPU, a backend error, or disabled after repeated failures.
    Unavailable(String),
    /// A download or load error; retryable.
    Failed(String),
}

/// Whether this build contains the classifier at all.
pub const COMPILED: bool = cfg!(all(unix, feature = "local-classifier"));

impl ClassifierState {
    /// The status line Settings shows next to the checkbox.
    pub fn text(&self, compiled: bool) -> String {
        if !compiled {
            return "Not included in this build".to_owned();
        }
        match self {
            ClassifierState::Off => "Off".to_owned(),
            ClassifierState::NotInstalled => "Model not downloaded (≈135 MB)".to_owned(),
            ClassifierState::Downloading { done, total } if *total > 0 => {
                format!(
                    "Downloading… {} %",
                    (done.saturating_mul(100) / total).min(100)
                )
            }
            ClassifierState::Downloading { .. } => "Downloading…".to_owned(),
            ClassifierState::Loading => {
                "Loading on GPU… (first start can take a minute)".to_owned()
            }
            ClassifierState::Ready => "Ready".to_owned(),
            ClassifierState::Unavailable(why) | ClassifierState::Failed(why) => why.clone(),
        }
    }

    /// Whether the checkbox may be edited: the classifier is compiled in and
    /// the device is not known to be unusable.
    pub fn available(&self, compiled: bool) -> bool {
        compiled && !matches!(self, ClassifierState::Unavailable(_))
    }

    /// Whether Settings offers the Download / Retry button.
    pub fn can_download(&self, compiled: bool) -> bool {
        compiled
            && matches!(
                self,
                ClassifierState::NotInstalled | ClassifierState::Failed(_)
            )
    }
}

#[cfg(test)]
mod tests {
    use super::ClassifierState::*;

    #[test]
    fn state_text_table() {
        let rows = [
            (Off, "Off"),
            (NotInstalled, "Model not downloaded (≈135 MB)"),
            (Downloading { done: 0, total: 0 }, "Downloading…"),
            (
                Downloading {
                    done: 42,
                    total: 100,
                },
                "Downloading… 42 %",
            ),
            (Loading, "Loading on GPU… (first start can take a minute)"),
            (Ready, "Ready"),
            (Unavailable("no usable GPU".into()), "no usable GPU"),
            (Failed("offline".into()), "offline"),
        ];
        for (state, text) in rows {
            assert_eq!(state.text(true), text, "{state:?}");
            assert_eq!(state.text(false), "Not included in this build", "{state:?}");
        }
    }

    #[test]
    fn download_button_only_when_not_installed_or_failed() {
        let states = [
            Off,
            NotInstalled,
            Downloading { done: 1, total: 2 },
            Loading,
            Ready,
            Unavailable("x".into()),
            Failed("x".into()),
        ];
        for state in states {
            let expected = matches!(state, NotInstalled | Failed(_));
            assert_eq!(state.can_download(true), expected, "{state:?}");
            assert!(!state.can_download(false), "{state:?}");
        }
    }

    #[test]
    fn available_unless_unavailable_or_not_compiled() {
        assert!(Failed("x".into()).available(true));
        assert!(Downloading { done: 0, total: 0 }.available(true));
        assert!(Loading.available(true));
        assert!(!Unavailable("x".into()).available(true));
        assert!(!Ready.available(false));
    }
}
