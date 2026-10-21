//! The ✦ AI tab (spec §12): one session-like timeline for this PR's AI run.
//!
//! The tab is a pure view over [`AiReviewState`] (Task 2): the stage strip,
//! the timeline (stage rows, collapsible agent entries with their tool
//! calls, question cards, approval cards, findings, chat, notices), the
//! findings summary, the token usage, the chat composer, and the run
//! controls. Like every widget here it never mutates state: every control
//! returns an [`AiPanelAction`] for `app.rs` to turn into a worker
//! [`crate::runtime::Command`]. Unix-only with [`crate::ai`].

use eframe::egui::{self, Color32, Frame, Margin, RichText};

use crate::ai::events::{StageKind, StageStatus};
use crate::ai::interaction::ApprovalDecision;
use crate::ai::tools::AgentRole;
use crate::ai::ui_interaction::ApprovalKind;
use crate::state::ai_review::{AiPhase, AiReviewState, TimelineEntry};
use crate::ui::widgets::markdown;
use crate::ui::{fonts, icons, theme};

/// What a control in the tab asked of the app (spec §12).
#[derive(Clone, Debug, PartialEq)]
pub enum AiPanelAction {
    /// Start a review: plan the checkout first (spec §4 step 2).
    Start,
    /// Incremental re-review from the last completed head (spec §5.8).
    Continue,
    /// Answer a parked question (spec §10.4).
    Answer {
        /// The [`crate::runtime::Event::AiQuestion`] id.
        id: u64,
        /// The answer text.
        answer: String,
    },
    /// Decide a parked command approval (spec §8.1).
    Approve {
        /// The [`crate::runtime::Event::AiApprovalRequest`] id.
        id: u64,
        /// The decision.
        decision: ApprovalDecision,
    },
    /// Decide a parked prepare approval (spec §8.5).
    ApprovePrepare {
        /// The [`crate::runtime::Event::AiApprovalRequest`] id.
        id: u64,
        /// Whether the prepare commands may run.
        ok: bool,
    },
    /// A chat message for the live run (spec §10.5).
    SendChat(String),
    /// Park every running agent (spec §12).
    Pause,
    /// Release a pause.
    Resume,
    /// Stop the run, keeping the checkout (spec §7.3).
    Cancel,
    /// End the run and restore the checkout (spec §10.6).
    Finish,
    /// Scroll the diff to a finding (Task 5 wires the scrolling).
    JumpToFinding(i64),
    /// Toggle the dropped-by-verifier findings (spec §12).
    ShowDropped(bool),
    /// User clicked "Add to settings" on the suggested-commands card
    /// (task 4): copy the brief's commands into the repo's AI settings.
    AddSuggestedCommands,
}

/// The display name of a pipeline stage.
fn stage_name(stage: StageKind) -> &'static str {
    match stage {
        StageKind::PriorCheck => "Prior check",
        StageKind::Scout => "Scout",
        StageKind::Review => "Review",
        StageKind::Challenge => "Challenge",
        StageKind::Verify => "Verify",
        StageKind::Assemble => "Assemble",
    }
}

/// One stage-strip chip: the stage name, its agent progress when the stage
/// counts agents (`"2/3"`), and a status mark — the mockup's
/// `scout ✓, reviewer 2/3 ●` strip (spec §12).
pub fn stage_label(stage: StageKind, status: &StageStatus, progress: &str) -> String {
    let mark = match status {
        StageStatus::Done => icons::CIRCLE_CHECK,
        StageStatus::Failed(_) => icons::CIRCLE_X,
        StageStatus::Started => icons::CIRCLE_DOT,
    };
    let name = stage_name(stage);
    if progress.is_empty() {
        format!("{name} {mark}")
    } else {
        format!("{name} {progress} {mark}")
    }
}

/// The display name of an agent role.
fn role_name(role: AgentRole) -> &'static str {
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

