//! The chat agent's pipeline-steering tools (spec §6.5, Task 7).
//!
//! These tools are not in [`crate::ai::tools::all`]: each one needs a
//! [`SteerCtx`] — the live [`PipelineControl`], the run's liveness flag, the
//! parked questions, and the store — so they are built with [`tools`]
//! when the chat agent is spawned. Nothing here writes to the repo or to
//! GitHub: steering goes through the pipeline's own control handle and
//! findings through the run's [`crate::ai::review::RunState`].
//!
//! After the run ([`SteerCtx::is_live`] is `false`), the tools that would
//! change the pipeline answer [`FINISHED`] — the run has nothing left to
//! steer. The reads (`pipeline_status`, `get_finding`, `get_agent_log`)
//! keep working, so the follow-up chat can explain a finished review.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use review_llm::ContentBlock;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::pipeline::{GroupState, PipelineControl, PipelineStatus};
use crate::ai::review::Finding;
use crate::ai::store::Store;
use crate::ai::ui_interaction::UiInteraction;

/// What every steering tool says once the run is over.
pub const FINISHED: &str = "the review has finished";

/// How many transcript messages `get_agent_log` shows by default.
const DEFAULT_LOG_MESSAGES: usize = 20;
/// How much of one transcript text block the log shows.
const MAX_LOG_TEXT: usize = 400;
/// How much of one finding body `get_finding` shows.
const MAX_FINDING_BODY: usize = 2_000;

/// Everything the steering tools read and write (spec §6.5).
pub struct SteerCtx {
    /// The run's pipeline handle; `None` only for a chat with no run behind
    /// it.
    control: Option<PipelineControl>,
    /// Whether the pipeline is still running; the hub's forwarder clears it
    /// when the event channel closes.
    live: Arc<AtomicBool>,
    /// The run's parked questions, for `answer_question`.
    interaction: Arc<UiInteraction>,
    /// The AI database.
    store: Arc<Store>,
    /// The PR's session row.
    session_id: i64,
    /// The run whose transcript and findings the tools read.
    run_id: i64,
}

impl SteerCtx {
    /// A steering context over one run.
    pub fn new(
        control: Option<PipelineControl>,
        live: Arc<AtomicBool>,
        interaction: Arc<UiInteraction>,
        store: Arc<Store>,
        session_id: i64,
        run_id: i64,
    ) -> SteerCtx {
        SteerCtx {
            control,
            live,
            interaction,
            store,
            session_id,
            run_id,
        }
    }

    /// Whether the pipeline still has work to steer.
    pub fn is_live(&self) -> bool {
        self.control.is_some() && self.live.load(Ordering::SeqCst)
    }

    /// The run's control handle, or [`FINISHED`] once the run ended.
    fn live_control(&self) -> Result<&PipelineControl, ToolOutput> {
        match &self.control {
            Some(control) if self.is_live() => Ok(control),
            _ => Err(ToolOutput::error(FINISHED)),
        }
    }

    /// The user-role preamble every chat turn gets (spec §6.5): the compact
    /// run status plus the brief's intent, never a transcript.
    pub fn digest(&self) -> String {
        let status = self.control.as_ref().map(PipelineControl::status);
        let paused = self
            .control
            .as_ref()
            .is_some_and(PipelineControl::is_paused);
        let questions = self.interaction.open_questions();
        let intent = self
            .store
            .session_brief(self.session_id)
            .ok()
            .flatten()
            .and_then(|json| {
                serde_json::from_str::<crate::ai::stages::scout::Brief>(&json)
                    .ok()
                    .map(|brief| brief.intent)
            });
        status_digest(
            status.as_ref(),
            &questions,
            intent.as_deref(),
            self.is_live(),
            paused,
        )
    }
}

