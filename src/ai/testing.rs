//! Test doubles for the pipeline (spec §9.2–§9.3).
//!
//! Compiled for unit tests and, behind the `ai-testing` feature, for integration
//! tests (`tests/`) that drive the worker end to end.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;

use crate::ai::interaction::{ApprovalDecision, Interaction, Question};
use crate::ai::tools::AgentRef;
use crate::persist::settings::ModelSettings;

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

/// One scripted model turn: what the next [`ScriptedLlm`] `stream` call replays.
#[derive(Debug)]
pub enum ScriptStep {
    /// A single text block; the turn ends.
    Text(String),
    /// Tool calls, one block per call in order. Ids are `tu_<n>` counting up
    /// across the whole script, starting at 1.
    ToolCalls(Vec<(String /*name*/, serde_json::Value /*input*/)>),
    /// The call fails with this error; the stream yields it as its only event.
    Fail(review_llm::Error),
}

/// The queues and recordings of a [`ScriptedLlm`], behind one mutex.
#[derive(Debug, Default)]
struct LlmScript {
    /// The single-script queue (unused in per-agent mode).
    steps: VecDeque<ScriptStep>,
    /// Per-agent queues by label prefix (unused in single-script mode).
    agents: BTreeMap<String, VecDeque<ScriptStep>>,
    /// Every request received, in order.
    requests: Vec<review_llm::GenerateRequest>,
    /// Requests per agent key, in order (per-agent mode only).
    agent_requests: BTreeMap<String, Vec<review_llm::GenerateRequest>>,
    /// The next `tu_<n>` id; global across agents, starts at 1.
    next_tool_use: u64,
}

/// An [`Llm`](crate::ai::model::Llm) that replays a script and records every request.
///
/// Each `stream` call pops the next [`ScriptStep`] and replays it as a full
/// [`review_llm::StreamEvent`] sequence (`MessageStart`, per-block
/// `BlockStart` → delta → `BlockStop`, `Done`), so the agent loop under test
/// sees exactly the wire client's event shape. Running out of steps panics
/// with "script exhausted" — the test asked the model for more turns than it
/// scripted. Every received request is recorded for
/// [`ScriptedLlm::requests`].
///
/// [`ScriptedLlm::per_agent`] routes instead by the agent label: the request's
/// system prompt first line, `# agent: <label>` (Task 10's prompts always
/// emit it), is matched against the script keys and the longest key that is a
/// prefix of the label wins. Each key keeps its own queue and recording
/// ([`ScriptedLlm::requests_for`]); an unroutable label panics, because a
/// mis-wired prompt is a test bug.
#[derive(Debug)]
pub struct ScriptedLlm {
    model: ModelSettings,
    /// Whether `stream` routes by the `# agent: <label>` line.
    per_agent: bool,
    /// The `Usage` every scripted `Done` carries; zero by default, set with
    /// [`ScriptedLlm::usage_per_call`] by tests that watch token budgets.
    usage: review_llm::Usage,
    script: Mutex<LlmScript>,
}

impl ScriptedLlm {
    /// A model that replays `steps` to every caller, in order.
    pub fn new(model: ModelSettings, steps: Vec<ScriptStep>) -> ScriptedLlm {
        ScriptedLlm {
            model,
            per_agent: false,
            usage: review_llm::Usage::default(),
            script: Mutex::new(LlmScript {
                steps: steps.into(),
                next_tool_use: 1,
                ..LlmScript::default()
            }),
        }
    }

    /// A model with one script per agent, keyed by label prefix.
    pub fn per_agent(
        model: ModelSettings,
        scripts: BTreeMap<String /*label prefix*/, Vec<ScriptStep>>,
    ) -> ScriptedLlm {
        ScriptedLlm {
            model,
            per_agent: true,
            usage: review_llm::Usage::default(),
            script: Mutex::new(LlmScript {
                agents: scripts
                    .into_iter()
                    .map(|(prefix, steps)| (prefix, steps.into()))
                    .collect(),
                next_tool_use: 1,
                ..LlmScript::default()
            }),
        }
    }

