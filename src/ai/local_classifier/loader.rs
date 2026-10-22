//! What the Settings window and the review run are told about the local
//! classifier: where it is on its way from "switched on" to "ready".

/// Lifecycle of the local classifier.
#[derive(Clone, Debug, PartialEq)]
pub enum ClassifierState {
    /// The setting is off.
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