/// The collapsible header for one timeline entry, e.g.
/// `"Reviewer · notifications"`.
pub fn entry_title(entry: &TimelineEntry) -> String {
    match entry {
        TimelineEntry::Stage { stage, status } => {
            let what = match status {
                StageStatus::Started => "started",
                StageStatus::Done => "done",
                StageStatus::Failed(_) => "failed",
            };
            format!("{} · {what}", stage_name(*stage))
        }
        TimelineEntry::Agent { agent, group, .. } => match group {
            Some(group) => format!("{} · {group}", role_name(agent.role)),
            None => {
                // Transcript-rebuilt entries carry the group only inside
                // the label (`reviewer:g2`); a bare `scout` adds nothing.
                if agent.label.is_empty() || agent.label == role_keyword(agent.role) {
                    role_name(agent.role).to_string()
                } else {
                    format!("{} · {}", role_name(agent.role), agent.label)
                }
            }
        },
        TimelineEntry::Question { agent, .. } => format!("{} asks", role_name(agent.role)),
        TimelineEntry::Approval { kind, .. } => match kind {
            ApprovalKind::Command { .. } => "Run this command?".to_string(),
            ApprovalKind::Prepare { .. } => "Run the prepare commands?".to_string(),
        },
        TimelineEntry::Finding { id } => format!("Finding #{id}"),
        TimelineEntry::ChatUser { .. } => "You".to_string(),
        TimelineEntry::ChatReply { .. } => "Chat".to_string(),
        TimelineEntry::Notice { .. } => "Notice".to_string(),
    }
}

/// The approval card's buttons, in order: three for a sandboxed command
/// (spec §8.1), two for the prepare stage (spec §8.5).
pub fn approval_buttons(kind: &ApprovalKind) -> &'static [&'static str] {
    match kind {
        ApprovalKind::Command { .. } => &["Once", "For this review", "No"],
        ApprovalKind::Prepare { .. } => &["Run", "No"],
    }
}

/// What button `index` of a command card's [`approval_buttons`] means.
/// `None` for prepare cards (use [`Self`]-free index math there: 0 runs,
/// 1 denies) and for out-of-range indexes.
pub fn command_decision(index: usize) -> Option<ApprovalDecision> {
    match index {
        0 => Some(ApprovalDecision::Once),
        1 => Some(ApprovalDecision::ForReview),
        2 => Some(ApprovalDecision::Deny { reason: None }),
        _ => None,
    }
}

/// The decision's display text, matching [`approval_buttons`] so the
/// timeline shows the same words the user clicked.
pub fn decision_text(decision: &ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Once => "Once",
        ApprovalDecision::ForReview => "For this review",
        ApprovalDecision::Deny { .. } => "No",
    }
}

/// Whether the chat composer accepts input: during a run, while paused, and
/// after it ends (the chat agent answers from the stored session, spec
/// §10.5). Planning, checkout and restore phases have no run to talk to.
pub fn chat_enabled(phase: &AiPhase) -> bool {
    matches!(phase, AiPhase::Running | AiPhase::Paused | AiPhase::Done)
}

/// Paint the tab; `show_dropped` reveals the verifier-dropped findings.
pub fn render(ui: &mut egui::Ui, st: &AiReviewState, show_dropped: bool) -> Vec<AiPanelAction> {
    let mut actions = Vec::new();
    render_stage_strip(ui, st);
    render_phase(ui, st, &mut actions);
    render_suggestions(ui, st, &mut actions);
    ui.separator();
    for (index, entry) in st.timeline.iter().enumerate() {
        render_entry(ui, st, index, entry, &mut actions);
    }
    render_findings(ui, st, show_dropped, &mut actions);
    render_usage(ui, st);
    ui.separator();
    render_chat(ui, st, &mut actions);
    render_run_controls(ui, st, &mut actions);
    actions
}

/// The compact strip above the timeline: one chip per announced stage.
fn render_stage_strip(ui: &mut egui::Ui, st: &AiReviewState) {
    let p = theme::palette_of(ui);
    let strip = st.stage_strip();
    if strip.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for (stage, status, progress) in &strip {
            let color = match status {
                StageStatus::Done => p.ok,
                StageStatus::Failed(_) => p.bad,
                StageStatus::Started => p.ai,
            };
            ui.label(
                RichText::new(stage_label(*stage, status, progress))
                    .color(color)
                    .font(fonts::semibold(11.5)),
            );
        }
    });
}