/// Renders the chat's status digest. Pure, so its shape is unit-tested.
pub fn status_digest(
    status: Option<&PipelineStatus>,
    questions: &[(u64, String)],
    intent: Option<&str>,
    live: bool,
    paused: bool,
) -> String {
    let mut out = String::from("## Run status\n\n");
    out.push_str(&format!(
        "state: {}\n",
        match (live, paused) {
            (false, _) => "finished",
            (true, true) => "running, paused",
            (true, false) => "running",
        }
    ));
    if let Some(intent) = intent.map(one_line).filter(|intent| !intent.is_empty()) {
        out.push_str(&format!("intent: {intent}\n"));
    }
    match status {
        Some(status) => {
            if !status.stages.is_empty() {
                let stages: Vec<String> = status
                    .stages
                    .iter()
                    .map(|(stage, state)| format!("{} {}", stage_name(*stage), stage_state(state)))
                    .collect();
                out.push_str(&format!("stages: {}\n", stages.join(", ")));
            }
            if !status.groups.is_empty() {
                let groups: Vec<String> = status
                    .groups
                    .iter()
                    .map(|(id, state)| format!("{id} {}", group_state(*state)))
                    .collect();
                out.push_str(&format!("groups: {}\n", groups.join(", ")));
            }
            if !status.running_agents.is_empty() {
                let running: Vec<String> = status
                    .running_agents
                    .iter()
                    .map(|agent| format!("{} (id {})", agent.label, agent.id))
                    .collect();
                out.push_str(&format!("running: {}\n", running.join(", ")));
            }
            if !status.findings.is_empty() {
                let findings: Vec<String> = status
                    .findings
                    .iter()
                    .map(|(severity, state, count)| {
                        format!("{} {} {count}", severity.as_str(), state.as_str())
                    })
                    .collect();
                out.push_str(&format!("findings: {}\n", findings.join(", ")));
            }
        }
        None => out.push_str("no pipeline is attached\n"),
    }
    if questions.is_empty() {
        out.push_str("open questions: none\n");
    } else {
        let listed: Vec<String> = questions
            .iter()
            .map(|(id, text)| format!("#{id} \"{}\"", one_line(text)))
            .collect();
        out.push_str(&format!("open questions: {}\n", listed.join(", ")));
    }
    out
}

/// One line of `text`, with runs of whitespace collapsed and the length cut.
fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(200).collect()
}

fn stage_name(stage: crate::ai::events::StageKind) -> &'static str {
    use crate::ai::events::StageKind;
    match stage {
        StageKind::Scout => "scout",
        StageKind::Review => "review",
        StageKind::Challenge => "challenge",
        StageKind::Verify => "verify",
        StageKind::PriorCheck => "prior-check",
        StageKind::Assemble => "assemble",
    }
}

fn stage_state(state: &crate::ai::events::StageStatus) -> &'static str {
    use crate::ai::events::StageStatus;
    match state {
        StageStatus::Started => "started",
        StageStatus::Done => "done",
        StageStatus::Failed(_) => "failed",
    }
}

fn group_state(state: GroupState) -> &'static str {
    match state {
        GroupState::Pending => "pending",
        GroupState::Running => "running",
        GroupState::Done => "done",
        GroupState::Skipped => "skipped",
        GroupState::Failed => "failed",
    }
}

/// The chat agent's steering tools over `steer`, in presentation order.
pub fn tools(steer: Arc<SteerCtx>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PipelineStatusTool {
            steer: steer.clone(),
        }),
        Arc::new(GetFinding {
            steer: steer.clone(),
        }),
        Arc::new(GetAgentLog {
            steer: steer.clone(),
        }),
        Arc::new(AddLead {
            steer: steer.clone(),
        }),
        Arc::new(AddCheck {
            steer: steer.clone(),
        }),
        Arc::new(NoteAgent {
            steer: steer.clone(),
        }),
        Arc::new(RerunGroup {
            steer: steer.clone(),
        }),
        Arc::new(SkipGroup {
            steer: steer.clone(),
        }),
        Arc::new(PauseRun {
            steer: steer.clone(),
        }),
        Arc::new(ResumeRun {
            steer: steer.clone(),
        }),
        Arc::new(CancelRun {
            steer: steer.clone(),
        }),
        Arc::new(AnswerQuestion { steer }),
    ]
}

/// `pipeline_status {}`.
struct PipelineStatusTool {
    steer: Arc<SteerCtx>,
}

#[async_trait::async_trait]
impl Tool for PipelineStatusTool {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "pipeline_status",
            "Show the review run's stages, groups, running agents, finding \
             counts and open question ids. The same digest is in your \
             message; call this when it may have changed.",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolOutput {
        ToolOutput::text(self.steer.digest())
    }
}

/// `get_finding { id }`.
struct GetFinding {
    steer: Arc<SteerCtx>,
}

#[derive(Deserialize)]
struct FindingInput {
    id: i64,
}

#[async_trait::async_trait]
impl Tool for GetFinding {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "get_finding",
            "Read one recorded finding in full: severity, state, anchor, \
             body and evidence.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "integer", "description": "The finding id." } },
                "required": ["id"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: FindingInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let findings = match self.steer.store.finding_list(self.steer.session_id) {
            Ok(findings) => findings,
            Err(error) => {
                return ToolOutput::error(format!("could not read the findings: {error}"));
            }
        };
        match findings.iter().find(|finding| finding.id == input.id) {
            Some(finding) => ToolOutput::text(render_finding(finding)),
            None => ToolOutput::error(format!("no finding #{} in this review", input.id)),
        }
    }
}