    /// Report `input`/`output` tokens on every reply's `Done` usage, so a
    /// test can drive token-budget logic against a scripted model.
    pub fn usage_per_call(self, input: u64, output: u64) -> ScriptedLlm {
        ScriptedLlm {
            usage: review_llm::Usage {
                input,
                output,
                ..review_llm::Usage::default()
            },
            ..self
        }
    }

    /// Every request received, in order, across all agents.
    pub fn requests(&self) -> Vec<review_llm::GenerateRequest> {
        self.lock().requests.clone()
    }

    /// The requests routed to the script keyed `prefix`, in order.
    pub fn requests_for(&self, prefix: &str) -> Vec<review_llm::GenerateRequest> {
        self.lock()
            .agent_requests
            .get(prefix)
            .cloned()
            .unwrap_or_default()
    }

    /// Queue one more step for the agent keyed `prefix` (per-agent mode).
    ///
    /// A test seam for replies that depend on a value only known at runtime:
    /// a question id, a reviewer's agent id. The key is created on demand,
    /// so a test can script the very first chat turn after learning the id.
    pub fn push_agent_step(&self, prefix: &str, step: ScriptStep) {
        assert!(
            self.per_agent,
            "push_agent_step needs a per-agent ScriptedLlm"
        );
        self.lock()
            .agents
            .entry(prefix.to_string())
            .or_default()
            .push_back(step);
    }

    /// Never held across an `.await`, and poisoning is recovered rather than
    /// propagated, as in [`ScriptedInteraction::lock`].
    fn lock(&self) -> MutexGuard<'_, LlmScript> {
        self.script.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The agent label from a system prompt's first line, `# agent: <label>`.
fn agent_label(system: Option<&str>) -> Option<&str> {
    system?
        .lines()
        .next()?
        .strip_prefix("# agent: ")
        .map(str::trim)
}

/// The `CallMeta` of a scripted reply: nothing actually served the call.
fn scripted_meta(model: &str) -> review_llm::CallMeta {
    review_llm::CallMeta {
        api: review_llm::ApiType::AnthropicMessages,
        provider: "scripted".into(),
        model: model.to_string(),
        latency: std::time::Duration::ZERO,
    }
}

/// The full event sequence for one scripted step. `first_tool_use` is the
/// `tu_<n>` id of the step's first tool call; `usage` is what the `Done`
/// reply reports.
fn step_events(
    step: &ScriptStep,
    first_tool_use: u64,
    model: &str,
    usage: &review_llm::Usage,
) -> Vec<review_llm::StreamEvent> {
    use review_llm::{BlockStart, ContentBlock, Reply, StopReason, StreamEvent, Usage};

    let mut events = vec![StreamEvent::MessageStart {
        id: None,
        model: model.to_string(),
        usage: Usage::default(),
    }];
    let (content, stop) = match step {
        ScriptStep::Text(text) => {
            events.push(StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text,
            });
            events.push(StreamEvent::TextDelta {
                index: 0,
                text: text.clone(),
            });
            let block = ContentBlock::text(text.clone());
            events.push(StreamEvent::BlockStop {
                index: 0,
                block: block.clone(),
            });
            (vec![block], StopReason::EndTurn)
        }
        ScriptStep::ToolCalls(calls) => {
            let mut content = Vec::with_capacity(calls.len());
            for (index, (name, input)) in calls.iter().enumerate() {
                let id = format!("tu_{}", first_tool_use + index as u64);
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                    },
                });
                events.push(StreamEvent::ToolInputDelta {
                    index,
                    partial_json: input.to_string(),
                });
                let block = ContentBlock::ToolUse {
                    id,
                    name: name.clone(),
                    input: input.clone(),
                };
                events.push(StreamEvent::BlockStop {
                    index,
                    block: block.clone(),
                });
                content.push(block);
            }
            (content, StopReason::ToolUse)
        }
        ScriptStep::Fail(_) => unreachable!("Fail replays its error instead"),
    };
    events.push(StreamEvent::Done(Reply {
        content,
        stop,
        usage: *usage,
        meta: scripted_meta(model),
    }));
    events
}

