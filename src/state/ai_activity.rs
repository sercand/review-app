//! Read-models the ✦ AI tab derives from [`AiReviewState`]: the stage
//! stepper, one row per agent with what it is doing now, the open asks, and
//! the command log of every decided `bash` call. Pure folds over the
//! reducer's state, no egui, so every rule here is unit-tested.

use crate::ai::events::{StageKind, StageStatus};
use crate::ai::tools::{AgentRef, AgentRole};
use crate::state::ai_review::{
    AiReviewState, ClassifierNote, CommandDecision, CommandRef, TimelineEntry, ToolLine,
    agent_stage, stage_order,
};

/// The display name of an agent role.
pub fn role_name(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Scout => "Scout",
        AgentRole::Reviewer => "Reviewer",
        AgentRole::Challenger => "Challenger",
        AgentRole::Lookup => "Lookup",
        AgentRole::Verifier => "Verifier",
        AgentRole::PriorCheck => "Prior check",
        AgentRole::Assembler => "Assembler",
        AgentRole::Chat => "Chat",
        AgentRole::App => "App",
    }
}

/// The role's transcript label prefix (see the reducer's
/// `agent_ref_from_label`), so a rebuilt entry whose label is just the role
/// does not title as `"Scout · scout"`.
fn role_keyword(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Scout => "scout",
        AgentRole::Reviewer => "reviewer",
        AgentRole::Challenger => "challenger",
        AgentRole::Lookup => "lookup",
        AgentRole::Verifier => "verifier",
        AgentRole::PriorCheck => "prior_check",
        AgentRole::Assembler => "assembler",
        AgentRole::Chat => "chat",
        AgentRole::App => "app",
    }
}

/// An agent's title, e.g. `"Reviewer · notifications"`. Transcript-rebuilt
/// entries carry the group only inside the label (`reviewer:g2`); a bare
/// `scout` adds nothing.
pub fn agent_title(agent: &AgentRef, group: Option<&str>) -> String {
    match group {
        Some(group) => format!("{} · {group}", role_name(agent.role)),
        None if agent.label.is_empty() || agent.label == role_keyword(agent.role) => {
            role_name(agent.role).to_string()
        }
        None => format!("{} · {}", role_name(agent.role), agent.label),
    }
}

/// The agent's short name for the command log: its group, else the part of
/// its label after the role (`reviewer:g2` → `g2`), else the role keyword.
pub fn agent_short(agent: &AgentRef, group: Option<&str>) -> String {
    if let Some(group) = group.filter(|group| !group.is_empty()) {
        return group.to_string();
    }
    match agent.label.split_once(':') {
        Some((_, rest)) if !rest.is_empty() => rest.to_string(),
        _ if agent.label.is_empty() => role_keyword(agent.role).to_string(),
        _ => agent.label.clone(),
    }
}

/// The command a `bash` tool summary carries. The summary is the tool's
/// JSON input cut at 120 chars, so a cut one is read up to the cut and ends
/// in "…"; anything else is shown as it is.
pub fn command_text(summary: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(summary)
        && let Some(command) = value.get("command").and_then(serde_json::Value::as_str)
    {
        return command.to_string();
    }
    let body = summary
        .strip_prefix("{\"command\":\"")
        .or_else(|| summary.strip_prefix("{\"command\": \""));
    match body {
        Some(rest) => {
            let cut = rest.ends_with('…');
            let rest = rest.trim_end_matches('…');
            let rest = rest.strip_suffix("\"}").unwrap_or(rest);
            let mut out = unescape_json(rest);
            if cut {
                out.push('…');
            }
            out
        }
        None => summary.to_string(),
    }
}

/// Undo the JSON string escapes a cut summary still carries.
fn unescape_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// One tool call as a short line: `bash` shows its command, other tools
/// their name and the string values of their input
/// (`read_file src/a.rs`).
pub fn tool_brief(line: &ToolLine) -> String {
    tool_brief_parts(&line.name, &line.summary)
}