/// One finding rendered for the chat model.
fn render_finding(finding: &Finding) -> String {
    let mut out = format!(
        "finding #{} [{} {}] {}",
        finding.id,
        finding.severity.as_str(),
        finding.state.as_str(),
        finding.anchor.path()
    );
    if let crate::ai::review::FindingAnchor::Line(anchor) = &finding.anchor {
        out.push_str(&format!(":{}", anchor.line));
    }
    out.push('\n');
    out.push_str(&format!("title: {}\n", finding.title));
    out.push_str(&format!(
        "body: {}\n",
        finding
            .body
            .chars()
            .take(MAX_FINDING_BODY)
            .collect::<String>()
    ));
    for evidence in &finding.evidence {
        let line = evidence
            .line
            .map(|line| format!(":{line}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "evidence: {}{line} — {}\n",
            evidence.path, evidence.note
        ));
    }
    out
}

/// `get_agent_log { agent_id, last_n? }`.
struct GetAgentLog {
    steer: Arc<SteerCtx>,
}

#[derive(Deserialize)]
struct LogInput {
    agent_id: String,
    #[serde(default)]
    last_n: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for GetAgentLog {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "get_agent_log",
            "Read the tail of one agent's transcript. `agent_id` is the \
             agent's label (`reviewer 1`) or the numeric id shown in the \
             digest. Defaults to the last 20 messages.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Agent label or numeric id." },
                    "last_n": { "type": "integer", "description": "How many messages to show; default 20." },
                },
                "required": ["agent_id"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: LogInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let label = match self.steer.resolve_label(&input.agent_id) {
            Ok(label) => label,
            Err(out) => return out,
        };
        let rows = match self.steer.store.messages(self.steer.run_id, &label) {
            Ok(rows) => rows,
            Err(error) => {
                return ToolOutput::error(format!("could not read the transcript: {error}"));
            }
        };
        let last_n = input.last_n.unwrap_or(DEFAULT_LOG_MESSAGES).max(1);
        ToolOutput::text(format!(
            "transcript of {label} (last {} of {} messages):\n{}",
            rows.len().min(last_n),
            rows.len(),
            render_log(&rows, last_n)
        ))
    }
}

impl SteerCtx {
    /// The transcript label behind a `get_agent_log` `agent_id`: a label as
    /// given, a numeric id of a running agent, or an error naming the labels
    /// that do have a transcript.
    fn resolve_label(&self, raw: &str) -> Result<String, ToolOutput> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ToolOutput::error("agent_id is required"));
        }
        if self
            .store
            .messages(self.run_id, raw)
            .is_ok_and(|rows| !rows.is_empty())
        {
            return Ok(raw.to_string());
        }
        if let Some(control) = &self.control
            && let Ok(id) = raw.parse::<u32>()
            && let Some(agent) = control
                .status()
                .running_agents
                .iter()
                .find(|agent| agent.id == id)
        {
            return Ok(agent.label.clone());
        }
        let labels: BTreeSet<String> = self
            .store
            .run_timeline(self.run_id)
            .unwrap_or_default()
            .into_iter()
            .map(|(label, ..)| label)
            .collect();
        let labels: Vec<String> = labels.into_iter().collect();
        Err(ToolOutput::error(format!(
            "no transcript for agent {raw:?}; agents with one: {}",
            if labels.is_empty() {
                "none yet".to_string()
            } else {
                labels.join(", ")
            }
        )))
    }
}

/// The last `last_n` transcript rows, one per line, tool blocks summarised.
fn render_log(rows: &[(String, String)], last_n: usize) -> String {
    let skip = rows.len().saturating_sub(last_n);
    let mut out = String::new();
    for (role, content_json) in &rows[skip..] {
        out.push_str(&format!("{role}: {}\n", render_content(content_json)));
    }
    out
}

