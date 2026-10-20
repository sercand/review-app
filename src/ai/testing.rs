//! Test doubles for the pipeline (spec §9.2–§9.3).
//!
//! Compiled for unit tests and, behind the `ai-testing` feature, for integration
//! tests (`tests/`) that drive the worker end to end. Task 8 adds `ScriptedLlm`
//! here.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::ai::interaction::{ApprovalDecision, Interaction, Question};
use crate::ai::tools::AgentRef;

/// One call made to a [`ScriptedInteraction`], in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractionCall {
    /// `ask_user` was called with this question.
    Ask {
        /// The agent that asked.
        agent: AgentRef,
        /// The question it asked.
        question: Question,
    },
    /// `approve_command` was called for this command.
    Approve {
        /// The agent that asked.
        agent: AgentRef,
        /// The command it wants to run.
        command: String,
        /// The classifier's reason.
        reason: String,
    },
    /// `approve_prepare` was called.
    Prepare {
        /// The agent that asked.
        agent: AgentRef,
        /// Why it wants the prepare commands.
        reason: String,
    },
}

/// What the scripted user has queued: one entry per future call, plus the log.
#[derive(Debug, Default)]
struct Script {
    answers: VecDeque<String>,
    decisions: VecDeque<ApprovalDecision>,
    prepares: VecDeque<bool>,
    calls: Vec<InteractionCall>,
}

/// An [`Interaction`] that answers from queues and records every call.
///
/// Queue one answer per `ask_user`, one [`ApprovalDecision`] per
/// `approve_command` and one `bool` per `approve_prepare`, in call order (the
/// builders return `Self`, so they chain off [`ScriptedInteraction::new`]). An
/// exhausted queue falls back to the "cannot ask" defaults: an empty answer,
/// [`ApprovalDecision::Deny`] with no reason, and `false`. Every call is
/// appended to [`ScriptedInteraction::calls`], so a test can assert that a
/// silent command asked nothing — including how many times it asked.
#[derive(Debug, Default)]
pub struct ScriptedInteraction {
    script: Mutex<Script>,
}

impl ScriptedInteraction {
    /// A scripted user with empty queues.
    pub fn new() -> ScriptedInteraction {
        ScriptedInteraction::default()
    }

    /// Queues the answer to the next `ask_user`.
    pub fn answer(self, text: impl Into<String>) -> ScriptedInteraction {
        self.lock().answers.push_back(text.into());
        self
    }

    /// Queues the decision for the next `approve_command`.
    pub fn approve(self, decision: ApprovalDecision) -> ScriptedInteraction {
        self.lock().decisions.push_back(decision);
        self
    }

    /// Queues the answer to the next `approve_prepare`.
    pub fn prepare(self, allowed: bool) -> ScriptedInteraction {
        self.lock().prepares.push_back(allowed);
        self
    }

    /// Every call so far, oldest first.
    pub fn calls(&self) -> Vec<InteractionCall> {
        self.lock().calls.clone()
    }

    /// The recorded calls in order. Never held across an `.await`: a panic
    /// while recording must not wedge the next test either, so poisoning is
    /// recovered rather than propagated.
    fn lock(&self) -> MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl Interaction for ScriptedInteraction {
    async fn ask_user(&self, agent: &AgentRef, q: Question) -> String {
        let mut script = self.lock();
        script.calls.push(InteractionCall::Ask {
            agent: agent.clone(),
            question: q,
        });
        script.answers.pop_front().unwrap_or_default()
    }

    async fn approve_command(
        &self,
        agent: &AgentRef,
        command: &str,
        reason: &str,
    ) -> ApprovalDecision {
        let mut script = self.lock();
        script.calls.push(InteractionCall::Approve {
            agent: agent.clone(),
            command: command.to_string(),
            reason: reason.to_string(),
        });
        script
            .decisions
            .pop_front()
            .unwrap_or(ApprovalDecision::Deny { reason: None })
    }

    async fn approve_prepare(&self, agent: &AgentRef, reason: &str) -> bool {
        let mut script = self.lock();
        script.calls.push(InteractionCall::Prepare {
            agent: agent.clone(),
            reason: reason.to_string(),
        });
        script.prepares.pop_front().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{InteractionCall, ScriptedInteraction};
    use crate::ai::interaction::{ApprovalDecision, Interaction, Question};
    use crate::ai::tools::{AgentRef, AgentRole};

    fn agent() -> AgentRef {
        AgentRef {
            id: 2,
            role: AgentRole::Reviewer,
            label: "reviewer 2".to_string(),
        }
    }

    #[tokio::test]
    async fn empty_queues_fall_back_to_denying() {
        let user = ScriptedInteraction::new();
        let agent = agent();
        let question = Question {
            id: 7,
            text: "is empty `links` allowed?".to_string(),
            options: vec!["yes".to_string(), "no".to_string()],
        };

        assert_eq!(user.ask_user(&agent, question.clone()).await, "");
        assert_eq!(
            user.approve_command(&agent, "rm -rf /", "rm -rf /: not allowed")
                .await,
            ApprovalDecision::Deny { reason: None }
        );
        assert!(!user.approve_prepare(&agent, "it wants the build").await);
        assert_eq!(
            user.calls(),
            vec![
                InteractionCall::Ask {
                    agent: agent.clone(),
                    question,
                },
                InteractionCall::Approve {
                    agent: agent.clone(),
                    command: "rm -rf /".to_string(),
                    reason: "rm -rf /: not allowed".to_string(),
                },
                InteractionCall::Prepare {
                    agent,
                    reason: "it wants the build".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn queued_answers_are_used_in_order() {
        let user = ScriptedInteraction::new()
            .answer("first")
            .answer("second")
            .approve(ApprovalDecision::Once)
            .approve(ApprovalDecision::ForReview)
            .prepare(true);
        let agent = agent();
        let ask = |id| Question {
            id,
            text: "q".to_string(),
            options: Vec::new(),
        };

        assert_eq!(user.ask_user(&agent, ask(1)).await, "first");
        assert_eq!(user.ask_user(&agent, ask(2)).await, "second");
        assert_eq!(
            user.approve_command(&agent, "a", "r").await,
            ApprovalDecision::Once
        );
        assert_eq!(
            user.approve_command(&agent, "b", "r").await,
            ApprovalDecision::ForReview
        );
        assert!(user.approve_prepare(&agent, "r").await);
        // The queue is empty again: back to the defaults.
        assert_eq!(
            user.approve_command(&agent, "c", "r").await,
            ApprovalDecision::Deny { reason: None }
        );
        assert_eq!(user.calls().len(), 6);
    }
}