/// [`tool_brief`] over a name and a summary (chat tools carry both loose).
pub fn tool_brief_parts(name: &str, summary: &str) -> String {
    if name == "bash" {
        return command_text(summary);
    }
    let values = serde_json::from_str::<serde_json::Value>(summary)
        .ok()
        .and_then(|value| match value {
            serde_json::Value::Object(map) => Some(
                map.values()
                    .filter_map(|value| match value {
                        serde_json::Value::String(s) => Some(s.clone()),
                        serde_json::Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        });
    match values {
        Some(values) if !values.is_empty() => format!("{name} {values}"),
        _ if summary.is_empty() => name.to_string(),
        _ => format!("{name} {summary}"),
    }
}

/// The last non-empty line of `text`, trimmed.
pub fn last_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("")
}

/// The last paragraph of `text` (blocks split by a blank line), trimmed.
pub fn last_paragraph(text: &str) -> &str {
    text.rsplit("\n\n")
        .map(str::trim)
        .find(|block| !block.is_empty())
        .unwrap_or("")
}

/// Which slice of the command log the filter chips show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFilter {
    /// Every decided command.
    #[default]
    All,
    /// The ones the user decided (now, or earlier "for this review").
    AskedYou,
    /// The ones the rules, the classifier or full trust let run.
    Auto,
    /// The ones a deny rule blocked or the mode refused.
    Refused,
}

impl LogFilter {
    /// The chips, in display order.
    pub const ALL: [LogFilter; 4] = [
        LogFilter::All,
        LogFilter::AskedYou,
        LogFilter::Auto,
        LogFilter::Refused,
    ];

    /// The chip's label.
    pub fn label(self) -> &'static str {
        match self {
            LogFilter::All => "All",
            LogFilter::AskedYou => "Asked you",
            LogFilter::Auto => "Auto",
            LogFilter::Refused => "Blocked / denied",
        }
    }

    /// Whether a command decided as `decision` shows under this chip.
    pub fn matches(self, decision: &CommandDecision) -> bool {
        self == LogFilter::All || decision_bucket(decision) == self
    }
}

/// The one non-`All` chip a decision belongs to.
pub fn decision_bucket(decision: &CommandDecision) -> LogFilter {
    match decision {
        CommandDecision::User { .. }
        | CommandDecision::UserDenied
        | CommandDecision::Remembered => LogFilter::AskedYou,
        CommandDecision::Rules { .. } | CommandDecision::Classifier | CommandDecision::Trust => {
            LogFilter::Auto
        }
        CommandDecision::Blocked { .. } | CommandDecision::AutoDenied { .. } => LogFilter::Refused,
    }
}

/// One decided `bash` call in the command log.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandRow {
    /// Where the line sits in the timeline.
    pub at: CommandRef,
    /// The agent's short name ([`agent_short`]), `chat` for the chat.
    pub agent: String,
    /// The command, read out of the tool summary.
    pub command: String,
    /// How it was decided.
    pub decision: CommandDecision,
    /// What the classifier said, when it was asked.
    pub note: Option<ClassifierNote>,
}