/// The phase line and the Start / Continue entry points.
fn render_phase(ui: &mut egui::Ui, st: &AiReviewState, actions: &mut Vec<AiPanelAction>) {
    let p = theme::palette_of(ui);
    let mut can_start = false;
    match &st.phase {
        AiPhase::Idle => {
            ui.weak("No AI review yet.");
            can_start = true;
        }
        AiPhase::Planning => {
            ui.spinner();
            ui.label("The checkout is planned — confirm it to start the review.");
        }
        AiPhase::CheckingOut => {
            ui.spinner();
            ui.label("Checking out the PR…");
        }
        AiPhase::Running => {
            ui.spinner();
            ui.colored_label(p.ai, "Review running");
        }
        AiPhase::Paused => {
            ui.colored_label(p.pending, "Review paused");
        }
        AiPhase::Done => {
            ui.colored_label(p.ok, "Review finished");
        }
        AiPhase::Restoring => {
            ui.spinner();
            ui.label("Restoring the checkout…");
        }
        AiPhase::Finished => {
            ui.weak("The checkout was restored.");
            can_start = true;
        }
        AiPhase::Failed(message) => {
            ui.colored_label(p.bad, format!("The review stopped: {message}"));
            can_start = true;
        }
    }
    ui.horizontal(|ui| {
        if can_start && ui.button("Start AI review").clicked() {
            actions.push(AiPanelAction::Start);
        }
        // The head comparison behind a re-review (`can_continue`) needs the
        // PR head, which the tab cannot see; offering Continue whenever a
        // completed review exists and nothing is in flight is the tab's
        // half, and `app.rs` gates the click on `can_continue` (Task 4).
        if st.last_reviewed_sha.is_some()
            && matches!(
                st.phase,
                AiPhase::Idle | AiPhase::Done | AiPhase::Finished | AiPhase::Failed(_)
            )
            && ui.button("Continue review").clicked()
        {
            actions.push(AiPanelAction::Continue);
        }
        // A plan waiting for confirmation can be abandoned from here too:
        // the confirm dialog is the usual path, but a dialog that never
        // reached the screen (or was replaced) must not strand a spinner
        // with no way out (Task 4 review findings 1 and 3).
        if st.phase == AiPhase::Planning && ui.button("Cancel").clicked() {
            actions.push(AiPanelAction::Cancel);
        }
    });
}

/// The suggested-commands card: the scout's proposals for a repo with no
/// configured commands (spec §4 step 1), shown until the user applies them
/// to the repo's AI settings (task 4).
fn render_suggestions(ui: &mut egui::Ui, st: &AiReviewState, actions: &mut Vec<AiPanelAction>) {
    let Some(suggestions) = st.suggestions.as_ref() else {
        return;
    };
    if suggestions.commands.is_empty() && suggestions.prepare.is_empty() {
        return;
    }
    let p = theme::palette_of(ui);
    card(ui, |ui| {
        ui.label(RichText::new("Suggested commands").font(fonts::semibold(12.5)));
        ui.label(
            RichText::new("The scout proposed these for this repo. Add them to the repo's AI settings so agents can run them without asking.")
                .color(p.muted)
                .size(11.5),
        );
        for command in &suggestions.commands {
            mono_line(ui, command);
        }
        if !suggestions.prepare.is_empty() {
            ui.label(
                RichText::new("Prepare (outside the sandbox)")
                    .color(p.faint)
                    .size(11.5),
            );
            for command in &suggestions.prepare {
                mono_line(ui, command);
            }
        }
        if ui.button("Add to settings").clicked() {
            actions.push(AiPanelAction::AddSuggestedCommands);
        }
    });
}

