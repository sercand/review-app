//! How an agent talks to the user: `ask_user` questions and the two approvals
//! that can block a tool call — a shell command (spec §8.1) and a prepare
//! command (spec §8.5).
//!
//! The pipeline runs in the worker, the user is in the UI, so every method is
//! async and answers when the user does (spec §9.2). [`NoInteraction`] is the
//! stand-in for contexts that cannot ask (headless tests, no UI attached).

use async_trait::async_trait;

use crate::ai::tools::AgentRef;

/// A question an agent asks the user through `ask_user` (spec §6.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The run-wide id the UI answers with.
    pub id: u64,
    /// The question text.
    pub text: String,
    /// Quick-reply choices; empty means free text.
    pub options: Vec<String>,
}

/// What the user decided about a command that needed approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Run it this once; ask again the next time it comes up.
    Once,
    /// Run it and stop asking for it for the rest of this review.
    ForReview,
    /// Do not run it.
    Deny {
        /// Why, when the user typed a reason.
        reason: Option<String>,
    },
}

/// The user, as an agent sees them.
///
/// Implementations are shared across agents (`Arc<dyn Interaction>`), so every
/// method takes `&self`. An unanswered question resolves to an empty string and
/// an unanswered approval is [`ApprovalDecision::Deny`]: when in doubt, nothing
/// runs.
#[async_trait]
pub trait Interaction: Send + Sync {
    /// Asks the user `q` on `agent`'s behalf and returns their answer (empty
    /// when they dismiss it).
    async fn ask_user(&self, agent: &AgentRef, q: Question) -> String;

    /// Asks whether `agent` may run `command`; `reason` is the classifier's
    /// explanation of why it needs approval.
    async fn approve_command(
        &self,
        agent: &AgentRef,
        command: &str,
        reason: &str,
    ) -> ApprovalDecision;

    /// Asks whether `agent` may run the repo's prepare commands; `reason` says
    /// why it wants them.
    async fn approve_prepare(&self, agent: &AgentRef, reason: &str) -> bool;
}

/// An [`Interaction`] for contexts that cannot ask: headless tool tests, or a
/// run with no UI attached.
///
/// Nothing is ever approved — an empty answer, [`ApprovalDecision::Deny`] with
/// no reason, and no prepare — so a gated tool refuses instead of hanging.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoInteraction;

#[async_trait]
impl Interaction for NoInteraction {
    async fn ask_user(&self, _agent: &AgentRef, _q: Question) -> String {
        String::new()
    }

    async fn approve_command(
        &self,
        _agent: &AgentRef,
        _command: &str,
        _reason: &str,
    ) -> ApprovalDecision {
        ApprovalDecision::Deny { reason: None }
    }

    async fn approve_prepare(&self, _agent: &AgentRef, _reason: &str) -> bool {
        false
    }
}