/// Every decided `bash` call, newest first: in the order the decisions
/// landed ([`AiReviewState::decided`]), then any decided line that order
/// does not know (a hand-built state) in reverse timeline order.
pub fn command_log(st: &AiReviewState) -> Vec<CommandRow> {
    let mut rows: Vec<(Option<usize>, CommandRow)> = Vec::new();
    for (entry, item) in st.timeline.iter().enumerate() {
        match item {
            TimelineEntry::Agent {
                agent,
                group,
                tools,
                ..
            } => {
                for (tool, line) in tools.iter().enumerate() {
                    let Some(decision) = &line.decision else {
                        continue;
                    };
                    let at = CommandRef {
                        entry,
                        tool: Some(tool),
                    };
                    rows.push((
                        st.decided.iter().position(|seen| *seen == at),
                        CommandRow {
                            at,
                            agent: agent_short(agent, group.as_deref()),
                            command: command_text(&line.summary),
                            decision: decision.clone(),
                            note: line.note,
                        },
                    ));
                }
            }
            TimelineEntry::ChatTool {
                summary,
                note,
                decision: Some(decision),
                ..
            } => {
                let at = CommandRef { entry, tool: None };
                rows.push((
                    st.decided.iter().position(|seen| *seen == at),
                    CommandRow {
                        at,
                        agent: "chat".to_string(),
                        command: command_text(summary),
                        decision: decision.clone(),
                        note: *note,
                    },
                ));
            }
            _ => {}
        }
    }
    // Known order first (newest first), then the rest newest first by
    // timeline position; `rows` is in timeline order, so a stable sort on
    // the reversed list keeps that.
    rows.reverse();
    rows.sort_by_key(|(rank, _)| match rank {
        Some(rank) => (0, usize::MAX - rank),
        None => (1, 0),
    });
    rows.into_iter().map(|(_, row)| row).collect()
}

/// How many rows each [`LogFilter::ALL`] chip shows.
pub fn filter_counts(rows: &[CommandRow]) -> [usize; 4] {
    LogFilter::ALL.map(|filter| {
        rows.iter()
            .filter(|row| filter.matches(&row.decision))
            .count()
    })
}

/// Who settled the run's commands, for the auto-approve popover's bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecisionCounts {
    /// A rule policy vouched for it.
    pub rules: usize,
    /// The local classifier approved it.
    pub classifier: usize,
    /// Full trust ran it unchecked.
    pub trust: usize,
    /// The user decided it (now, or earlier for this review).
    pub you: usize,
    /// A deny rule blocked it.
    pub blocked: usize,
    /// Nothing vouched for it and the mode refused it.
    pub denied: usize,
}

impl DecisionCounts {
    /// Every counted command.
    pub fn total(&self) -> usize {
        self.rules + self.classifier + self.trust + self.you + self.blocked + self.denied
    }
}

/// Count `rows` by who settled them.
pub fn decision_counts(rows: &[CommandRow]) -> DecisionCounts {
    let mut counts = DecisionCounts::default();
    for row in rows {
        match row.decision {
            CommandDecision::Rules { .. } => counts.rules += 1,
            CommandDecision::Classifier => counts.classifier += 1,
            CommandDecision::Trust => counts.trust += 1,
            CommandDecision::Remembered
            | CommandDecision::User { .. }
            | CommandDecision::UserDenied => counts.you += 1,
            CommandDecision::Blocked { .. } => counts.blocked += 1,
            CommandDecision::AutoDenied { .. } => counts.denied += 1,
        }
    }
    counts
}

/// The timeline indexes of the asks still waiting on the user: undecided
/// approvals and unanswered questions, oldest first.
pub fn open_asks(st: &AiReviewState) -> Vec<usize> {
    st.timeline
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            matches!(
                entry,
                TimelineEntry::Approval { decided: None, .. }
                    | TimelineEntry::Question { answer: None, .. }
            )
        })
        .map(|(index, _)| index)
        .collect()
}

/// Whether `agent` has an ask open.
fn waiting_on_user(st: &AiReviewState, agent: &AgentRef) -> bool {
    st.timeline.iter().any(|entry| match entry {
        TimelineEntry::Approval {
            agent: asker,
            decided: None,
            ..
        }
        | TimelineEntry::Question {
            agent: asker,
            answer: None,
            ..
        } => asker.label == agent.label,
        _ => false,
    })
}

/// Where one agent stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Working.
    Running,
    /// Parked on an approval or a question.
    Waiting,
    /// Its loop ended normally.
    Done,
    /// Its loop ended any other way; the payload is the stop reason.
    Stopped(String),
}