/// One timeline entry, in arrival order (spec §12).
fn render_entry(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    index: usize,
    entry: &TimelineEntry,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    match entry {
        // The strip already carries the live state; the timeline keeps the
        // transition as a faint line.
        TimelineEntry::Stage { .. } => {
            ui.label(RichText::new(entry_title(entry)).color(p.faint).size(11.5));
        }
        TimelineEntry::Agent {
            tools, text, stop, ..
        } => {
            // A running agent's entry stays open; finished ones collapse.
            egui::CollapsingHeader::new(entry_title(entry))
                .id_salt(("ai-entry", index))
                .default_open(stop.is_none())
                .show(ui, |ui| {
                    for (name, summary) in tools {
                        ui.label(
                            RichText::new(format!("{name} {summary}"))
                                .monospace()
                                .color(p.muted)
                                .size(11.5),
                        );
                    }
                    if !text.is_empty() {
                        markdown::render(ui, &markdown::to_doc(text));
                    }
                    if let Some(stop) = stop {
                        ui.label(RichText::new(stop).color(p.faint).size(11.5));
                    }
                });
        }
        TimelineEntry::Question {
            id,
            text,
            options,
            answer,
            ..
        } => card(ui, |ui| {
            ui.label(RichText::new(entry_title(entry)).font(fonts::semibold(12.5)));
            ui.label(text);
            match answer {
                Some(answer) => {
                    ui.colored_label(p.ok, format!("Answered: {answer}"));
                }
                None => {
                    let mut answered: Option<String> = None;
                    if !options.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for option in options {
                                if ui.button(option).clicked() {
                                    answered = Some(option.clone());
                                }
                            }
                        });
                    }
                    let field_id = ui.scope_id().with(("ai-answer", id));
                    let mut draft: String =
                        ui.data_mut(|d| d.get_persisted(field_id).unwrap_or_default());
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut draft)
                            .hint_text("Answer…")
                            .desired_width(f32::INFINITY),
                    );
                    let send = ui.button("Send");
                    ui.data_mut(|d| d.insert_persisted(field_id, draft.clone()));
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (send.clicked() || enter) && !draft.trim().is_empty() {
                        answered = Some(draft.trim().to_string());
                    }
                    if let Some(answer) = answered {
                        actions.push(AiPanelAction::Answer { id: *id, answer });
                    }
                }
            }
        }),
        TimelineEntry::Approval {
            id, kind, decided, ..
        } => card(ui, |ui| {
            ui.label(RichText::new(entry_title(entry)).font(fonts::semibold(12.5)));
            match kind {
                ApprovalKind::Command { command, reason } => {
                    mono_line(ui, command);
                    ui.label(RichText::new(reason).color(p.muted).size(11.5));
                }
                ApprovalKind::Prepare { reason, commands } => {
                    for command in commands {
                        mono_line(ui, command);
                    }
                    ui.label(RichText::new(reason).color(p.muted).size(11.5));
                }
            }
            match decided {
                Some(decided) => {
                    ui.colored_label(p.ok, format!("Decided: {decided}"));
                }
                None => {
                    ui.horizontal_wrapped(|ui| {
                        for (index, label) in approval_buttons(kind).iter().enumerate() {
                            if ui.button(*label).clicked() {
                                let action = match kind {
                                    ApprovalKind::Command { .. } => {
                                        command_decision(index).map(|decision| {
                                            AiPanelAction::Approve { id: *id, decision }
                                        })
                                    }
                                    ApprovalKind::Prepare { .. } => {
                                        Some(AiPanelAction::ApprovePrepare {
                                            id: *id,
                                            ok: index == 0,
                                        })
                                    }
                                };
                                if let Some(action) = action {
                                    actions.push(action);
                                }
                            }
                        }
                    });
                }
            }
        }),
        TimelineEntry::Finding { id } => {
            let finding = st.findings.get(id);
            let mut label = entry_title(entry);
            if let Some(finding) = finding {
                label = format!("{label} · {}", finding.title);
            }
            let color = finding.map_or(p.muted, |finding| severity_color(p, finding.severity));
            if ui
                .button(RichText::new(label).color(color).size(11.5))
                .clicked()
            {
                actions.push(AiPanelAction::JumpToFinding(*id));
            }
        }
        TimelineEntry::ChatUser { text } => {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new("You").font(fonts::semibold(12.5)));
                ui.label(text);
            });
        }
        TimelineEntry::ChatReply { text } => {
            ui.label(RichText::new("Chat").font(fonts::semibold(12.5)));
            markdown::render(ui, &markdown::to_doc(text));
        }
        TimelineEntry::Notice { text } => {
            ui.colored_label(p.pending, text);
        }
    }
}

