//! The ✦ AI tab (spec §12): one session-like timeline for this PR's AI run.
//!
//! The tab is a pure view over [`AiReviewState`] (Task 2): the stage strip,
//! the timeline (stage rows, collapsible agent entries with their tool
//! calls, question cards, approval cards, findings, chat, notices), the
//! findings summary, the token usage, the chat composer, and the run
//! controls. Like every widget here it never mutates state: every control
//! returns an [`AiPanelAction`] for `app.rs` to turn into a worker
//! [`crate::runtime::Command`]. Unix-only with [`crate::ai`].

use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, Frame, Margin, RichText};

use crate::ai::events::{StageKind, StageStatus};
use crate::ai::interaction::ApprovalDecision;
use crate::ai::repo_link::{FolderStatus, folder_status, start_blockers};
use crate::ai::store::CheckoutRecord;
use crate::ai::tools::AgentRole;
use crate::ai::ui_interaction::ApprovalKind;
use crate::persist::settings::{AiSettings, RepoAiSettings};
use crate::state::ai_review::{
    AiPhase, AiReviewState, ClassifierNote, TimelineEntry, change_blocked, classifier_note_text,
    unrestored_for_repo,
};
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
    /// Keep a summary-anchored draft into the review body (spec §5.6): the
    /// tab is its only Keep / Drop surface, since the diff has no row to
    /// anchor a whole-file remark to.
    KeepSummary(i64),
    /// Drop a summary-anchored draft (spec §5.6).
    DropSummary(i64),
    /// Toggle the dropped-by-verifier findings (spec §12).
    ShowDropped(bool),
    /// User clicked "Add to settings" on the suggested-commands card
    /// (task 4): copy the brief's commands into the repo's AI settings.
    AddSuggestedCommands,
    /// Choose or change this repo's local folder: the app opens the OS
    /// picker and probes the answer (AI settings Task 7). The row's
    /// "Choose folder…" and "Change…" buttons.
    ChooseFolder,
    /// Unlink the repo's local folder; the commands, prepare commands and
    /// extra paths stay for a relink (AI settings Task 7).
    UnlinkFolder,
    /// Reopen the recovery dialog for this repo's unrestored checkouts
    /// (dismissed earlier with "Later"); the records ride along.
    RestoreCheckout(Vec<CheckoutRecord>),
    /// Open the repo-commands dialog from the row's edit affordance
    /// (AI settings Task 7).
    EditRepoCommands,
}

/// The Local checkout row's view-model (AI settings Task 7): what the
/// folder is, whether it is usable, and what stands between the user and
/// Start. `app.rs` builds it per frame from the settings and the cached
/// keychain probe; the only I/O in the build is [`folder_status`],
/// filesystem-only by design (the clone check itself ran on the worker
/// when the folder was picked, and its complaint rides in as `error`).
#[derive(Clone, Debug, PartialEq)]
pub struct LocalCheckoutVm {
    /// `owner/name`, the row's title.
    pub repo: String,
    /// The linked folder; `None` when the repo has no folder yet.
    pub path: Option<PathBuf>,
    /// Whether the folder is still there and still a repository.
    pub status: FolderStatus,
    /// Why the last folder choice was refused — a probe failure or a clone
    /// of another repository — shown in the row, never a toast.
    pub error: Option<String>,
    /// Why Start is disabled, in display order; empty means Start is live.
    pub blockers: Vec<String>,
    /// Why Change… / Unlink are disabled, or `None` when the link can move.
    pub change_blocked: Option<String>,
    /// The repo's unrestored checkouts a Restore… action can offer (empty
    /// when nothing waits or a run is live); the records [`change_blocked`]
    /// names, for the recovery dialog.
    pub restore: Vec<CheckoutRecord>,
    /// Full trust (`[ai.auto_approve].dangerously_approve_all`) is on: the
    /// tab warns that every command will run without asking.
    pub trust_all: bool,
}