/// One agent's row in the Agents list.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    /// The agent's timeline index (the row's identity for expansion).
    pub entry: usize,
    /// Its title ([`agent_title`]).
    pub title: String,
    /// Where it stands.
    pub status: AgentStatus,
    /// One line on what it is doing now, or what it said last.
    pub activity: String,
    /// How many tools it called.
    pub calls: usize,
}

/// One row per agent entry, in timeline order. The chat agent's calls are
/// [`TimelineEntry::ChatTool`] entries, never an agent row.
pub fn agent_rows(st: &AiReviewState) -> Vec<AgentRow> {
    st.timeline
        .iter()
        .enumerate()
        .filter_map(|(entry, item)| {
            let TimelineEntry::Agent {
                agent,
                group,
                tools,
                text,
                stop,
            } = item
            else {
                return None;
            };
            let status = match stop.as_deref() {
                Some("finished") => AgentStatus::Done,
                Some(other) => AgentStatus::Stopped(other.to_string()),
                None if waiting_on_user(st, agent) => AgentStatus::Waiting,
                None => AgentStatus::Running,
            };
            Some(AgentRow {
                entry,
                title: agent_title(agent, group.as_deref()),
                activity: agent_activity(&status, tools, text),
                status,
                calls: tools.len(),
            })
        })
        .collect()
}

/// The row's one-line activity: a waiting agent waits for the user, a
/// running one shows its last tool call (or its last line of text), a
/// finished one its last line of text.
pub fn agent_activity(status: &AgentStatus, tools: &[ToolLine], text: &str) -> String {
    let said = last_line(text);
    match status {
        AgentStatus::Waiting => "waiting for you".to_string(),
        AgentStatus::Running => match tools.last() {
            Some(line) => tool_brief(line),
            None if !said.is_empty() => said.to_string(),
            None => "starting…".to_string(),
        },
        AgentStatus::Done if !said.is_empty() => said.to_string(),
        AgentStatus::Done => "finished".to_string(),
        AgentStatus::Stopped(why) if !said.is_empty() => format!("{why} — {said}"),
        AgentStatus::Stopped(why) => why.clone(),
    }
}

/// Where one stage of the stepper stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepState {
    /// Not announced yet.
    Pending,
    /// Started, not done.
    Active,
    /// Done.
    Done,
    /// Failed; the payload says why.
    Failed(String),
    /// Never announced while a later stage was: this run skips it.
    Skipped,
}

/// One stage of the stepper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The stage.
    pub stage: StageKind,
    /// Where it stands.
    pub state: StepState,
    /// Agents counted against it: `(finished, started)`.
    pub agents: (u32, u32),
    /// How many agents the stage said it would run; 0 when unknown.
    pub planned: u32,
}

/// The stepper: Scout, Review, Challenge, Verify, Assemble (and the prior
/// check, first, when the run announced one), with each stage's latest
/// status and agent progress.
pub fn stepper(st: &AiReviewState) -> Vec<Step> {
    let mut latest: Vec<(StageKind, StageStatus)> = Vec::new();
    for entry in &st.timeline {
        if let TimelineEntry::Stage { stage, status } = entry {
            match latest.iter_mut().find(|(seen, _)| seen == stage) {
                Some((_, last)) => *last = status.clone(),
                None => latest.push((*stage, status.clone())),
            }
        }
    }
    let mut stages = vec![
        StageKind::Scout,
        StageKind::Review,
        StageKind::Challenge,
        StageKind::Verify,
        StageKind::Assemble,
    ];
    if latest
        .iter()
        .any(|(stage, _)| *stage == StageKind::PriorCheck)
    {
        stages.insert(0, StageKind::PriorCheck);
    }
    let announced_after = |stage: StageKind| {
        latest
            .iter()
            .any(|(seen, _)| stage_order(*seen) > stage_order(stage))
    };
    stages
        .into_iter()
        .map(|stage| {
            let state = match latest.iter().find(|(seen, _)| *seen == stage) {
                Some((_, StageStatus::Started)) => StepState::Active,
                Some((_, StageStatus::Done)) => StepState::Done,
                Some((_, StageStatus::Failed(why))) => StepState::Failed(why.clone()),
                None if announced_after(stage) => StepState::Skipped,
                None => StepState::Pending,
            };
            Step {
                stage,
                state,
                agents: stage_agents(st, stage),
                planned: st
                    .planned
                    .iter()
                    .find(|(seen, _)| *seen == stage)
                    .map_or(0, |(_, n)| *n),
            }
        })
        .collect()
}