/// One stored `content_json` rendered as text: text blocks verbatim (cut),
/// tool calls and results as one-liners.
fn render_content(content_json: &str) -> String {
    let Ok(blocks) = serde_json::from_str::<Vec<ContentBlock>>(content_json) else {
        return content_json.chars().take(MAX_LOG_TEXT).collect();
    };
    let rendered: Vec<String> = blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().take(MAX_LOG_TEXT).collect(),
            ContentBlock::ToolUse { name, input, .. } => {
                format!(
                    "[called {name} {}]",
                    input.to_string().chars().take(120).collect::<String>()
                )
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let text = content
                    .iter()
                    .find_map(|part| match part {
                        review_llm::ToolResultPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or_default();
                format!(
                    "[{}: {}]",
                    if *is_error { "error" } else { "result" },
                    text.chars().take(MAX_LOG_TEXT).collect::<String>()
                )
            }
            _ => "[non-text block]".to_string(),
        })
        .collect();
    rendered.join(" ")
}

/// `add_lead { group, text }`.
struct AddLead {
    steer: Arc<SteerCtx>,
}

#[derive(Deserialize)]
struct GroupText {
    group: String,
    text: String,
}

#[async_trait::async_trait]
impl Tool for AddLead {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "add_lead",
            "Add a lead to a group whose reviewer has not started yet; it \
             lands in that reviewer's task message.",
            json!({
                "type": "object",
                "properties": {
                    "group": { "type": "string" },
                    "text": { "type": "string" },
                },
                "required": ["group", "text"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: GroupText = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let control = match self.steer.live_control() {
            Ok(control) => control,
            Err(out) => return out,
        };
        match control.add_lead(&input.group, &input.text) {
            Ok(()) => ToolOutput::text(format!("added a lead to {}", input.group)),
            Err(error) => ToolOutput::error(error),
        }
    }
}

/// `add_check { group, item }`.
struct AddCheck {
    steer: Arc<SteerCtx>,
}

#[derive(Deserialize)]
struct GroupItem {
    group: String,
    item: String,
}

#[async_trait::async_trait]
impl Tool for AddCheck {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "add_check",
            "Add an extra checklist item to a group whose reviewer has not \
             started yet.",
            json!({
                "type": "object",
                "properties": {
                    "group": { "type": "string" },
                    "item": { "type": "string" },
                },
                "required": ["group", "item"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: GroupItem = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let control = match self.steer.live_control() {
            Ok(control) => control,
            Err(out) => return out,
        };
        match control.add_check(&input.group, &input.item) {
            Ok(()) => ToolOutput::text(format!("added the check to {}", input.group)),
            Err(error) => ToolOutput::error(error),
        }
    }
}

/// `note_agent { agent_id, text }`.
struct NoteAgent {
    steer: Arc<SteerCtx>,
}

#[derive(Deserialize)]
struct NoteInput {
    agent_id: u32,
    text: String,
}

#[async_trait::async_trait]
impl Tool for NoteAgent {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "note_agent",
            "Deliver a note to one running agent before its next model call. \
             `agent_id` is the numeric id from the digest; unknown ids fall \
             back to every agent.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "integer" },
                    "text": { "type": "string" },
                },
                "required": ["agent_id", "text"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: NoteInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let control = match self.steer.live_control() {
            Ok(control) => control,
            Err(out) => return out,
        };
        control.note_agent(input.agent_id, &input.text);
        ToolOutput::text(format!(
            "noted agent {} — it reads the note before its next model call",
            input.agent_id
        ))
    }
}

/// `rerun_group { group }` and `skip_group { group }`.
struct RerunGroup {
    steer: Arc<SteerCtx>,
}

struct SkipGroup {
    steer: Arc<SteerCtx>,
}

#[derive(Deserialize)]
struct GroupRef {
    group: String,
}

#[async_trait::async_trait]
impl Tool for RerunGroup {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "rerun_group",
            "Cancel a running group's reviewer and start it again, or let a \
             pending group run unchanged.",
            json!({
                "type": "object",
                "properties": { "group": { "type": "string" } },
                "required": ["group"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: GroupRef = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let control = match self.steer.live_control() {
            Ok(control) => control,
            Err(out) => return out,
        };
        match control.rerun_group(&input.group) {
            Ok(()) => ToolOutput::text(format!("rerun requested for {}", input.group)),
            Err(error) => ToolOutput::error(error),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SkipGroup {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "skip_group",
            "Skip a group: a pending one never starts, a running one is \
             cancelled and marked skipped.",
            json!({
                "type": "object",
                "properties": { "group": { "type": "string" } },
                "required": ["group"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: GroupRef = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let control = match self.steer.live_control() {
            Ok(control) => control,
            Err(out) => return out,
        };
        match control.skip_group(&input.group) {
            Ok(()) => ToolOutput::text(format!("skipped {}", input.group)),
            Err(error) => ToolOutput::error(error),
        }
    }
}

/// `pause_run {}`, `resume_run {}` and `cancel_run {}`.
struct PauseRun {
    steer: Arc<SteerCtx>,
}

struct ResumeRun {
    steer: Arc<SteerCtx>,
}

struct CancelRun {
    steer: Arc<SteerCtx>,
}

#[async_trait::async_trait]
impl Tool for PauseRun {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "pause_run",
            "Hold every running agent before its next model call. Running \
             tool calls finish first.",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolOutput {
        match self.steer.live_control() {
            Ok(control) => {
                control.pause();
                ToolOutput::text("the review is paused")
            }
            Err(out) => out,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ResumeRun {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "resume_run",
            "Release a pause: the agents continue from where they stopped.",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolOutput {
        match self.steer.live_control() {
            Ok(control) => {
                control.resume();
                ToolOutput::text("the review continues")
            }
            Err(out) => out,
        }
    }
}

#[async_trait::async_trait]
impl Tool for CancelRun {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "cancel_run",
            "Stop the whole review run. What is already recorded stays. Use \
             only when the user asks to stop.",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolOutput {
        match self.steer.live_control() {
            Ok(control) => {
                control.cancel();
                ToolOutput::text("the review is cancelled")
            }
            Err(out) => out,
        }
    }
}

/// `answer_question { question_id, answer }`.
struct AnswerQuestion {
    steer: Arc<SteerCtx>,
}

#[derive(Deserialize)]
struct AnswerInput {
    question_id: u64,
    answer: String,
}

#[async_trait::async_trait]
impl Tool for AnswerQuestion {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "answer_question",
            "Route the user's free-text answer to the agent parked on \
             `ask_user`, by the question id from the digest. An id that is \
             no longer waiting is ignored.",
            json!({
                "type": "object",
                "properties": {
                    "question_id": { "type": "integer" },
                    "answer": { "type": "string" },
                },
                "required": ["question_id", "answer"],
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let input: AnswerInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        if self.steer.live_control().is_err() {
            return ToolOutput::error(FINISHED);
        }
        if self
            .steer
            .interaction
            .answer(input.question_id, input.answer)
        {
            ToolOutput::text(format!("answered question #{}", input.question_id))
        } else {
            ToolOutput::text(format!(
                "no question #{} is waiting for an answer",
                input.question_id
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::{FINISHED, SteerCtx, status_digest, tools};
    use crate::ai::events::{StageKind, StageStatus};
    use crate::ai::interaction::{Interaction, Question};
    use crate::ai::pipeline::{GroupState, PipelineStatus};
    use crate::ai::review::FindingState;
    use crate::ai::review::Severity;
    use crate::ai::store::Store;
    use crate::ai::ui_interaction::UiInteraction;
    use crate::runtime::{SessionId, event_channel};
    use std::sync::{Arc, Mutex};

    fn interaction(events: &crate::runtime::EventSender) -> Arc<UiInteraction> {
        UiInteraction::new(
            events.clone(),
            Arc::new(Mutex::new(SessionId(1))),
            Vec::new(),
        )
    }

    #[test]
    fn digest_lists_stages_groups_running_agents_findings_and_questions() {
        let status = PipelineStatus {
            stages: vec![
                (StageKind::Scout, StageStatus::Done),
                (StageKind::Review, StageStatus::Started),
            ],
            groups: vec![
                ("g1".to_string(), GroupState::Done),
                ("g2".to_string(), GroupState::Running),
            ],
            running_agents: vec![crate::ai::tools::AgentRef {
                id: 4,
                role: crate::ai::tools::AgentRole::Reviewer,
                label: "reviewer 1".to_string(),
            }],
            findings: vec![(Severity::Bug, FindingState::Draft, 2)],
            open_questions: 1,
        };
        let digest = status_digest(
            Some(&status),
            &[(12, "is an empty list allowed?".to_string())],
            Some("adds a helper"),
            true,
            false,
        );
        assert!(digest.contains("state: running"), "{digest}");
        assert!(!digest.contains("paused"), "{digest}");
        assert!(digest.contains("intent: adds a helper"), "{digest}");
        assert!(
            digest.contains("stages: scout done, review started"),
            "{digest}"
        );
        assert!(digest.contains("groups: g1 done, g2 running"), "{digest}");
        assert!(digest.contains("running: reviewer 1 (id 4)"), "{digest}");
        assert!(digest.contains("findings: bug draft 2"), "{digest}");
        assert!(
            digest.contains("open questions: #12 \"is an empty list allowed?\""),
            "{digest}"
        );
    }

    #[test]
    fn digest_without_a_run_says_finished_and_no_pipeline() {
        let digest = status_digest(None, &[], None, false, false);
        assert!(digest.contains("state: finished"), "{digest}");
        assert!(digest.contains("no pipeline is attached"), "{digest}");
        assert!(digest.contains("open questions: none"), "{digest}");
    }

    #[test]
    fn digest_marks_a_paused_run() {
        let empty = PipelineStatus {
            stages: Vec::new(),
            groups: Vec::new(),
            running_agents: Vec::new(),
            findings: Vec::new(),
            open_questions: 0,
        };
        let digest = status_digest(Some(&empty), &[], None, true, true);
        assert!(digest.contains("state: running, paused"), "{digest}");
    }

    #[tokio::test]
    async fn steering_tools_answer_finished_after_the_run() {
        let (tx, _rx) = event_channel();
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let steer = Arc::new(SteerCtx::new(
            None,
            Arc::new(AtomicBool::new(false)),
            interaction(&tx),
            store,
            1,
            1,
        ));
        let dir = crate::test_support::tempdir();
        let cx = cx_in(&dir);
        let registry = tools(steer);
        for name in [
            "add_lead",
            "add_check",
            "note_agent",
            "rerun_group",
            "skip_group",
            "pause_run",
            "resume_run",
            "cancel_run",
            "answer_question",
        ] {
            let tool = registry
                .iter()
                .find(|tool| tool.definition().name == name)
                .expect("tool exists");
            let input = match name {
                "add_lead" => serde_json::json!({"group": "g1", "text": "x"}),
                "add_check" => serde_json::json!({"group": "g1", "item": "x"}),
                "note_agent" => serde_json::json!({"agent_id": 4, "text": "x"}),
                "rerun_group" | "skip_group" => serde_json::json!({"group": "g1"}),
                "answer_question" => serde_json::json!({"question_id": 1, "answer": "yes"}),
                _ => serde_json::json!({}),
            };
            let out = tool.call(input, &cx).await;
            assert!(out.is_error, "{name} must refuse after the run");
            assert_eq!(out.parts.len(), 1);
            assert!(
                format!("{:?}", out.parts).contains(FINISHED),
                "{name}: {:?}",
                out.parts
            );
        }
    }

    /// A minimal [`ToolCx`] for the tools above: they answer before they
    /// look at it when the run is over, but the signature still needs one.
    fn cx_in(dir: &tempfile::TempDir) -> crate::ai::tools::ToolCx {
        let host = review_sandbox::LocalHost::new(dir.path()).expect("local host");
        crate::ai::tools::ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: crate::ai::tools::AgentRef {
                id: 0,
                role: crate::ai::tools::AgentRole::Chat,
                label: "chat".to_string(),
            },
            interaction: Arc::new(crate::ai::interaction::NoInteraction),
            allowed: Arc::new(Mutex::new(Vec::new())),
            for_review: Arc::new(Mutex::new(Vec::new())),
            snapshot: None,
            context: None,
            run: crate::test_support::ai_run_state(),
            assigned: Arc::new(Vec::new()),
            spawner: None,
            prepare: None,
        }
    }

    #[tokio::test]
    async fn open_questions_lists_the_parked_ask_in_id_order() {
        let (tx, _rx) = event_channel();
        let interaction = interaction(&tx);
        let parked = {
            let interaction = interaction.clone();
            tokio::spawn(async move {
                interaction
                    .ask_user(
                        &crate::ai::tools::AgentRef {
                            id: 4,
                            role: crate::ai::tools::AgentRole::Reviewer,
                            label: "reviewer 1".to_string(),
                        },
                        Question {
                            id: 9,
                            text: "which style?".to_string(),
                            options: Vec::new(),
                        },
                    )
                    .await
            })
        };
        for _ in 0..100 {
            if !interaction.open_questions().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // The digest reads the parked question: id and text, so the chat
        // model can call `answer_question` with a real id.
        assert_eq!(
            interaction.open_questions(),
            vec![(9, "which style?".to_string())]
        );
        // Answering resolves the parked `ask_user`; answering again is a
        // no-op, never a panic (the tool's "no question is waiting" path).
        assert!(interaction.answer(9, "the old one".to_string()));
        assert_eq!(parked.await.expect("the task runs"), "the old one");
        assert!(!interaction.answer(9, "too late".to_string()));
        assert!(interaction.open_questions().is_empty());
    }
}