/// Window-level data the tab displays but does not own.
#[derive(Clone, Copy, Default)]
pub struct PanelInputs<'a> {
    /// The Local checkout row above everything else; `None` hides the row
    /// (a tab painted without repo context, as in some tests).
    pub checkout: Option<&'a LocalCheckoutVm>,
    /// The repo's AI settings entry, for the commands summary; `None`
    /// when the repo has no entry yet.
    pub repo: Option<&'a RepoAiSettings>,
}

/// Build the Local checkout row's view-model for `repo_key`
/// (`host/owner/name`, matched case-insensitively like [`start_blockers`]).
///
/// `has_key` answers whether a provider's API key is stored: the app fills
/// it from its cached probe, never from the keychain, which blocks.
/// `error` is the app's complaint about the last folder choice, if any.
pub fn local_checkout_vm(
    ai: &AiSettings,
    repo_key: &str,
    ai_state: &AiReviewState,
    unrestored: &[CheckoutRecord],
    has_key: &dyn Fn(&str) -> bool,
    error: Option<String>,
) -> LocalCheckoutVm {
    let path = ai
        .repos
        .iter()
        .find(|entry| entry.repo.eq_ignore_ascii_case(repo_key))
        .map(|entry| entry.path.clone())
        .filter(|path| !path.as_os_str().is_empty());
    let status = folder_status(path.as_deref().unwrap_or(Path::new("")));
    LocalCheckoutVm {
        repo: repo_key
            .split_once('/')
            .map_or(repo_key, |(_, slug)| slug)
            .to_string(),
        path,
        status,
        error,
        blockers: start_blockers(ai, repo_key, has_key),
        change_blocked: change_blocked(ai_state, unrestored, repo_key),
        restore: unrestored_for_repo(ai_state, unrestored, repo_key),
        trust_all: ai.auto_approve.dangerously_approve_all,
    }
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
        TimelineEntry::ChatTool { name, .. } => format!("Chat · {name}"),
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

/// Paint the tab; `inputs` carries the Local checkout row and the repo's
/// settings entry, `show_dropped` reveals the verifier-dropped findings.
pub fn render(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    inputs: &PanelInputs,
    show_dropped: bool,
) -> Vec<AiPanelAction> {
    let mut actions = Vec::new();
    if let Some(vm) = inputs.checkout {
        render_local_checkout(ui, vm, inputs.repo, &mut actions);
        // A standing reminder while full trust is on: the agent asks nothing
        // and can read secret paths (Settings → AI → Auto-approve).
        if vm.trust_all {
            let p = theme::palette_of(ui);
            ui.colored_label(
                p.bad,
                RichText::new("Full trust is on: every command runs without asking.").strong(),
            );
        }
        ui.separator();
    }
    render_stage_strip(ui, st);
    render_phase(ui, st, inputs.checkout, &mut actions);
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

/// The Local checkout row at the top of the tab (spec: AI settings —
/// "PR → AI tab: Local checkout row"): the folder, the clone-check result,
/// the Choose… / Change… / Unlink controls, the probe's complaint, and the
/// repo's commands with an edit affordance. Shown before a review starts
/// and kept visible afterwards.
fn render_local_checkout(
    ui: &mut egui::Ui,
    vm: &LocalCheckoutVm,
    repo: Option<&RepoAiSettings>,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    card(ui, |ui| {
        ui.label(
            RichText::new(format!("Local checkout for {}", vm.repo)).font(fonts::semibold(12.5)),
        );
        let blocked = vm.change_blocked.is_some();
        match &vm.path {
            Some(path) => {
                ui.horizontal_wrapped(|ui| {
                    // A long path is one unbreakable-ish token: truncate,
                    // full text on hover, so the row cannot widen the pane.
                    let shown = path.display().to_string();
                    ui.add(
                        egui::Label::new(RichText::new(&shown).monospace().size(11.5)).truncate(),
                    )
                    .on_hover_text(shown);
                    match vm.status {
                        FolderStatus::Ok => {
                            ui.colored_label(
                                p.ok,
                                format!("{} clone of {}", icons::CHECK, vm.repo),
                            );
                        }
                        FolderStatus::Missing => {
                            ui.colored_label(p.bad, "folder is missing");
                        }
                        FolderStatus::NotARepo => {
                            ui.colored_label(p.bad, "not a git repository");
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(!blocked, |ui| {
                            if ui.button("Unlink").clicked() {
                                actions.push(AiPanelAction::UnlinkFolder);
                            }
                            if ui.button("Change…").clicked() {
                                actions.push(AiPanelAction::ChooseFolder);
                            }
                        });
                    });
                });
            }
            None => {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("No folder yet — Start stays disabled until one is linked.");
                    ui.add_enabled_ui(!blocked, |ui| {
                        if ui.button("Choose folder…").clicked() {
                            actions.push(AiPanelAction::ChooseFolder);
                        }
                    });
                });
            }
        }
        if let Some(reason) = &vm.change_blocked {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(p.pending, reason);
                // The blocker the user can act on: the recovery dialog
                // they dismissed with "Later", reopened from here.
                if !vm.restore.is_empty()
                    && ui
                        .button("Restore…")
                        .on_hover_text("Restore the checkout this AI review left behind")
                        .clicked()
                {
                    actions.push(AiPanelAction::RestoreCheckout(vm.restore.clone()));
                }
            });
        }
        if let Some(error) = &vm.error {
            ui.colored_label(p.bad, error);
        }
        render_commands_summary(ui, repo, actions);
    });
}