#[async_trait]
impl crate::ai::model::Llm for ScriptedLlm {
    async fn stream(
        &self,
        req: review_llm::GenerateRequest,
    ) -> review_llm::Result<BoxStream<'static, review_llm::Result<review_llm::StreamEvent>>> {
        let (step, first_tool_use, model) = {
            let mut script = self.lock();
            script.requests.push(req.clone());
            let (queue, who) = if self.per_agent {
                let label = agent_label(req.system.as_deref()).unwrap_or_else(|| {
                    panic!(
                        "scripted llm: per-agent script needs a `# agent: <label>` first line, \
                         got {:?}",
                        req.system
                    )
                });
                let key = script
                    .agents
                    .keys()
                    .filter(|prefix| label.starts_with(prefix.as_str()))
                    .max_by_key(|prefix| prefix.len())
                    .cloned()
                    .unwrap_or_else(|| panic!("scripted llm: no script for agent `{label}`"));
                script
                    .agent_requests
                    .entry(key.clone())
                    .or_default()
                    .push(req.clone());
                (
                    script.agents.get_mut(&key).expect("key just matched"),
                    format!(" for agent `{key}`"),
                )
            } else {
                (&mut script.steps, String::new())
            };
            let step = queue
                .pop_front()
                .unwrap_or_else(|| panic!("scripted llm: script exhausted{who}"));
            let first_tool_use = match &step {
                ScriptStep::ToolCalls(calls) => {
                    let id = script.next_tool_use;
                    script.next_tool_use += calls.len() as u64;
                    id
                }
                _ => 0,
            };
            let model = req.model.clone().unwrap_or_else(|| "scripted".to_string());
            (step, first_tool_use, model)
        };
        let events = match step {
            ScriptStep::Fail(e) => vec![Err(e)],
            step => step_events(&step, first_tool_use, &model, &self.usage)
                .into_iter()
                .map(Ok)
                .collect(),
        };
        Ok(futures::stream::iter(events).boxed())
    }

    fn model(&self) -> &ModelSettings {
        &self.model
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

#[cfg(test)]
mod scripted_llm_tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use futures::StreamExt as _;
    use serde_json::json;

    use review_llm::{
        ApiType, BlockStart, CallMeta, ContentBlock, GenerateRequest, Message, Reply, StopReason,
        StreamEvent, Usage,
    };

    use super::{ScriptStep, ScriptedLlm};
    use crate::ai::model::Llm as _;
    use crate::persist::settings::ModelSettings;

    fn model() -> ModelSettings {
        ModelSettings {
            provider: "work".into(),
            model: "claude-test".into(),
            context_window: 100_000,
            max_tokens: 4096,
            reasoning: None,
        }
    }

    fn req(text: &str) -> GenerateRequest {
        GenerateRequest::new(vec![Message::user_text(text)])
    }

    /// A request whose system prompt carries Task 10's agent label.
    fn agent_req(label: &str, text: &str) -> GenerateRequest {
        let mut r = req(text);
        r.system = Some(format!("# agent: {label}\nrest of the prompt"));
        r
    }

    fn meta(model: &str) -> CallMeta {
        CallMeta {
            api: ApiType::AnthropicMessages,
            provider: "scripted".into(),
            model: model.into(),
            latency: Duration::ZERO,
        }
    }

    /// Drains one `stream` call, panicking on an `Err` event.
    async fn events(llm: &ScriptedLlm, request: GenerateRequest) -> Vec<StreamEvent> {
        llm.stream(request)
            .await
            .expect("stream opens")
            .map(|e| e.expect("ok event"))
            .collect()
            .await
    }

    fn text_events(text: &str) -> Vec<StreamEvent> {
        let block = ContentBlock::text(text);
        vec![
            StreamEvent::MessageStart {
                id: None,
                model: "scripted".into(),
                usage: Usage::default(),
            },
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: text.into(),
            },
            StreamEvent::BlockStop {
                index: 0,
                block: block.clone(),
            },
            StreamEvent::Done(Reply {
                content: vec![block],
                stop: StopReason::EndTurn,
                usage: Usage::default(),
                meta: meta("scripted"),
            }),
        ]
    }

    #[tokio::test]
    async fn scripted_llm_emits_tool_calls_and_records_requests() {
        let llm = ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("hello".into()),
                ScriptStep::ToolCalls(vec![("done".into(), json!({"summary": "x"}))]),
                ScriptStep::Fail(review_llm::Error::Timeout),
            ],
        );

        assert_eq!(events(&llm, req("one")).await, text_events("hello"));

        let call = ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "done".into(),
            input: json!({"summary": "x"}),
        };
        assert_eq!(
            events(&llm, req("two")).await,
            vec![
                StreamEvent::MessageStart {
                    id: None,
                    model: "scripted".into(),
                    usage: Usage::default(),
                },
                StreamEvent::BlockStart {
                    index: 0,
                    block: BlockStart::ToolUse {
                        id: "tu_1".into(),
                        name: "done".into(),
                    },
                },
                StreamEvent::ToolInputDelta {
                    index: 0,
                    partial_json: "{\"summary\":\"x\"}".into(),
                },
                StreamEvent::BlockStop {
                    index: 0,
                    block: call.clone(),
                },
                StreamEvent::Done(Reply {
                    content: vec![call],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                    meta: meta("scripted"),
                }),
            ]
        );

        // The failing step yields exactly one error event.
        let failed: Vec<_> = llm
            .stream(req("three"))
            .await
            .expect("stream opens")
            .collect()
            .await;
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].as_ref().err().map(|e| e.to_string()),
            Some("request timed out".into())
        );

        // All three requests are recorded, in order.
        let recorded = llm.requests();
        assert_eq!(recorded.len(), 3);
        assert_eq!(recorded, vec![req("one"), req("two"), req("three")]);
    }

    #[tokio::test]
    #[should_panic(expected = "script exhausted")]
    async fn scripted_llm_panics_when_script_exhausted() {
        let llm = ScriptedLlm::new(model(), vec![ScriptStep::Text("only".into())]);
        let _ = events(&llm, req("one")).await;
        let _ = events(&llm, req("two")).await;
    }

    #[tokio::test]
    async fn per_agent_routes_by_label_prefix() {
        let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
        scripts.insert("scout".into(), vec![ScriptStep::Text("from scout".into())]);
        scripts.insert(
            "scout senior".into(),
            vec![ScriptStep::Text("from senior".into())],
        );
        scripts.insert(
            "verifier".into(),
            vec![ScriptStep::ToolCalls(vec![("report".into(), json!({}))])],
        );
        let llm = ScriptedLlm::per_agent(model(), scripts);

        // The longest matching key wins: "scout senior 3" is the senior scout.
        assert_eq!(
            events(&llm, agent_req("scout senior 3", "a")).await,
            text_events("from senior")
        );
        assert_eq!(
            events(&llm, agent_req("scout 2", "b")).await,
            text_events("from scout")
        );

        // The verifier gets its own queue; its tool ids still count from 1.
        let call = ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "report".into(),
            input: json!({}),
        };
        assert_eq!(
            events(&llm, agent_req("verifier 1", "c")).await,
            vec![
                StreamEvent::MessageStart {
                    id: None,
                    model: "scripted".into(),
                    usage: Usage::default(),
                },
                StreamEvent::BlockStart {
                    index: 0,
                    block: BlockStart::ToolUse {
                        id: "tu_1".into(),
                        name: "report".into(),
                    },
                },
                StreamEvent::ToolInputDelta {
                    index: 0,
                    partial_json: "{}".into(),
                },
                StreamEvent::BlockStop {
                    index: 0,
                    block: call.clone(),
                },
                StreamEvent::Done(Reply {
                    content: vec![call],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                    meta: meta("scripted"),
                }),
            ]
        );

        // Recording is per agent as well as global.
        assert_eq!(llm.requests_for("scout"), vec![agent_req("scout 2", "b")]);
        assert_eq!(
            llm.requests_for("scout senior"),
            vec![agent_req("scout senior 3", "a")]
        );
        assert_eq!(llm.requests().len(), 3);
    }

    #[tokio::test]
    #[should_panic(expected = "no script for agent `author 1`")]
    async fn per_agent_panics_on_unrouted_label() {
        let mut scripts: BTreeMap<String, Vec<ScriptStep>> = BTreeMap::new();
        scripts.insert("scout".into(), vec![ScriptStep::Text("hi".into())]);
        let llm = ScriptedLlm::per_agent(model(), scripts);
        let _ = events(&llm, agent_req("author 1", "a")).await;
    }
}
