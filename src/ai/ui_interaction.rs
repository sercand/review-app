//! The [`Interaction`] a live run talks to the UI through (spec §10.4, §10.7).
//!
//! [`TrackingInteraction`](crate::ai::pipeline) wraps the pipeline's
//! interaction to persist questions and count them; the hub hands it this,
//! which turns each call into an [`Event::AiQuestion`] or
//! [`Event::AiApprovalRequest`] and parks a oneshot until the matching
//! [`Command::AiAnswer`] / [`Command::AiApprove`] arrives. The hub owns one
//! instance per run and shares it with those commands, so an answer travels
//! back through the parked sender and the agent resumes mid-turn.
//!
//! Questions are keyed by [`Question::id`] (assigned process-wide in
//! [`crate::ai::tools`]); approvals and prepare requests by ids this type
//! issues alongside the events it emits. A run cancelled or closed while
//! parked gets what [`NoInteraction`] would have returned — an empty answer,
//! a denial, no prepare — so the agent finishes cleanly instead of hanging.
//!
//! Events carry the run's *current* [`SessionId`], read from the shared cell
//! the hub rebinds on [`Command::AiAttach`] (spec §10.2), so a prompt that
//! arrives after the user left and re-entered the PR still reaches the UI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::ai::interaction::{ApprovalDecision, Interaction, Question};
use crate::ai::tools::AgentRef;
use crate::runtime::{Event, EventSender, SessionId};

/// What the UI is being asked to approve (spec §10.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    /// A single sandboxed shell command (spec §8.1).
    Command { command: String, reason: String },
    /// The repo's prepare commands, which would run *outside* the sandbox
    /// as the user (spec §8.5). `commands` is the configured list.
    Prepare {
        reason: String,
        commands: Vec<String>,
    },
}

enum Pending {
    Decision(oneshot::Sender<ApprovalDecision>),
    Prepare(oneshot::Sender<bool>),
}

/// One `ask_user` call waiting for an answer.
struct ParkedQuestion {
    tx: oneshot::Sender<String>,
    /// The question text, for the chat digest (spec §6.5).
    text: String,
}

/// The live UI side of [`Interaction`], shared between the hub and the
/// pipeline's tasks.
pub struct UiInteraction {
    events: EventSender,
    /// The run's current session, rebound by `AiAttach`.
    session: Arc<Mutex<SessionId>>,
    /// The repo's configured prepare commands, shown with a prepare request.
    prepare_commands: Vec<String>,
    questions: Mutex<HashMap<u64, ParkedQuestion>>,
    approvals: Mutex<HashMap<u64, Pending>>,
    next_approval: AtomicU64,
}

impl UiInteraction {
    /// A UI interaction for one run; `session` is the shared cell the hub
    /// rebinds, `prepare_commands` the repo's configured prepare list.
    pub fn new(
        events: EventSender,
        session: Arc<Mutex<SessionId>>,
        prepare_commands: Vec<String>,
    ) -> Arc<UiInteraction> {
        Arc::new(UiInteraction {
            events,
            session,
            prepare_commands,
            questions: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            next_approval: AtomicU64::new(0),
        })
    }

    fn current_session(&self) -> SessionId {
        *self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_id(&self) -> u64 {
        self.next_approval.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Answer a parked question. `false` means nothing was waiting for it:
    /// the run moved on, was cancelled, or the id belongs to an approval.
    pub fn answer(&self, question_id: u64, text: String) -> bool {
        match self
            .questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&question_id)
        {
            Some(parked) => parked.tx.send(text).is_ok(),
            None => false,
        }
    }

    /// Every question still waiting for an answer, by id, for the chat
    /// agent's status digest (spec §6.5). Sorted so the digest is stable.
    pub fn open_questions(&self) -> Vec<(u64, String)> {
        let mut open: Vec<(u64, String)> = self
            .questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, parked)| (*id, parked.text.clone()))
            .collect();
        open.sort_by_key(|(id, _)| *id);
        open
    }

    /// Decide a parked command approval. `false` when nothing is waiting.
    pub fn approve(&self, approval_id: u64, decision: ApprovalDecision) -> bool {
        match self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&approval_id)
        {
            Some(Pending::Decision(tx)) => tx.send(decision).is_ok(),
            Some(other) => {
                self.approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(approval_id, other);
                false
            }
            None => false,
        }
    }

    /// Decide a parked prepare approval. `false` when nothing is waiting.
    pub fn decide_prepare(&self, approval_id: u64, ok: bool) -> bool {
        match self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&approval_id)
        {
            Some(Pending::Prepare(tx)) => tx.send(ok).is_ok(),
            Some(other) => {
                self.approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(approval_id, other);
                false
            }
            None => false,
        }
    }

    /// Resolve everything still parked the way [`NoInteraction`] would, so a
    /// cancelled or closed run's agents can finish.
    pub fn cancel_all(&self) {
        let questions: Vec<oneshot::Sender<String>> = {
            self.questions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain()
                .map(|(_, parked)| parked.tx)
                .collect()
        };
        for tx in questions {
            let _ = tx.send(String::new());
        }
        let approvals: Vec<Pending> = {
            self.approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain()
                .map(|(_, parked)| parked)
                .collect()
        };
        for parked in approvals {
            match parked {
                Pending::Decision(tx) => {
                    let _ = tx.send(ApprovalDecision::Deny { reason: None });
                }
                Pending::Prepare(tx) => {
                    let _ = tx.send(false);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Interaction for UiInteraction {
    async fn ask_user(&self, agent: &AgentRef, question: Question) -> String {
        let (tx, rx) = oneshot::channel();
        self.questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                question.id,
                ParkedQuestion {
                    tx,
                    text: question.text.clone(),
                },
            );
        self.events.send(Event::AiQuestion {
            session: self.current_session(),
            id: question.id,
            agent: agent.clone(),
            text: question.text.clone(),
            options: question.options.clone(),
        });
        // A dropped sender means `cancel_all` resolved it already.
        rx.await.unwrap_or_default()
    }

    async fn approve_command(
        &self,
        agent: &AgentRef,
        command: &str,
        reason: &str,
    ) -> ApprovalDecision {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Pending::Decision(tx));
        self.events.send(Event::AiApprovalRequest {
            session: self.current_session(),
            id,
            agent: agent.clone(),
            kind: ApprovalKind::Command {
                command: command.to_string(),
                reason: reason.to_string(),
            },
        });
        rx.await.unwrap_or(ApprovalDecision::Deny { reason: None })
    }

    async fn approve_prepare(&self, agent: &AgentRef, reason: &str) -> bool {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Pending::Prepare(tx));
        self.events.send(Event::AiApprovalRequest {
            session: self.current_session(),
            id,
            agent: agent.clone(),
            kind: ApprovalKind::Prepare {
                reason: reason.to_string(),
                commands: self.prepare_commands.clone(),
            },
        });
        rx.await.unwrap_or(false)
    }
}