/// The repo's allowed commands and prepare commands with the edit
/// affordance (they moved here from Settings; the dialog edits the entry
/// even before a folder is linked).
fn render_commands_summary(
    ui: &mut egui::Ui,
    repo: Option<&RepoAiSettings>,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    ui.horizontal_wrapped(|ui| {
        match repo.filter(|repo| !repo.commands.is_empty()) {
            Some(repo) => {
                ui.label(
                    RichText::new("Commands agents may run:")
                        .color(p.muted)
                        .size(11.5),
                );
                ui.label(
                    RichText::new(repo.commands.join(", "))
                        .monospace()
                        .size(11.5),
                );
            }
            None => {
                ui.label(
                    RichText::new("No commands yet — agents ask before running anything.")
                        .color(p.muted)
                        .size(11.5),
                );
            }
        }
        if let Some(repo) = repo.filter(|repo| !repo.prepare.is_empty()) {
            ui.label(
                RichText::new(format!("· Prepare: {}", repo.prepare.join(", ")))
                    .color(p.muted)
                    .size(11.5),
            );
        }
        if ui.link("edit").clicked() {
            actions.push(AiPanelAction::EditRepoCommands);
        }
    });
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

/// The phase line and the Start / Continue entry points. Start (and
/// Continue) stay disabled while the Local checkout row lists blockers —
/// the reasons are stated right under the buttons (AI settings Task 7).
fn render_phase(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    checkout: Option<&LocalCheckoutVm>,
    actions: &mut Vec<AiPanelAction>,
) {
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
    // What stands between the user and a run; no row (no repo context)
    // means nothing here is known to block the start.
    let blockers: &[String] = checkout.map_or(&[], |vm| vm.blockers.as_slice());
    ui.horizontal_wrapped(|ui| {
        if can_start {
            let start = ui
                .add_enabled_ui(blockers.is_empty(), |ui| ui.button("Start AI review"))
                .inner;
            let start = if blockers.is_empty() {
                start
            } else {
                start.on_hover_text(blockers.join("\n"))
            };
            if start.clicked() {
                actions.push(AiPanelAction::Start);
            }
        }
        // The head comparison behind a re-review (`can_continue`) needs the
        // PR head, which the tab cannot see; offering Continue whenever a
        // completed review exists and nothing is in flight is the tab's
        // half, and `app.rs` gates the click on `can_continue` (Task 4).
        // The same blockers that stop Start stop a re-review too.
        if st.last_reviewed_sha.is_some()
            && matches!(
                st.phase,
                AiPhase::Idle | AiPhase::Done | AiPhase::Finished | AiPhase::Failed(_)
            )
        {
            let cont = ui
                .add_enabled_ui(blockers.is_empty(), |ui| ui.button("Continue review"))
                .inner;
            if cont.clicked() {
                actions.push(AiPanelAction::Continue);
            }
        }
        // A plan waiting for confirmation can be abandoned from here too:
        // the confirm dialog is the usual path, but a dialog that never
        // reached the screen (or was replaced) must not strand a spinner
        // with no way out (Task 4 review findings 1 and 3).
        if st.phase == AiPhase::Planning && ui.button("Cancel").clicked() {
            actions.push(AiPanelAction::Cancel);
        }
    });
    if can_start {
        for reason in blockers {
            ui.colored_label(p.pending, reason);
        }
    }
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
                    for line in tools {
                        // Tool summaries are JSON one-liners with long
                        // unbreakable tokens; truncate to the pane width and
                        // show the full line on hover, or the tab's min width
                        // would widen the right pane past the splitter
                        // (the ScrollArea expands to fit its content).
                        tool_line(ui, &format!("{} {}", line.name, line.summary), p.muted);
                        if let Some(note) = line.note {
                            classifier_note_label(ui, note, p);
                        }
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
            // Truncate like the tool lines: a long finding title must not
            // widen the pane past the splitter. The tooltip repeats the full
            // label only when there is one to reveal.
            let button =
                egui::Button::new(RichText::new(label.clone()).color(color).size(11.5)).truncate();
            let button = if finding.is_some() {
                ui.add(button).on_hover_text(label)
            } else {
                ui.add(button)
            };
            if button.clicked() {
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
        TimelineEntry::ChatTool {
            name,
            summary,
            note,
        } => {
            tool_line(ui, &format!("{name} {summary}"), p.muted);
            if let Some(note) = note {
                classifier_note_label(ui, *note, p);
            }
        }
        TimelineEntry::Notice { text } => {
            ui.colored_label(p.pending, text);
        }
    }
}

/// The findings summary: severity counts, the summary items with their
/// Keep / Drop, the dropped toggle, skipped checks, and the prior review's
/// states (spec §12).
fn render_findings(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    show_dropped: bool,
    actions: &mut Vec<AiPanelAction>,
) {
    use crate::ai::review::{FindingAnchor, FindingState};

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
    // Summary-anchored drafts never reach the diff (`place_ai_draft` has no
    // row to anchor a whole-file remark to), so this list is their only
    // Keep / Drop surface (spec §5.6).
    let summaries = st.summary_drafts(show_dropped);
    if !summaries.is_empty() {
        ui.label(RichText::new("Summary items").color(p.muted).size(11.5));
        for finding in summaries {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(finding.severity.label())
                        .color(severity_color(p, finding.severity))
                        .size(11.0)
                        .strong(),
                );
                ui.label(RichText::new(&finding.title).size(11.5));
                // The path is one unbreakable word: truncate, full path on
                // hover, so a deep path cannot widen the pane.
                ui.add(
                    egui::Label::new(
                        RichText::new(finding.anchor.path())
                            .monospace()
                            .color(p.faint)
                            .size(11.0),
                    )
                    .truncate(),
                )
                .on_hover_text(finding.anchor.path());
                if finding.state == FindingState::DroppedByVerifier {
                    ui.label(
                        RichText::new("dropped by verifier")
                            .color(p.muted)
                            .size(11.0),
                    );
                }
                if ui.button("Keep").clicked() {
                    actions.push(AiPanelAction::KeepSummary(finding.id));
                }
                if ui.button("Drop").clicked() {
                    actions.push(AiPanelAction::DropSummary(finding.id));
                }
            });
        }
    }
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
            // The summary-anchored drops carry Keep / Drop in the list
            // above; this one names the line-anchored ones.
            for finding in st.findings.values().filter(|finding| {
                finding.state == FindingState::DroppedByVerifier
                    && !matches!(finding.anchor, FindingAnchor::Summary { .. })
            }) {
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
        // Right-to-left so the send button claims its width first and the
        // field fills the rest: in a left-to-right row an `INFINITY` field
        // takes the full available width and the button overflows past it,
        // which grows the pane a little every frame (the details ScrollArea
        // expands to its content's min width).
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let send = icons::icon_button(ui, icons::SEND, "Send message");
            let mut text: String = ui.data_mut(|d| d.get_persisted(text_id).unwrap_or_default());
            let field = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text("Ask the review…")
                    .desired_width(f32::INFINITY),
            );
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
    ui.horizontal_wrapped(|ui| {
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

/// A mono command line inside a card. Commands contain spaces, so word
/// wrapping keeps the whole command visible at any pane width (the min width
/// is the longest single token): the approval decision must not hide the
/// command tail — where the risk lives (`&& rm -rf …`) — behind a transient
/// tooltip the way the noisy tool-call lines may.
fn mono_line(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.surface)
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text).monospace().size(11.5).color(p.text))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
}