/// `(finished, started)` agents counted against `stage`.
fn stage_agents(st: &AiReviewState, stage: StageKind) -> (u32, u32) {
    let mut started = 0;
    let mut finished = 0;
    for entry in &st.timeline {
        if let TimelineEntry::Agent { agent, stop, .. } = entry
            && agent_stage(agent.role) == Some(stage)
        {
            started += 1;
            if stop.is_some() {
                finished += 1;
            }
        }
    }
    (finished, started)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ui_interaction::ApprovalKind;

    fn agent(role: AgentRole, label: &str) -> AgentRef {
        AgentRef {
            id: 1,
            role,
            label: label.to_string(),
        }
    }

    fn bash(command: &str, decision: Option<CommandDecision>) -> ToolLine {
        ToolLine {
            name: "bash".into(),
            summary: serde_json::json!({ "command": command }).to_string(),
            note: None,
            decision,
        }
    }

    fn agent_entry(label: &str, group: Option<&str>, tools: Vec<ToolLine>) -> TimelineEntry {
        TimelineEntry::Agent {
            agent: agent(AgentRole::Reviewer, label),
            group: group.map(str::to_string),
            tools,
            text: String::new(),
            stop: None,
        }
    }

    #[test]
    fn command_text_reads_whole_and_cut_summaries() {
        assert_eq!(command_text("{\"command\":\"ls -la\"}"), "ls -la");
        assert_eq!(
            command_text(
                "{\"command\":\"grep -n \\\"fn x\\\" src/a.rs | head -60 && sed -n 1,2p b…"
            ),
            "grep -n \"fn x\" src/a.rs | head -60 && sed -n 1,2p b…",
            "a cut summary is read up to the cut"
        );
        assert_eq!(command_text("not json"), "not json");
    }

    #[test]
    fn tool_brief_names_non_bash_tools_by_their_input() {
        let line = ToolLine {
            name: "read_file".into(),
            summary: "{\"path\":\"src/a.rs\"}".into(),
            ..ToolLine::default()
        };
        assert_eq!(tool_brief(&line), "read_file src/a.rs");
        let line = ToolLine {
            name: "read_file".into(),
            summary: "src/a.rs".into(),
            ..ToolLine::default()
        };
        assert_eq!(tool_brief(&line), "read_file src/a.rs");
        assert_eq!(tool_brief(&bash("cargo test", None)), "cargo test");
        assert_eq!(tool_brief_parts("list_files", ""), "list_files");
    }

    #[test]
    fn last_line_and_paragraph() {
        assert_eq!(last_line("one\ntwo\n\n  "), "two");
        assert_eq!(last_line(""), "");
        assert_eq!(last_paragraph("a\nb\n\nc\nd\n\n"), "c\nd");
    }

    #[test]
    fn agent_titles_and_short_names() {
        let scout = agent(AgentRole::Scout, "scout");
        assert_eq!(agent_title(&scout, None), "Scout");
        assert_eq!(agent_short(&scout, None), "scout");
        let rebuilt = agent(AgentRole::Reviewer, "reviewer:g2");
        assert_eq!(agent_title(&rebuilt, None), "Reviewer · reviewer:g2");
        assert_eq!(agent_short(&rebuilt, None), "g2");
        assert_eq!(agent_title(&rebuilt, Some("notif")), "Reviewer · notif");
        assert_eq!(agent_short(&rebuilt, Some("notif")), "notif");
    }

    #[test]
    fn the_log_lists_decided_bash_calls_newest_first_and_filters() {
        let mut st = AiReviewState {
            timeline: vec![
                agent_entry(
                    "reviewer:g1",
                    Some("g1"),
                    vec![
                        bash(
                            "ls",
                            Some(CommandDecision::Rules {
                                source: "read-only",
                            }),
                        ),
                        bash("pending", None),
                        ToolLine {
                            name: "read_file".into(),
                            summary: "src/a.rs".into(),
                            ..ToolLine::default()
                        },
                        bash(
                            "cat .env",
                            Some(CommandDecision::Blocked {
                                reason: "secret path".into(),
                            }),
                        ),
                    ],
                ),
                agent_entry(
                    "reviewer:g2",
                    Some("g2"),
                    vec![bash(
                        "sed -n 1,2p a",
                        Some(CommandDecision::User { for_review: false }),
                    )],
                ),
                TimelineEntry::ChatTool {
                    name: "bash".into(),
                    summary: "{\"command\":\"wc -l a\"}".into(),
                    note: Some(ClassifierNote::Approved),
                    decision: Some(CommandDecision::Classifier),
                },
            ],
            ..AiReviewState::default()
        };
        // No recorded order: reverse timeline order.
        let rows = command_log(&st);
        let commands: Vec<&str> = rows.iter().map(|row| row.command.as_str()).collect();
        assert_eq!(commands, ["wc -l a", "sed -n 1,2p a", "cat .env", "ls"]);
        assert_eq!(rows[0].agent, "chat");
        assert_eq!(rows[1].agent, "g2");
        assert_eq!(rows[0].note, Some(ClassifierNote::Approved));

        // A recorded order wins: the decisions landed ls, chat, sed, cat.
        st.decided = vec![
            CommandRef {
                entry: 0,
                tool: Some(0),
            },
            CommandRef {
                entry: 2,
                tool: None,
            },
            CommandRef {
                entry: 1,
                tool: Some(0),
            },
            CommandRef {
                entry: 0,
                tool: Some(3),
            },
        ];
        let rows = command_log(&st);
        let commands: Vec<&str> = rows.iter().map(|row| row.command.as_str()).collect();
        assert_eq!(commands, ["cat .env", "sed -n 1,2p a", "wc -l a", "ls"]);

        assert_eq!(filter_counts(&rows), [4, 1, 2, 1]);
        let refused: Vec<&str> = rows
            .iter()
            .filter(|row| LogFilter::Refused.matches(&row.decision))
            .map(|row| row.command.as_str())
            .collect();
        assert_eq!(refused, ["cat .env"]);
        assert_eq!(
            decision_counts(&rows),
            DecisionCounts {
                rules: 1,
                classifier: 1,
                you: 1,
                blocked: 1,
                ..DecisionCounts::default()
            }
        );
        assert_eq!(decision_counts(&rows).total(), 4);
    }

    #[test]
    fn decision_buckets() {
        use CommandDecision as D;
        for (decision, bucket) in [
            (
                D::Rules {
                    source: "read-only",
                },
                LogFilter::Auto,
            ),
            (D::Classifier, LogFilter::Auto),
            (D::Trust, LogFilter::Auto),
            (D::Remembered, LogFilter::AskedYou),
            (D::User { for_review: true }, LogFilter::AskedYou),
            (D::UserDenied, LogFilter::AskedYou),
            (D::Blocked { reason: "x".into() }, LogFilter::Refused),
            (D::AutoDenied { reason: "x".into() }, LogFilter::Refused),
        ] {
            assert_eq!(decision_bucket(&decision), bucket, "{decision:?}");
            assert!(LogFilter::All.matches(&decision));
        }
    }

    #[test]
    fn agent_rows_derive_status_and_activity() {
        let waiting = agent(AgentRole::Reviewer, "reviewer:g2");
        let st = AiReviewState {
            timeline: vec![
                TimelineEntry::Agent {
                    agent: agent(AgentRole::Scout, "scout"),
                    group: None,
                    tools: vec![bash("ls", None)],
                    text: "Mapping.\n\nGrouped into 2 areas.".into(),
                    stop: Some("finished".into()),
                },
                agent_entry(
                    "reviewer:g1",
                    Some("g1"),
                    vec![ToolLine {
                        name: "read_file".into(),
                        summary: "{\"path\":\"src/a.rs\"}".into(),
                        ..ToolLine::default()
                    }],
                ),
                TimelineEntry::Agent {
                    agent: waiting.clone(),
                    group: Some("g2".into()),
                    tools: Vec::new(),
                    text: String::new(),
                    stop: None,
                },
                TimelineEntry::Agent {
                    agent: agent(AgentRole::Verifier, "verifier:a.rs"),
                    group: None,
                    tools: Vec::new(),
                    text: String::new(),
                    stop: Some("max_turns".into()),
                },
                TimelineEntry::Approval {
                    id: 3,
                    agent: waiting,
                    kind: ApprovalKind::Command {
                        command: "cargo test".into(),
                        reason: "runs code".into(),
                    },
                    decided: None,
                },
            ],
            ..AiReviewState::default()
        };
        let rows = agent_rows(&st);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].status, AgentStatus::Done);
        assert_eq!(rows[0].activity, "Grouped into 2 areas.");
        assert_eq!(rows[0].calls, 1);
        assert_eq!(rows[1].status, AgentStatus::Running);
        assert_eq!(rows[1].activity, "read_file src/a.rs");
        assert_eq!(rows[1].title, "Reviewer · g1");
        assert_eq!(rows[2].status, AgentStatus::Waiting);
        assert_eq!(rows[2].activity, "waiting for you");
        assert_eq!(rows[3].status, AgentStatus::Stopped("max_turns".into()));
        assert_eq!(rows[3].activity, "max_turns");
        assert_eq!(open_asks(&st), vec![4]);
        assert_eq!(agent_activity(&AgentStatus::Running, &[], ""), "starting…");
    }

    #[test]
    fn the_stepper_marks_progress_and_skips() {
        let st = AiReviewState {
            timeline: vec![
                TimelineEntry::Stage {
                    stage: StageKind::Scout,
                    status: StageStatus::Started,
                },
                TimelineEntry::Stage {
                    stage: StageKind::Scout,
                    status: StageStatus::Done,
                },
                TimelineEntry::Stage {
                    stage: StageKind::Review,
                    status: StageStatus::Started,
                },
                agent_entry("reviewer:g1", Some("g1"), Vec::new()),
                TimelineEntry::Agent {
                    agent: agent(AgentRole::Reviewer, "reviewer:g2"),
                    group: Some("g2".into()),
                    tools: Vec::new(),
                    text: String::new(),
                    stop: Some("finished".into()),
                },
            ],
            ..AiReviewState::default()
        };
        let steps = stepper(&st);
        let states: Vec<(StageKind, StepState)> =
            steps.iter().map(|s| (s.stage, s.state.clone())).collect();
        assert_eq!(
            states,
            [
                (StageKind::Scout, StepState::Done),
                (StageKind::Review, StepState::Active),
                (StageKind::Challenge, StepState::Pending),
                (StageKind::Verify, StepState::Pending),
                (StageKind::Assemble, StepState::Pending),
            ]
        );
        assert_eq!(steps[1].agents, (1, 2));

        // Verify started without a challenge stage: Challenge was skipped.
        let mut st = st;
        st.timeline.push(TimelineEntry::Stage {
            stage: StageKind::Verify,
            status: StageStatus::Started,
        });
        st.timeline.push(TimelineEntry::Stage {
            stage: StageKind::PriorCheck,
            status: StageStatus::Done,
        });
        let steps = stepper(&st);
        assert_eq!(steps[0].stage, StageKind::PriorCheck);
        assert_eq!(steps[3].stage, StageKind::Challenge);
        assert_eq!(steps[3].state, StepState::Skipped);
    }
}