/// The findings summary: severity counts, the dropped toggle, skipped
/// checks, and the prior review's states (spec §12).
fn render_findings(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    show_dropped: bool,
    actions: &mut Vec<AiPanelAction>,
) {
    use crate::ai::review::FindingState;

    let p = theme::palette_of(ui);
    let counts = st.counts();
    let live = [
        ("Bug", counts.by_severity[0], p.bad),
        ("Risk", counts.by_severity[1], p.warn),
        ("Nit", counts.by_severity[2], p.faint),
        ("Question", counts.by_severity[3], p.info),
    ];
    let any = live.iter().any(|(_, count, _)| *count > 0)
        || counts.dropped_by_verifier > 0
        || counts.skipped_checks > 0
        || counts.prior.iter().any(|count| *count > 0);
    if !any {
        return;
    }
    ui.separator();
    ui.label(RichText::new("Findings").font(fonts::semibold(12.5)));
    ui.horizontal_wrapped(|ui| {
        for (name, count, color) in live {
            if count > 0 {
                ui.label(RichText::new(format!("{name} {count}")).color(color));
            }
        }
    });
    if counts.dropped_by_verifier > 0 {
        let mut show = show_dropped;
        if ui
            .checkbox(
                &mut show,
                format!("Dropped by verifier ({})", counts.dropped_by_verifier),
            )
            .changed()
        {
            actions.push(AiPanelAction::ShowDropped(show));
        }
        if show_dropped {
            for finding in st
                .findings
                .values()
                .filter(|finding| finding.state == FindingState::DroppedByVerifier)
            {
                ui.label(
                    RichText::new(format!("#{} · {}", finding.id, finding.title))
                        .color(p.faint)
                        .size(11.5),
                );
            }
        }
    }
    if counts.skipped_checks > 0 {
        ui.colored_label(
            p.faint,
            format!("Skipped checks ({})", counts.skipped_checks),
        );
    }
    if counts.prior.iter().any(|count| *count > 0) {
        ui.colored_label(
            p.faint,
            format!(
                "Prior review: {} addressed · {} still present · {} outdated",
                counts.prior[0], counts.prior[1], counts.prior[2]
            ),
        );
    }
}

/// The run's token totals (spec §12).
fn render_usage(ui: &mut egui::Ui, st: &AiReviewState) {
    let p = theme::palette_of(ui);
    let (input, output) = st.usage;
    if input + output > 0 {
        ui.colored_label(p.faint, format!("tokens: {input} in · {output} out"));
    }
}

/// The chat composer. The draft text lives in `ui.data` so `render` can
/// stay an immutable view; it survives tab switches and snapshots.
fn render_chat(ui: &mut egui::Ui, st: &AiReviewState, actions: &mut Vec<AiPanelAction>) {
    let text_id = ui.scope_id().with("ai-chat-input");
    let enabled = chat_enabled(&st.phase);
    ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal(|ui| {
            let mut text: String = ui.data_mut(|d| d.get_persisted(text_id).unwrap_or_default());
            let field = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text("Ask the review…")
                    .desired_width(f32::INFINITY),
            );
            let send = icons::icon_button(ui, icons::SEND, "Send message");
            ui.data_mut(|d| d.insert_persisted(text_id, text.clone()));
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (send.clicked() || enter) && !text.trim().is_empty() {
                actions.push(AiPanelAction::SendChat(text.trim().to_string()));
                ui.data_mut(|d| d.insert_persisted(text_id, String::new()));
            }
        });
    });
    if !enabled {
        ui.weak("Chat is available while a review runs or after one ends.");
    }
}