/// One monospace tool-call line that truncates instead of widening the
/// pane; the full text lives in the hover tooltip. (Approval commands use
/// [`mono_line`] instead: they must stay fully visible.)
fn tool_line(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text).monospace().color(color).size(11.5)).truncate())
        .on_hover_text(text);
}

/// The classifier's verdict under a `bash` tool line, in the badge colour
/// its meaning deserves: quiet approval in green, an ask it could not
/// settle in amber, danger in red, the rest muted.
fn classifier_note_label(ui: &mut egui::Ui, note: ClassifierNote, p: &theme::Palette) {
    let color = match note {
        ClassifierNote::Approved => p.ok,
        ClassifierNote::Blocked | ClassifierNote::NeedsApproval => p.pending,
        ClassifierNote::Dangerous => p.bad,
        ClassifierNote::Unavailable => p.faint,
    };
    ui.label(
        RichText::new(classifier_note_text(note))
            .color(color)
            .size(11.0),
    );
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
    use crate::state::ai_review::{BriefSuggestions, ToolLine};

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
                    tools: vec![ToolLine {
                        name: "read_file".to_string(),
                        summary: "src/a.rs".to_string(),
                        note: None,
                    }],
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
        // Summary-anchored items: the tab's findings list is their only
        // Keep / Drop surface (spec §5.6).
        st.findings.insert(
            3,
            Finding {
                id: 3,
                anchor: FindingAnchor::Summary {
                    path: "assets/logo.png".to_string(),
                },
                severity: Severity::Risk,
                title: "the exported asset lost its license header".to_string(),
                state: FindingState::Draft,
                ..st.findings[&1].clone()
            },
        );
        st.findings.insert(
            4,
            Finding {
                id: 4,
                anchor: FindingAnchor::Summary {
                    path: "vendor/big.js".to_string(),
                },
                severity: Severity::Question,
                title: "why is a vendored bundle in the diff?".to_string(),
                state: FindingState::DroppedByVerifier,
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
                    render(ui, &painted, &PanelInputs::default(), show_dropped).is_empty(),
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
                    actions.extend(render(ui, &st, &PanelInputs::default(), false));
                },
                Vec::new(),
            )
    }

    /// [`panel_harness`] with the Local checkout row and the repo's
    /// settings entry painted above the tab (AI settings Task 7).
    fn panel_harness_row(
        st: AiReviewState,
        vm: LocalCheckoutVm,
        repo: Option<RepoAiSettings>,
    ) -> egui_kittest::Harness<'static, Vec<AiPanelAction>> {
        egui_kittest::Harness::builder()
            .with_size([420.0, 900.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui_state(
                move |ui, actions: &mut Vec<AiPanelAction>| {
                    let inputs = PanelInputs {
                        checkout: Some(&vm),
                        repo: repo.as_ref(),
                    };
                    actions.extend(render(ui, &st, &inputs, false));
                },
                Vec::new(),
            )
    }

    /// A row with no folder: Start is disabled and every blocker is stated
    /// in the tab (never a toast), while the row's Choose… still reports
    /// its action (AI settings Task 7).
    #[test]
    fn start_disabled_with_reason_when_not_linked() {
        use egui_kittest::kittest::Queryable;

        let vm = LocalCheckoutVm {
            repo: "acme/widgets".to_string(),
            path: None,
            status: FolderStatus::Missing,
            error: None,
            blockers: vec!["Link a local folder for github.com/acme/widgets.".to_string()],
            change_blocked: None,
            restore: Vec::new(),
            trust_all: false,
        };
        let mut harness = panel_harness_row(AiReviewState::default(), vm, None);
        harness.get_by_label("Start AI review").click();
        harness.run();
        assert!(
            harness.state().is_empty(),
            "a blocked Start reports nothing: {:?}",
            harness.state()
        );
        harness.get_by_label("Link a local folder for github.com/acme/widgets.");
        harness.get_by_label("Choose folder…").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiPanelAction::ChooseFolder],
            "the row's only control while unlinked is the picker"
        );
    }

    /// The linked row reports Change…, Unlink, and the commands summary's
    /// edit affordance (spec: AI settings — the row owns the link and the
    /// commands moved here from Settings).
    #[test]
    fn the_linked_row_reports_change_unlink_and_edit() {
        use egui_kittest::kittest::Queryable;
        use std::path::PathBuf;

        let vm = LocalCheckoutVm {
            repo: "acme/widgets".to_string(),
            path: Some(PathBuf::from("/src/widgets")),
            status: FolderStatus::Ok,
            error: None,
            blockers: Vec::new(),
            change_blocked: None,
            restore: Vec::new(),
            trust_all: false,
        };
        let repo = RepoAiSettings {
            repo: "github.com/acme/widgets".to_string(),
            path: PathBuf::from("/src/widgets"),
            commands: vec!["cargo test".to_string()],
            prepare: vec!["cargo fetch".to_string()],
            extra_write_paths: Vec::new(),
            extra_deny_read: Vec::new(),
        };

        let mut harness = panel_harness_row(AiReviewState::default(), vm.clone(), Some(repo));
        harness.get_by_label("Change…").click();
        harness.run();
        assert_eq!(*harness.state(), vec![AiPanelAction::ChooseFolder]);

        let mut harness = panel_harness_row(AiReviewState::default(), vm.clone(), None);
        harness.get_by_label("Unlink").click();
        harness.run();
        assert_eq!(*harness.state(), vec![AiPanelAction::UnlinkFolder]);

        let mut harness = panel_harness_row(AiReviewState::default(), vm, None);
        harness.get_by_label("edit").click();
        harness.run();
        assert_eq!(*harness.state(), vec![AiPanelAction::EditRepoCommands]);
    }

    /// Change… and Unlink are disabled with the reason while a run for the
    /// repo is live or one of its checkouts is unrestored: the restore
    /// logic depends on the path.
    #[test]
    fn change_and_unlink_stay_disabled_while_blocked() {
        use egui_kittest::kittest::Queryable;
        use std::path::PathBuf;

        let vm = LocalCheckoutVm {
            repo: "acme/widgets".to_string(),
            path: Some(PathBuf::from("/src/widgets")),
            status: FolderStatus::Ok,
            error: None,
            blockers: Vec::new(),
            change_blocked: Some(
                "An AI review is running for this repository — finish it before changing the folder."
                    .to_string(),
            ),
            restore: Vec::new(),
            trust_all: false,
        };
        let mut harness = panel_harness_row(
            AiReviewState {
                phase: AiPhase::Running,
                ..AiReviewState::default()
            },
            vm,
            None,
        );
        harness.get_by_label("Unlink").click();
        harness.get_by_label("Change…").click();
        // The Running phase's spinner repaints forever, so `run` would
        // never settle: a few steps is all the clicks need.
        harness.run_steps(4);
        assert!(
            harness.state().is_empty(),
            "blocked controls report nothing: {:?}",
            harness.state()
        );
        harness.get_by_label(
            "An AI review is running for this repository — finish it before changing the folder.",
        );
    }

    /// A dismissed recovery dialog is reopenable: with unrestored records
    /// the row shows Restore… next to the blocker and hands the records to
    /// the app, which reopens the dialog.
    #[test]
    fn the_row_offers_to_reopen_recovery() {
        use crate::ai::store::{CheckoutState, OriginalRef};
        use egui_kittest::kittest::Queryable;
        use std::path::PathBuf;

        let record = CheckoutRecord {
            id: 4,
            repo_path: PathBuf::from("/src/widgets"),
            repo: "acme/widgets".to_string(),
            pr_number: 7,
            original: OriginalRef::Branch("main".to_string()),
            stash_sha: None,
            head_sha: "sha_head".to_string(),
            state: CheckoutState::Failed,
        };
        let vm = LocalCheckoutVm {
            repo: "acme/widgets".to_string(),
            path: Some(PathBuf::from("/src/widgets")),
            status: FolderStatus::Ok,
            error: None,
            blockers: Vec::new(),
            change_blocked: Some(
                "This repository has an unrestored AI checkout — restore it before changing the folder or starting a review."
                    .to_string(),
            ),
            restore: vec![record.clone()],
            trust_all: false,
        };
        let mut harness = panel_harness_row(AiReviewState::default(), vm, None);
        harness.run();
        harness.get_by_label("Restore…").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiPanelAction::RestoreCheckout(vec![record])]
        );
    }

    /// The row's error — the probe's complaint about the last folder
    /// choice — is shown in the row, not a toast.
    #[test]
    fn the_row_shows_the_probe_error() {
        use egui_kittest::kittest::Queryable;
        use std::path::PathBuf;

        let vm = LocalCheckoutVm {
            repo: "acme/widgets".to_string(),
            path: Some(PathBuf::from("/src/other")),
            status: FolderStatus::Ok,
            error: Some(
                "that folder is a clone of github.com/acme/api, not github.com/acme/widgets"
                    .to_string(),
            ),
            blockers: Vec::new(),
            change_blocked: None,
            restore: Vec::new(),
            trust_all: false,
        };
        let mut harness = panel_harness_row(AiReviewState::default(), vm, None);
        harness.run();
        harness.get_by_label(
            "that folder is a clone of github.com/acme/api, not github.com/acme/widgets",
        );
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

    /// The right-pane layout from `pr_detail.rs`: a resizable panel whose
    /// details `ScrollArea` uses `auto_shrink([false, false])`, which makes
    /// the tab's min content width the pane's min width (the panel feeds its
    /// own response rect back as the next frame's size). A 400+ char tool
    /// JSON with unbreakable path tokens and a long approval command must
    /// therefore neither widen the pane past its splitter width nor leave
    /// content outside the visible clip rect — and the approval command must
    /// stay FULLY visible (wrapped, not truncated: the command tail is where
    /// the risk lives), its tail present in the label rather than behind a
    /// tooltip.
    #[test]
    fn long_tool_and_approval_lines_fit_a_narrow_right_pane() {
        use egui_kittest::kittest::Queryable;
        use std::cell::RefCell;
        use std::rc::Rc;

        let agent = AgentRef {
            id: 1,
            role: AgentRole::Reviewer,
            label: "reviewer:g1".to_string(),
        };
        let long_json = "bash {\"command\":\"for f in docs/superpowers/plans/2026-10-20-ai-review-1-foundation.md docs/superpowers/plans/2026-10-20-ai-review-2-state-machine.md docs/superpowers/plans/2026-10-20-ai-review-3-ui-chat.md docs/superpowers/specs/2026-10-19-ai-review-design.md ; do echo \\\"=== $f ===\\\"; head -40 \\\"$f\\\"; done\"}".to_string();
        assert!(
            long_json.len() > 300,
            "the fixture must be a real tool line"
        );
        let st = AiReviewState {
            phase: AiPhase::Running,
            timeline: vec![
                TimelineEntry::Agent {
                    agent: agent.clone(),
                    group: Some("notifications".into()),
                    tools: vec![
                        ToolLine {
                            name: "bash".into(),
                            summary: long_json,
                            note: None,
                        },
                        ToolLine {
                            name: "read_file".into(),
                            summary: "{\"path\":\"src/ui/widgets/merge_panel.rs\"}".into(),
                            note: None,
                        },
                    ],
                    text: String::new(),
                    stop: None,
                },
                TimelineEntry::Approval {
                    id: 3,
                    agent,
                    kind: crate::ai::ui_interaction::ApprovalKind::Command {
                        command: "git -C /Users/sercand/Developer/otsimo/web log --format=%H%n%an%n%s --since=2026-10-01 -- src/lib/data-tools/push-notification-default-links.ts".into(),
                        reason: "it runs code".into(),
                    },
                    decided: None,
                },
            ],
            ..AiReviewState::default()
        };

        let panel = Rc::new(RefCell::new(egui::Rect::NOTHING));
        let content = Rc::new(RefCell::new(egui::Rect::NOTHING));
        let clip = Rc::new(RefCell::new(egui::Rect::NOTHING));
        let (p, c, k) = (panel.clone(), content.clone(), clip.clone());
        let mut harness = egui_kittest::Harness::builder()
            .with_size([1440.0, 900.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui(move |ui| {
                let resp = egui::Panel::right("pr-detail-right")
                    .resizable(true)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)))
                    .default_size(420.0)
                    .show(ui, |ui| {
                        ui.label("Files Overview Conversation Commits Checks AI");
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .id_salt("metadata-panel")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let _ = render(ui, &st, &PanelInputs::default(), false);
                                *c.borrow_mut() = ui.min_rect();
                                *k.borrow_mut() = ui.clip_rect();
                            });
                    });
                *p.borrow_mut() = resp.response.rect;
            });
        // The growth compounds every frame: it grew the pane 36 pt per
        // frame before the fix, so give it ten frames to show itself.
        harness.run_steps(10);

        let panel = *panel.borrow();
        assert!(
            panel.width() <= 421.0,
            "the pane grew to {:.0} pt past its 420 pt splitter width",
            panel.width()
        );
        let (content, clip) = (*content.borrow(), *clip.borrow());
        assert!(
            content.min.x >= clip.min.x - 1.0 && content.max.x <= clip.max.x + 1.0,
            "tab content outside the visible pane: content {content:?} vs clip {clip:?}"
        );
        // The approval command must be fully visible, not truncated: its
        // tail — where the risk lives — is laid out in the card, not hidden
        // behind a tooltip. A truncated galley ends in "…" and its label
        // would not contain the command's last token.
        // The approval command must be fully laid out, not truncated. The
        // Label node always carries the full string, so check the laid-out
        // text runs instead: joined they must contain the command's last
        // token (wrapping only breaks at spaces/hyphens, which the token's
        // characters survive contiguously), while a truncated galley ends
        // in "…" and loses it.
        let laid_out: String = harness
            .query_all_by(|n| n.role() == egui::accesskit::Role::TextRun)
            .filter_map(|n| n.value())
            .collect();
        assert!(
            laid_out.contains("src/lib/data-tools/push-notification-default-links.ts"),
            "the approval command was not laid out in full: {laid_out:?}"
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

    /// A summary-anchored draft is kept or dropped from the tab's findings
    /// list: the diff has no row to anchor it to, so this is its only
    /// Keep / Drop surface (spec §5.6).
    #[test]
    fn summary_drafts_report_keep_and_drop() {
        use crate::ai::review::{Finding, FindingAnchor, FindingState, Severity};
        use egui_kittest::kittest::Queryable;

        let state = || AiReviewState {
            phase: AiPhase::Done,
            findings: std::collections::BTreeMap::from([(
                7,
                Finding {
                    id: 7,
                    run_id: 9,
                    agent: AgentRef {
                        id: 1,
                        role: AgentRole::Reviewer,
                        label: "reviewer:g1".to_string(),
                    },
                    anchor: FindingAnchor::Summary {
                        path: "assets/logo.png".to_string(),
                    },
                    severity: Severity::Risk,
                    title: "the asset lost its license header".to_string(),
                    body: "body".to_string(),
                    evidence: vec![],
                    checklist_item: None,
                    state: FindingState::Draft,
                    state_reason: None,
                    pending_local_id: None,
                },
            )]),
            ..AiReviewState::default()
        };

        let mut harness = panel_harness(state());
        harness.get_by_label("Keep").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiPanelAction::KeepSummary(7)],
            "Keep routes to the same keep_draft path the inline row uses"
        );

        let mut harness = panel_harness(state());
        harness.get_by_label("Drop").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiPanelAction::DropSummary(7)],
            "Drop marks the summary item dropped"
        );
    }
}