/// Pause / Resume · Finish · Cancel (spec §12).
fn render_run_controls(ui: &mut egui::Ui, st: &AiReviewState, actions: &mut Vec<AiPanelAction>) {
    ui.horizontal(|ui| {
        if matches!(st.phase, AiPhase::Running) && ui.button("Pause").clicked() {
            actions.push(AiPanelAction::Pause);
        }
        if matches!(st.phase, AiPhase::Paused) && ui.button("Resume").clicked() {
            actions.push(AiPanelAction::Resume);
        }
        if matches!(
            st.phase,
            AiPhase::Running | AiPhase::Paused | AiPhase::Done | AiPhase::Failed(_)
        ) && ui.button("Finish").clicked()
        {
            actions.push(AiPanelAction::Finish);
        }
        if matches!(st.phase, AiPhase::Running | AiPhase::Paused) && ui.button("Cancel").clicked() {
            actions.push(AiPanelAction::Cancel);
        }
    });
}

/// A raised card for a question or approval.
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.raised)
        .corner_radius(8.0)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, add);
}

/// A mono command line inside a card.
fn mono_line(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.surface)
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).monospace().size(11.5).color(p.text));
        });
}

/// The accent for a finding's severity, matching the diff's draft styling.
pub(crate) fn severity_color(p: &theme::Palette, severity: crate::ai::review::Severity) -> Color32 {
    use crate::ai::review::Severity;
    match severity {
        Severity::Bug => p.bad,
        Severity::Risk => p.warn,
        Severity::Nit => p.faint,
        Severity::Question => p.info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::tools::AgentRef;
    use crate::state::ai_review::BriefSuggestions;

    fn reviewer(group: Option<&str>) -> TimelineEntry {
        TimelineEntry::Agent {
            agent: AgentRef {
                id: 1,
                role: AgentRole::Reviewer,
                label: group.map_or("reviewer".into(), |g| format!("reviewer:{g}")),
            },
            group: group.map(str::to_string),
            tools: Vec::new(),
            text: String::new(),
            stop: None,
        }
    }

    #[test]
    fn stage_label_names_stages_and_marks_status() {
        assert_eq!(
            stage_label(StageKind::Scout, &StageStatus::Done, ""),
            format!("Scout {}", icons::CIRCLE_CHECK)
        );
        assert_eq!(
            stage_label(StageKind::Review, &StageStatus::Started, "2/3"),
            format!("Review 2/3 {}", icons::CIRCLE_DOT)
        );
        assert_eq!(
            stage_label(StageKind::Verify, &StageStatus::Failed("boom".into()), ""),
            format!("Verify {}", icons::CIRCLE_X)
        );
    }

    #[test]
    fn entry_title_names_agents_by_group() {
        assert_eq!(
            entry_title(&reviewer(Some("notifications"))),
            "Reviewer · notifications"
        );
        assert_eq!(
            entry_title(&reviewer(None)),
            "Reviewer",
            "a bare reviewer is just its role"
        );
        let scout = TimelineEntry::Agent {
            agent: AgentRef {
                id: 2,
                role: AgentRole::Scout,
                label: "scout".to_string(),
            },
            group: None,
            tools: Vec::new(),
            text: String::new(),
            stop: None,
        };
        assert_eq!(
            entry_title(&scout),
            "Scout",
            "the label duplicating the role is dropped"
        );
        let rebuilt = TimelineEntry::Agent {
            agent: AgentRef {
                id: 0,
                role: AgentRole::Reviewer,
                label: "reviewer:g2".to_string(),
            },
            group: None,
            tools: Vec::new(),
            text: String::new(),
            stop: None,
        };
        assert_eq!(
            entry_title(&rebuilt),
            "Reviewer · reviewer:g2",
            "a transcript-rebuilt entry keeps its label as the qualifier"
        );
        assert_eq!(
            entry_title(&TimelineEntry::Finding { id: 7 }),
            "Finding #7",
            "finding rows name their id"
        );
    }

    #[test]
    fn approval_buttons_match_the_kind() {
        assert_eq!(
            approval_buttons(&ApprovalKind::Command {
                command: "cargo test".into(),
                reason: "runs code".into()
            }),
            ["Once", "For this review", "No"]
        );
        assert_eq!(
            approval_buttons(&ApprovalKind::Prepare {
                reason: "fetches deps".into(),
                commands: vec!["cargo fetch".into()]
            }),
            ["Run", "No"]
        );
        assert_eq!(
            command_decision(1),
            Some(ApprovalDecision::ForReview),
            "button order maps to decisions"
        );
        assert_eq!(
            decision_text(&ApprovalDecision::ForReview),
            "For this review"
        );
    }

    #[test]
    fn chat_enabled_only_with_a_run_to_talk_to() {
        for (phase, want) in [
            (AiPhase::Idle, false),
            (AiPhase::Planning, false),
            (AiPhase::CheckingOut, false),
            (AiPhase::Running, true),
            (AiPhase::Paused, true),
            (AiPhase::Done, true),
            (AiPhase::Restoring, false),
            (AiPhase::Finished, false),
            (AiPhase::Failed("boom".to_string()), false),
        ] {
            assert_eq!(chat_enabled(&phase), want, "phase {phase:?}");
        }
    }

    /// Every entry kind, phase, and the summary must paint without
    /// panicking; nothing is clicked, so nothing reports actions.
    #[test]
    fn render_paints_the_whole_tab_without_clicking_anything() {
        use crate::ai::review::{
            CheckResult, CoverageEntry, Finding, FindingAnchor, FindingState, Severity,
        };
        use crate::ai::store::{CheckoutRecord, CheckoutState, OriginalRef};
        use crate::model::comment::Anchor;
        use crate::model::diff::Side;
        use std::path::PathBuf;

        let agent = AgentRef {
            id: 1,
            role: AgentRole::Reviewer,
            label: "reviewer:g1".to_string(),
        };
        let mut st = AiReviewState {
            phase: AiPhase::Running,
            checkout: Some(CheckoutRecord {
                id: 4,
                repo_path: PathBuf::from("/repo"),
                repo: "o/r".to_string(),
                pr_number: 7,
                original: OriginalRef::Branch("main".to_string()),
                stash_sha: None,
                head_sha: "sha_head".to_string(),
                state: CheckoutState::Active,
            }),
            timeline: vec![
                TimelineEntry::Stage {
                    stage: StageKind::Scout,
                    status: StageStatus::Done,
                },
                TimelineEntry::Agent {
                    agent: agent.clone(),
                    group: Some("notifications".to_string()),
                    tools: vec![("read_file".to_string(), "src/a.rs".to_string())],
                    text: "Checking the retry path.".to_string(),
                    stop: None,
                },
                TimelineEntry::Question {
                    id: 7,
                    agent: agent.clone(),
                    text: "Is an empty notification list allowed?".to_string(),
                    options: vec!["yes".into(), "no".into()],
                    answer: None,
                },
                TimelineEntry::Approval {
                    id: 3,
                    agent: agent.clone(),
                    kind: ApprovalKind::Command {
                        command: "cargo test -p notifier".to_string(),
                        reason: "it runs code".to_string(),
                    },
                    decided: None,
                },
                TimelineEntry::Approval {
                    id: 4,
                    agent: agent.clone(),
                    kind: ApprovalKind::Prepare {
                        reason: "fetches deps outside the sandbox".to_string(),
                        commands: vec!["cargo fetch".to_string()],
                    },
                    decided: Some("Run".to_string()),
                },
                TimelineEntry::Finding { id: 1 },
                TimelineEntry::ChatUser {
                    text: "focus on the retry path".to_string(),
                },
                TimelineEntry::ChatReply {
                    text: "Noted — the reviewers cover it.".to_string(),
                },
                TimelineEntry::Notice {
                    text: "the agent shell is unsandboxed: no bwrap".to_string(),
                },
            ],
            coverage: vec![CoverageEntry {
                file: "src/a.rs".to_string(),
                item: "errors".to_string(),
                result: CheckResult::Skipped,
                note: String::new(),
            }],
            usage: (12_000, 900),
            ..AiReviewState::default()
        };
        st.findings.insert(
            1,
            Finding {
                id: 1,
                run_id: 9,
                agent: agent.clone(),
                anchor: FindingAnchor::Line(Anchor {
                    path: "src/a.rs".to_string(),
                    side: Side::Right,
                    line: 3,
                    start_line: None,
                    start_side: None,
                }),
                severity: Severity::Bug,
                title: "retry loop never terminates on empty pages".to_string(),
                body: "body".to_string(),
                evidence: vec![],
                checklist_item: None,
                state: FindingState::Draft,
                state_reason: None,
                pending_local_id: None,
            },
        );
        st.findings.insert(
            2,
            Finding {
                state: FindingState::DroppedByVerifier,
                severity: Severity::Nit,
                title: "dropped nit".to_string(),
                ..st.findings[&1].clone()
            },
        );

        for (phase, show_dropped) in [
            (AiPhase::Idle, false),
            (AiPhase::Running, false),
            (AiPhase::Running, true),
            (AiPhase::Paused, false),
            (AiPhase::Done, false),
            (AiPhase::Finished, false),
            (AiPhase::Failed("boom".to_string()), false),
        ] {
            st.phase = phase.clone();
            let mut painted = AiReviewState::clone(&st);
            painted.phase = phase;
            egui::__run_test_ui(|ui| {
                assert!(
                    render(ui, &painted, show_dropped).is_empty(),
                    "painting alone reports no actions"
                );
            });
        }
    }

    fn panel_harness(st: AiReviewState) -> egui_kittest::Harness<'static, Vec<AiPanelAction>> {
        egui_kittest::Harness::builder()
            .with_size([420.0, 900.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui_state(
                move |ui, actions: &mut Vec<AiPanelAction>| {
                    // Accumulate: kittest runs a follow-up frame after the
                    // click, and that frame reports nothing.
                    actions.extend(render(ui, &st, false));
                },
                Vec::new(),
            )
    }

    /// The suggested-commands card offers "Add to settings" (task 4), and a
    /// finished run offers a fresh Start from the tab (ruling 7b).
    #[test]
    fn suggestions_and_finished_phase_report_their_actions() {
        use egui_kittest::kittest::Queryable;

        let mut harness = panel_harness(AiReviewState {
            suggestions: Some(BriefSuggestions {
                commands: vec!["cargo test".to_string()],
                prepare: vec!["cargo fetch".to_string()],
            }),
            ..AiReviewState::default()
        });
        harness.get_by_label("Add to settings").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiPanelAction::AddSuggestedCommands],
            "the card's button reports the settings action"
        );

        let mut harness = panel_harness(AiReviewState {
            phase: AiPhase::Finished,
            ..AiReviewState::default()
        });
        harness.get_by_label("Start AI review").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiPanelAction::Start],
            "a restored run can be started again from the tab"
        );
    }

    /// A tab waiting for a checkout confirmation offers its own way out of
    /// the spinner, so a dialog that never reached the screen (or was
    /// replaced by the recovery list) cannot strand it (Task 4 review
    /// findings 1 and 3).
    #[test]
    fn a_planning_tab_can_cancel_from_the_panel() {
        use egui_kittest::kittest::Queryable;

        let mut harness = panel_harness(AiReviewState {
            phase: AiPhase::Planning,
            ..AiReviewState::default()
        });
        harness.get_by_label("Cancel").click();
        // The planning spinner asks for a repaint every frame, so the
        // harness cannot settle: step the click frame and the one after.
        harness.step();
        harness.step();
        assert_eq!(*harness.state(), vec![AiPanelAction::Cancel]);
    }
}
