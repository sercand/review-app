//! The ✦ AI tab (spec §12): what this PR's AI run is doing, top to bottom.
//!
//! The tab is a pure view over [`AiReviewState`] (Task 2) and the read-models
//! in [`crate::state::ai_activity`]: a run header (status, PR, run controls,
//! the stage stepper, the one-line checkout, the live auto-approve chips and
//! their popover), the asks that need the user, one row per agent with what
//! it is doing now (expandable into its tool timeline), the log of decided
//! commands with its filter chips, the findings so far, and the chat, with
//! the composer and token usage pinned at the bottom. Like every widget here
//! it never mutates state: every control returns an [`AiPanelAction`] for
//! `app.rs` to turn into a worker [`crate::runtime::Command`]. What is
//! expanded and the log filter are UI-local and live in egui memory
//! ([`PanelView`]). Unix-only with [`crate::ai`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, Frame, Margin, RichText};

use crate::ai::events::StageKind;
use crate::ai::interaction::ApprovalDecision;
use crate::ai::repo_link::{FolderStatus, folder_status, start_blockers};
use crate::ai::store::CheckoutRecord;
use crate::ai::tools::AgentRef;
use crate::ai::ui_interaction::ApprovalKind;
use crate::persist::settings::{
    AiSettings, ApprovalLevel, ApprovalMode, AutoApproveSettings, RepoAiSettings,
};
use crate::state::ai_activity::{
    self, AgentStatus, CommandRow, DecisionCounts, LogFilter, Step, StepState,
};
use crate::state::ai_review::{
    AiPhase, AiReviewState, ClassifierNote, CommandDecision, TimelineEntry, change_blocked,
    classifier_note_text, unrestored_for_repo,
};
use crate::state::classifier::{self, ClassifierState};
use crate::ui::components::{self, Tone};
use crate::ui::widgets::{markdown, settings_ai_tab};
use crate::ui::{fonts, icons, theme};

/// What a control in the tab asked of the app (spec §12).
#[derive(Clone, Debug, PartialEq)]
pub enum AiPanelAction {
    /// Keep (`keep: true`) or drop every draft finding of `severity`.
    Triage {
        severity: crate::ai::review::Severity,
        keep: bool,
    },
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
    /// Open Settings → AI's auto-approve dialog from the tab's popover:
    /// its settings apply to running reviews from their next command.
    OpenAutoApprove,
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
    /// Full trust (`[ai.auto_approve].mode = "trust"`) is on: the tab warns
    /// that every command will run without asking.
    pub trust_all: bool,
}

/// The current `[ai.auto_approve]` settings as the tab's chips read them.
/// Settings apply live: a running review picks a change up with its next
/// command, so these are never a per-run snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct AutoApproveView {
    /// What a command nothing vouched for gets.
    pub mode: ApprovalMode,
    /// Whether the local classifier is switched on.
    pub classifier_on: bool,
    /// Where the classifier stands (Settings' latest state).
    pub classifier: ClassifierState,
}

impl AutoApproveView {
    /// The view of `aa` with the classifier at `classifier`.
    pub fn new(aa: &AutoApproveSettings, classifier: &ClassifierState) -> AutoApproveView {
        AutoApproveView {
            mode: aa.mode,
            classifier_on: aa.local_classifier,
            classifier: classifier.clone(),
        }
    }
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
    /// The header's PR line ([`run_title`]); `None` shows none.
    pub title: Option<&'a str>,
    /// The auto-approve chips; `None` hides them.
    pub auto_approve: Option<&'a AutoApproveView>,
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
        trust_all: ai.auto_approve.trust_all(),
    }
}

/// The header's PR line: `owner/name #number`, with the file count once
/// files have loaded.
pub fn run_title(owner: &str, name: &str, number: u64, files: usize) -> String {
    match files {
        0 => format!("{owner}/{name} #{number}"),
        1 => format!("{owner}/{name} #{number} · 1 file"),
        n => format!("{owner}/{name} #{number} · {n} files"),
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

/// A stepper column's caption and detail line: `"Review"` over
/// `"1/3 · 2 running"`, a check before a finished stage's name.
pub fn step_caption(step: &Step) -> (String, String) {
    let name = stage_name(step.stage);
    let (finished, started) = step.agents;
    let total = step_total(step);
    match &step.state {
        StepState::Done => (format!("{} {name}", icons::CHECK), "done".to_string()),
        StepState::Failed(_) => (format!("{} {name}", icons::X), "failed".to_string()),
        StepState::Active if total > 0 => {
            let running = started - finished;
            let detail = if running > 0 {
                format!("{finished}/{total} · {running} running")
            } else {
                format!("{finished}/{total}")
            };
            (name.to_string(), detail)
        }
        StepState::Active => (name.to_string(), "running".to_string()),
        StepState::Skipped => (name.to_string(), "skipped".to_string()),
        StepState::Pending => (name.to_string(), String::new()),
    }
}

/// How much of a stepper column's bar is filled, `0.0..=1.0`.
/// The agents a stage will run in all: what it announced, or more if it
/// started more since (a group run again).
fn step_total(step: &Step) -> u32 {
    step.planned.max(step.agents.1)
}

pub fn step_fill(step: &Step) -> f32 {
    let (finished, _) = step.agents;
    let total = step_total(step);
    match step.state {
        StepState::Done | StepState::Failed(_) => 1.0,
        StepState::Active if total > 0 => (finished as f32 / total as f32).max(0.15),
        StepState::Active => 0.5,
        StepState::Pending | StepState::Skipped => 0.0,
    }
}

/// The tone of a stepper column.
fn step_tone(step: &Step) -> Tone {
    match step.state {
        StepState::Done => Tone::Ok,
        StepState::Failed(_) => Tone::Bad,
        StepState::Active => Tone::Accent,
        StepState::Pending | StepState::Skipped => Tone::Neutral,
    }
}

/// The header's status pill for a phase.
pub fn phase_pill(phase: &AiPhase) -> (&'static str, Tone) {
    match phase {
        AiPhase::Idle => ("Not started", Tone::Neutral),
        AiPhase::Planning => ("Planning", Tone::Pending),
        AiPhase::CheckingOut => ("Checking out", Tone::Accent),
        AiPhase::Running => ("Reviewing", Tone::Accent),
        AiPhase::Paused => ("Paused", Tone::Pending),
        AiPhase::Done => ("Finished", Tone::Ok),
        AiPhase::Restoring => ("Restoring", Tone::Pending),
        AiPhase::Finished => ("Restored", Tone::Neutral),
        AiPhase::Failed(_) => ("Stopped", Tone::Bad),
    }
}

/// The phase's one-line explanation under the header, when it needs one.
fn phase_line(phase: &AiPhase) -> Option<String> {
    match phase {
        AiPhase::Idle => Some("No AI review yet.".to_string()),
        AiPhase::Planning => {
            Some("The checkout is planned — confirm it to start the review.".to_string())
        }
        AiPhase::CheckingOut => Some("Checking out the PR…".to_string()),
        AiPhase::Restoring => Some("Restoring the checkout…".to_string()),
        AiPhase::Finished => Some("The checkout was restored.".to_string()),
        AiPhase::Failed(message) => Some(format!("The review stopped: {message}")),
        AiPhase::Running | AiPhase::Paused | AiPhase::Done => None,
    }
}

impl AutoApproveView {
    /// The slider level these settings are at.
    pub fn level(&self) -> ApprovalLevel {
        ApprovalLevel::of(self.mode, self.classifier_on)
    }
}

/// The level chip: "Level 3 · Never ask", warmer the less it checks.
pub fn level_chip(view: &AutoApproveView) -> (String, Tone) {
    let level = view.level();
    let tone = match level {
        ApprovalLevel::AskMe => Tone::Neutral,
        ApprovalLevel::AskWhenUnsure => Tone::Accent,
        ApprovalLevel::NeverAsk => Tone::Ok,
        ApprovalLevel::AskOnDanger => Tone::Warn,
        ApprovalLevel::Trust => Tone::Bad,
    };
    (
        format!(
            "Level {} · {}",
            level.number(),
            settings_ai_tab::level_name(level)
        ),
        tone,
    )
}

/// The classifier chip, shown only while a level that uses the classifier
/// has none ready. `compiled` is whether this build has one.
pub fn classifier_chip(view: &AutoApproveView, compiled: bool) -> Option<(String, Tone)> {
    if !view.level().uses_classifier() {
        return None;
    }
    if !compiled {
        return Some(("Classifier: not in this build".to_string(), Tone::Warn));
    }
    match view.classifier {
        ClassifierState::Ready(_) => None,
        ClassifierState::Off | ClassifierState::Loading => {
            Some(("Classifier: loading".to_string(), Tone::Warn))
        }
        ClassifierState::Downloading { .. } => {
            Some(("Classifier: downloading".to_string(), Tone::Warn))
        }
        ClassifierState::Unavailable(_) | ClassifierState::Failed(_) => {
            Some(("Classifier: unavailable".to_string(), Tone::Bad))
        }
    }
}

/// A decided command's mark: an icon, a short name for it (its tooltip
/// heading and accessible name) and its tone. Green for the built-in
/// checks, purple for the classifier, accent for the user's own calls, red
/// for full trust and refusals.
pub fn decision_badge(decision: &CommandDecision) -> (char, &'static str, Tone) {
    match decision {
        CommandDecision::Rules { .. } => (icons::CIRCLE_CHECK, "safe", Tone::Ok),
        CommandDecision::Classifier => (icons::SPARKLES, "classifier", Tone::Info),
        CommandDecision::Trust => (icons::CIRCLE_DOT, "full trust", Tone::Bad),
        CommandDecision::Remembered | CommandDecision::User { .. } => {
            (icons::USER_CHECK, "you allowed it", Tone::Accent)
        }
        CommandDecision::UserDenied => (icons::X, "you declined it", Tone::Warn),
        CommandDecision::Blocked { .. } => (icons::EYE_OFF, "blocked", Tone::Bad),
        CommandDecision::AutoDenied { .. } => (icons::CIRCLE_X, "refused", Tone::Bad),
    }
}

/// Paint `decision`'s mark with `reason` on hover.
fn decision_mark(ui: &mut egui::Ui, decision: &CommandDecision, reason: &str) -> egui::Response {
    let p = theme::palette_of(ui);
    let (icon, name, tone) = decision_badge(decision);
    let (fg, _) = components::tone_colors(p, tone);
    let response = ui
        .add_sized(
            [18.0, 18.0],
            egui::Label::new(RichText::new(icon.to_string()).color(fg).size(13.0)),
        )
        .on_hover_text(format!("{}: {reason}", capitalize(name)));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, name));
    response
}

/// The full sentence behind a [`decision_badge`], with the classifier's
/// verdict when it was asked: the log row's tooltip and expanded text.
pub fn decision_reason(decision: &CommandDecision, note: Option<ClassifierNote>) -> String {
    let why = match decision {
        CommandDecision::Rules { source } => {
            format!("Ran: a built-in check found it safe ({source}).")
        }
        CommandDecision::Classifier => "Ran: the local classifier said safe.".to_string(),
        CommandDecision::Trust => "Ran: full trust is on, nothing was checked.".to_string(),
        CommandDecision::Remembered => {
            "Ran: you allowed this exact command for this review earlier.".to_string()
        }
        CommandDecision::User { for_review: false } => "Ran: you allowed it once.".to_string(),
        CommandDecision::User { for_review: true } => {
            "Ran: you allowed it for this review.".to_string()
        }
        CommandDecision::UserDenied => "Did not run: you declined it.".to_string(),
        CommandDecision::Blocked { reason } => format!("Blocked: {reason}"),
        CommandDecision::AutoDenied { reason } => {
            format!("Denied, the agent was told why: {reason}")
        }
    };
    match note {
        Some(note) => format!("{why} {}.", capitalize(classifier_note_text(note))),
        None => why,
    }
}

/// `text` with its first letter upper-cased.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The collapsible header for one timeline entry, e.g.
/// `"Reviewer · notifications"`.
pub fn entry_title(entry: &TimelineEntry) -> String {
    match entry {
        TimelineEntry::Stage { stage, .. } => stage_name(*stage).to_string(),
        TimelineEntry::Agent { agent, group, .. } => {
            ai_activity::agent_title(agent, group.as_deref())
        }
        TimelineEntry::Question { agent, .. } => {
            format!("{} asks", ai_activity::role_name(agent.role))
        }
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

/// How many command-log rows show before "Show N more".
pub const LOG_PAGE: usize = 8;

/// How many chat entries show before "Earlier messages".
pub const CHAT_RECENT: usize = 6;

/// The rows the log shows under `filter`: the matching rows, the first
/// [`LOG_PAGE`] unless `all`, and how many more there are.
pub fn visible_log(rows: &[CommandRow], filter: LogFilter, all: bool) -> (Vec<&CommandRow>, usize) {
    let matching: Vec<&CommandRow> = rows
        .iter()
        .filter(|row| filter.matches(&row.decision))
        .collect();
    if all || matching.len() <= LOG_PAGE {
        return (matching, 0);
    }
    let more = matching.len() - LOG_PAGE;
    (matching.into_iter().take(LOG_PAGE).collect(), more)
}

/// What is expanded in the tab and how the log is filtered. UI-local: kept
/// in egui memory per PR, never in [`AiReviewState`], and lost on restart.
#[derive(Clone, Debug, Default)]
struct PanelView {
    /// The checkout line's details; `None` follows the default (open while
    /// no folder is linked or the last choice failed).
    checkout_open: Option<bool>,
    /// The auto-approve popover.
    policy_open: bool,
    /// Expanded agent rows, by timeline index.
    agents_open: BTreeSet<usize>,
    /// Expanded tool lines inside agent rows, `(entry, tool)`.
    tools_open: BTreeSet<(usize, usize)>,
    /// Agents whose whole streamed text shows, by timeline index.
    text_full: BTreeSet<usize>,
    /// The command log's filter chip.
    filter: LogFilter,
    /// The log shows every row, not just the first page.
    log_all: bool,
    /// Expanded log rows.
    log_open: BTreeSet<(usize, Option<usize>)>,
    /// Approval cards showing their whole command, by timeline index.
    asks_full: BTreeSet<usize>,
    /// The chat shows its earlier messages.
    chat_earlier: bool,
    /// Sections the user opened or closed by their heading; a section not
    /// here follows [`section_default_open`].
    sections: std::collections::BTreeMap<&'static str, bool>,
}

/// Sections longer than this start collapsed: a review with hundreds of
/// commands or findings must not bury the rest of the tab.
const SECTION_COLLAPSE_AT: usize = 12;

/// Whether a section with `count` rows starts open.
fn section_default_open(count: usize) -> bool {
    count <= SECTION_COLLAPSE_AT
}

/// A section heading the user can click to fold the section: chevron,
/// `title · count`. Returns whether the section is open.
fn section(
    ui: &mut egui::Ui,
    view: &mut PanelView,
    key: &'static str,
    title: &str,
    count: usize,
) -> bool {
    let p = theme::palette_of(ui);
    let open = view
        .sections
        .get(key)
        .copied()
        .unwrap_or_else(|| section_default_open(count));
    ui.add_space(6.0);
    let chevron = if open {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    let response = ui
        .add(
            egui::Button::new(
                RichText::new(format!("{chevron} {title} · {count}"))
                    .font(fonts::semibold(11.5))
                    .color(p.muted),
            )
            .frame(false),
        )
        .on_hover_text(if open { "Collapse" } else { "Expand" });
    if response.clicked() {
        view.sections.insert(key, !open);
    }
    open != response.clicked()
}

/// Paint the tab; `inputs` carries the window-level rows (checkout, repo
/// settings, PR title, auto-approve), `show_dropped` reveals the
/// verifier-dropped findings. The composer and the token usage are pinned
/// at the bottom; everything else scrolls.
pub fn render(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    inputs: &PanelInputs,
    show_dropped: bool,
) -> Vec<AiPanelAction> {
    let mut actions = Vec::new();
    let view_id = egui::Id::unique(("ai-panel-view", inputs.title.unwrap_or("")));
    let mut view: PanelView = ui.data(|d| d.get_temp(view_id)).unwrap_or_default();
    // Bottom-up: the composer claims its rows first and the scroll area
    // takes what is left. Not an `egui::Panel`: a panel keeps the largest
    // size it ever had, so one tall frame starved the scroll area for good.
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        ui.add_space(6.0);
        render_composer(ui, st, &mut actions);
        render_usage(ui, st);
        ui.add_space(6.0);
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            render_scrolled(ui, st, inputs, show_dropped, &mut view, &mut actions);
        });
    });
    ui.data_mut(|d| d.insert_temp(view_id, view));
    actions
}

/// Everything above the composer, in one scroll area.
fn render_scrolled(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    inputs: &PanelInputs,
    show_dropped: bool,
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    egui::ScrollArea::vertical()
        .id_salt("ai-panel-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let log = ai_activity::command_log(st);
            render_header(ui, st, inputs, view, actions);
            render_auto_approve(ui, st, inputs.auto_approve, &log, view, actions);
            render_quick_triage(ui, st, view, actions);
            render_notices(ui, st);
            render_suggestions(ui, st, actions);
            render_needs_you(ui, st, view, actions);
            render_agents(ui, st, view);
            render_commands(ui, &log, view);
            render_findings_section(ui, st, show_dropped, view, actions);
            render_chat_log(ui, st, view);
        });
}

/// A section heading: small caps-like muted text, `color` for "Needs you".
fn heading(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).font(fonts::semibold(11.5)).color(color));
}

/// A [`components::pill`]-styled badge the user can click, announced to
/// assistive tech (and tests) as a button named `text`.
fn chip_button(ui: &mut egui::Ui, text: &str, tone: Tone) -> egui::Response {
    let response = components::pill(ui, None, text, tone);
    let response = ui
        .interact(
            response.rect,
            response.id.with("click"),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), text));
    response
}

/// The run header: status pill, PR line, run controls, the phase line, the
/// Start / Continue entry points with their blockers, the stage stepper and
/// the one-line checkout.
fn render_header(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    inputs: &PanelInputs,
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    ui.horizontal(|ui| {
        let (text, tone) = phase_pill(&st.phase);
        components::pill(ui, None, text, tone);
        if matches!(
            st.phase,
            AiPhase::Running | AiPhase::CheckingOut | AiPhase::Planning | AiPhase::Restoring
        ) {
            ui.spinner();
        }
        if let Some(title) = inputs.title {
            ui.add(
                egui::Label::new(
                    RichText::new(title)
                        .font(fonts::semibold(12.5))
                        .color(p.text),
                )
                .truncate(),
            )
            .on_hover_text(title);
        }
    });
    if let Some(line) = phase_line(&st.phase) {
        let color = match st.phase {
            AiPhase::Failed(_) => p.bad,
            _ => p.muted,
        };
        ui.colored_label(color, line);
    }
    render_controls(ui, st, inputs.checkout, actions);
    if st
        .timeline
        .iter()
        .any(|entry| matches!(entry, TimelineEntry::Stage { .. }))
    {
        render_stepper(ui, &ai_activity::stepper(st));
    }
    if let Some(vm) = inputs.checkout {
        render_checkout_line(ui, vm, inputs.repo, view, actions);
    }
}

/// Pause / Resume · Finish · Cancel (spec §12), and Start / Continue with
/// the reasons Start is disabled stated under them (AI settings Task 7).
fn render_controls(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    checkout: Option<&LocalCheckoutVm>,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    let can_start = matches!(
        st.phase,
        AiPhase::Idle | AiPhase::Finished | AiPhase::Failed(_)
    );
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
        // PR head, which the tab cannot see; `app.rs` gates the click on
        // `can_continue` (Task 4). The same blockers stop it too.
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
        // A plan waiting for confirmation can be abandoned from here too:
        // a confirm dialog that never reached the screen must not strand a
        // spinner with no way out (Task 4 review findings 1 and 3).
        if matches!(
            st.phase,
            AiPhase::Running | AiPhase::Paused | AiPhase::Planning
        ) && ui.button(RichText::new("Cancel").color(p.bad)).clicked()
        {
            actions.push(AiPanelAction::Cancel);
        }
    });
    if can_start {
        for reason in blockers {
            ui.colored_label(p.pending, reason);
        }
    }
}

/// The stage stepper: one column per stage with a progress bar, its name
/// (agent progress for Review) and a detail line.
fn render_stepper(ui: &mut egui::Ui, steps: &[Step]) {
    let p = theme::palette_of(ui);
    ui.columns(steps.len(), |columns| {
        for (column, step) in columns.iter_mut().zip(steps) {
            let (fg, _) = components::tone_colors(p, step_tone(step));
            let width = column.available_width();
            let (rect, _) =
                column.allocate_exact_size(egui::vec2(width, 4.0), egui::Sense::hover());
            column.painter().rect_filled(rect, 2.0, p.line);
            let fill = step_fill(step);
            if fill > 0.0 {
                let mut done = rect;
                done.set_width(rect.width() * fill);
                column.painter().rect_filled(done, 2.0, fg);
            }
            let (caption, detail) = step_caption(step);
            let color = if step.state == StepState::Pending || step.state == StepState::Skipped {
                p.muted
            } else {
                fg
            };
            column.add(
                egui::Label::new(
                    RichText::new(caption)
                        .font(fonts::semibold(11.5))
                        .color(color),
                )
                .truncate(),
            );
            if !detail.is_empty() {
                // Wraps rather than cuts: "1/3 · 2 running" is the line
                // that says how far the review is.
                column.add(
                    egui::Label::new(RichText::new(detail).size(11.0).color(p.muted))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            }
        }
    });
}

/// The Local checkout as one line (spec: AI settings — "PR → AI tab: Local
/// checkout row"): the folder, the clone check and a link that opens the
/// row's controls (Choose… / Change… / Unlink, the probe's complaint, the
/// repo's commands with their edit affordance). An actionable blocker
/// (Restore…) always shows.
fn render_checkout_line(
    ui: &mut egui::Ui,
    vm: &LocalCheckoutVm,
    repo: Option<&RepoAiSettings>,
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    let open = view
        .checkout_open
        .unwrap_or(vm.path.is_none() || vm.error.is_some());
    ui.horizontal(|ui| {
        ui.label(RichText::new("Checkout").color(p.muted).size(12.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let link = if open { "Hide" } else { "Commands & folder…" };
            if ui.link(link).clicked() {
                view.checkout_open = Some(!open);
            }
            match (&vm.path, vm.status) {
                (None, _) => {
                    ui.colored_label(p.pending, "no folder");
                }
                (Some(_), FolderStatus::Ok) => {
                    ui.colored_label(p.ok, format!("{} clone", icons::CHECK));
                }
                (Some(_), FolderStatus::Missing) => {
                    ui.colored_label(p.bad, "missing");
                }
                (Some(_), FolderStatus::NotARepo) => {
                    ui.colored_label(p.bad, "not a repo");
                }
            }
            if let Some(path) = &vm.path {
                let shown = path.display().to_string();
                ui.add(
                    egui::Label::new(RichText::new(&shown).monospace().size(11.5).color(p.text))
                        .truncate(),
                )
                .on_hover_text(shown);
            }
        });
    });
    if !vm.restore.is_empty()
        && let Some(reason) = &vm.change_blocked
        && !open
    {
        blocked_line(ui, vm, reason, actions);
    }
    if open {
        card(ui, |ui| render_checkout_details(ui, vm, repo, actions));
    }
}

/// The change-blocked reason with Restore… when the blocker is an
/// unrestored checkout the user can act on.
fn blocked_line(
    ui: &mut egui::Ui,
    vm: &LocalCheckoutVm,
    reason: &str,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    ui.horizontal_wrapped(|ui| {
        ui.colored_label(p.pending, reason);
        // The recovery dialog dismissed with "Later", reopened from here.
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

/// The checkout line's details: the folder with Change… / Unlink (or
/// Choose folder…), the change blocker, the probe error and the commands.
fn render_checkout_details(
    ui: &mut egui::Ui,
    vm: &LocalCheckoutVm,
    repo: Option<&RepoAiSettings>,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    ui.label(RichText::new(format!("Local checkout for {}", vm.repo)).font(fonts::semibold(12.5)));
    let blocked = vm.change_blocked.is_some();
    match &vm.path {
        Some(path) => {
            ui.horizontal_wrapped(|ui| {
                let shown = path.display().to_string();
                ui.add(egui::Label::new(RichText::new(&shown).monospace().size(11.5)).truncate())
                    .on_hover_text(shown);
                match vm.status {
                    FolderStatus::Ok => {
                        ui.colored_label(p.ok, format!("{} clone of {}", icons::CHECK, vm.repo));
                    }
                    FolderStatus::Missing => {
                        ui.colored_label(p.bad, "folder is missing");
                    }
                    FolderStatus::NotARepo => {
                        ui.colored_label(p.bad, "not a git repository");
                    }
                }
                ui.add_enabled_ui(!blocked, |ui| {
                    if ui.button("Change…").clicked() {
                        actions.push(AiPanelAction::ChooseFolder);
                    }
                    if ui.button("Unlink").clicked() {
                        actions.push(AiPanelAction::UnlinkFolder);
                    }
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
        blocked_line(ui, vm, reason, actions);
    }
    if let Some(error) = &vm.error {
        ui.colored_label(p.bad, error);
    }
    render_commands_summary(ui, repo, actions);
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
                    RichText::new("No repo commands yet.")
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

/// The auto-approve chips over the current settings, the full-trust
/// warning, and the popover the chips open.
fn render_auto_approve(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    aa: Option<&AutoApproveView>,
    log: &[CommandRow],
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    let Some(aa) = aa else {
        return;
    };
    let p = theme::palette_of(ui);
    let response = ui
        .horizontal_wrapped(|ui| {
            ui.label(RichText::new("Auto-approve").color(p.muted).size(12.0));
            let (level, tone) = level_chip(aa);
            let first = chip_button(ui, &level, tone);
            let mut clicked = first.clicked();
            if let Some((cls, cls_tone)) = classifier_chip(aa, classifier::COMPILED) {
                clicked |= chip_button(ui, &cls, cls_tone).clicked();
            }
            if clicked {
                view.policy_open = !view.policy_open;
            }
            first
        })
        .inner;
    egui::Popup::from_response(&response)
        .id(egui::Id::unique("ai-auto-approve-popover"))
        .open_bool(&mut view.policy_open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| render_policy_popover(ui, aa, log, actions));
    if aa.mode == ApprovalMode::Trust {
        ui.colored_label(
            p.bad,
            RichText::new("Full trust is on: every command runs without asking.").strong(),
        );
        if matches!(st.phase, AiPhase::Running | AiPhase::Paused) {
            ui.label(RichText::new(TRUST_SANDBOX_NOTE).color(p.muted).size(11.5));
        }
    }
}

/// Why full trust does not reveal secrets to a review already running: the
/// sandbox's read denies are fixed when the review starts.
const TRUST_SANDBOX_NOTE: &str = "A review started before full trust was turned on keeps its sandbox: secret files stay hidden until the next review.";

/// The auto-approve popover: the current settings, who settled this run's
/// commands so far, and the way to the settings dialog.
fn render_policy_popover(
    ui: &mut egui::Ui,
    aa: &AutoApproveView,
    log: &[CommandRow],
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    ui.set_width(320.0);
    let level = aa.level();
    let (title, tone) = level_chip(aa);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Auto-approve").font(fonts::semibold(13.0)));
        components::pill(ui, None, &title, tone);
    });
    ui.label(
        RichText::new(settings_ai_tab::level_summary(level))
            .color(p.muted)
            .size(11.5),
    );
    if level.uses_classifier() {
        ui.label(
            RichText::new(format!(
                "Classifier: {}",
                aa.classifier.text(classifier::COMPILED)
            ))
            .color(p.muted)
            .size(11.5),
        );
    }
    if level == ApprovalLevel::Trust {
        ui.label(RichText::new(TRUST_SANDBOX_NOTE).color(p.muted).size(11.5));
    }
    ui.label(
        RichText::new("Changes apply to running reviews from their next command.")
            .color(p.muted)
            .size(11.5),
    );
    let counts = ai_activity::decision_counts(log);
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!(
            "So far in this review · {} commands",
            counts.total()
        ))
        .color(p.muted)
        .size(11.5),
    );
    counts_bar(ui, &counts);
    ui.add_space(4.0);
    if ui.button("Auto-approve settings…").clicked() {
        actions.push(AiPanelAction::OpenAutoApprove);
    }
}

/// The popover's segments, in bar order: label, count, colour.
fn count_segments(
    p: &theme::Palette,
    counts: &DecisionCounts,
) -> [(&'static str, usize, Color32); 6] {
    [
        ("safe", counts.rules, p.ok),
        ("classifier", counts.classifier, p.info),
        ("you", counts.you, p.accent),
        ("full trust", counts.trust, p.warn),
        ("blocked", counts.blocked, p.bad),
        ("denied", counts.denied, p.pending),
    ]
}

/// The stacked bar and its legend (`rules N · classifier N · you N ·
/// blocked N · denied N`; full trust only when it ran anything).
fn counts_bar(ui: &mut egui::Ui, counts: &DecisionCounts) {
    let p = theme::palette_of(ui);
    let segments = count_segments(p, counts);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 8.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, p.line);
    let total = counts.total();
    if total > 0 {
        let mut x = rect.left();
        for (_, n, color) in segments {
            if n == 0 {
                continue;
            }
            let w = rect.width() * n as f32 / total as f32;
            let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, 8.0));
            ui.painter().rect_filled(seg, 0.0, color);
            x += w;
        }
    }
    ui.horizontal_wrapped(|ui| {
        for (label, n, color) in segments {
            if label == "full trust" && n == 0 {
                continue;
            }
            let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(dot, 2.0, color);
            ui.label(RichText::new(format!("{label} {n}")).size(11.5));
        }
    });
}

/// The app's notices (a cancelled run, the unsandboxed shell, a stash
/// conflict), near the header so they stay seen.
/// The draft findings per severity, in severity order, with their count;
/// severities with no drafts are left out.
pub fn draft_counts(st: &AiReviewState) -> Vec<(crate::ai::review::Severity, usize)> {
    use crate::ai::review::{FindingState, Severity};

    [
        Severity::Bug,
        Severity::Risk,
        Severity::Nit,
        Severity::Question,
    ]
    .into_iter()
    .map(|severity| {
        let n = st
            .findings
            .values()
            .filter(|f| f.state == FindingState::Draft && f.severity == severity)
            .count();
        (severity, n)
    })
    .filter(|(_, n)| *n > 0)
    .collect()
}

/// Quick triage: one row per severity with drafts, Keep all / Drop all, so
/// a review with a hundred findings is settled in a few clicks. The chat's
/// `triage_findings` does the same for anything finer.
fn render_quick_triage(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    let counts = draft_counts(st);
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    // The shortcut through a long list: open unless the user folded it.
    view.sections.entry("triage").or_insert(true);
    if total == 0 || !section(ui, view, "triage", "DRAFTS TO TRIAGE", total) {
        return;
    }
    let p = theme::palette_of(ui);
    egui::Grid::new("ai-quick-triage")
        .num_columns(3)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            for (severity, n) in counts {
                ui.label(
                    RichText::new(format!("{} · {n}", severity.label()))
                        .font(fonts::semibold(12.0))
                        .color(severity_color(p, severity)),
                );
                let plural = severity.label().to_lowercase();
                if ui
                    .button(format!("Keep all {n}"))
                    .on_hover_text(format!("Add every {plural} draft to your review"))
                    .clicked()
                {
                    actions.push(AiPanelAction::Triage {
                        severity,
                        keep: true,
                    });
                }
                if ui
                    .button(format!("Drop all {n}"))
                    .on_hover_text(format!("Dismiss every {plural} draft"))
                    .clicked()
                {
                    actions.push(AiPanelAction::Triage {
                        severity,
                        keep: false,
                    });
                }
                ui.end_row();
            }
        });
    ui.label(
        RichText::new(
            "For finer picks, ask the chat (\"keep the bugs in stream.rs, drop the rest\").",
        )
        .size(11.0)
        .color(p.faint),
    );
}

fn render_notices(ui: &mut egui::Ui, st: &AiReviewState) {
    let p = theme::palette_of(ui);
    for entry in &st.timeline {
        if let TimelineEntry::Notice { text } = entry {
            ui.colored_label(p.pending, text);
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

/// The asking agent's title, with its group when its entry knows one.
fn asker_title(st: &AiReviewState, asker: &AgentRef) -> String {
    let group = st.timeline.iter().find_map(|entry| match entry {
        TimelineEntry::Agent { agent, group, .. } if agent.label == asker.label => group.clone(),
        _ => None,
    });
    ai_activity::agent_title(asker, group.as_deref())
}

/// "Needs you": the undecided approvals and unanswered questions, compact,
/// at the top. Decided approvals are not here: their commands are in the
/// log.
fn render_needs_you(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    let asks = ai_activity::open_asks(st);
    if asks.is_empty() {
        return;
    }
    let p = theme::palette_of(ui);
    heading(ui, &format!("NEEDS YOU · {}", asks.len()), p.pending);
    for index in asks {
        let entry = &st.timeline[index];
        ask_card(ui, |ui| match entry {
            TimelineEntry::Approval {
                id, agent, kind, ..
            } => {
                approval_card(ui, st, index, *id, agent, kind, view, actions);
            }
            TimelineEntry::Question {
                id,
                agent,
                text,
                options,
                ..
            } => question_card(ui, st, *id, agent, text, options, actions),
            _ => {}
        });
    }
}

/// One undecided approval: who asks, the command cut to two lines with
/// "Full command", the reason, and the decision buttons.
#[allow(clippy::too_many_arguments)]
fn approval_card(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    index: usize,
    id: u64,
    agent: &AgentRef,
    kind: &ApprovalKind,
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(asker_title(st, agent)).font(fonts::semibold(12.5)));
        let what = match kind {
            ApprovalKind::Command { .. } => "wants to run a command",
            ApprovalKind::Prepare { .. } => "wants to run the prepare commands",
        };
        ui.label(RichText::new(what).color(p.muted).size(12.0));
    });
    match kind {
        ApprovalKind::Command { command, reason } => {
            let full = view.asks_full.contains(&index);
            let elided = mono_block(ui, command, if full { None } else { Some(2) });
            if (elided || full)
                && ui
                    .link(if full { "Less" } else { "Full command" })
                    .clicked()
            {
                if full {
                    view.asks_full.remove(&index);
                } else {
                    view.asks_full.insert(index);
                }
            }
            ui.label(RichText::new(reason).color(p.muted).size(11.5));
        }
        ApprovalKind::Prepare { reason, commands } => {
            for command in commands {
                mono_line(ui, command);
            }
            ui.label(RichText::new(reason).color(p.muted).size(11.5));
        }
    }
    ui.horizontal_wrapped(|ui| {
        for (button, label) in approval_buttons(kind).iter().enumerate() {
            if ui.button(*label).clicked() {
                let action = match kind {
                    ApprovalKind::Command { .. } => command_decision(button)
                        .map(|decision| AiPanelAction::Approve { id, decision }),
                    ApprovalKind::Prepare { .. } => Some(AiPanelAction::ApprovePrepare {
                        id,
                        ok: button == 0,
                    }),
                };
                if let Some(action) = action {
                    actions.push(action);
                }
            }
        }
    });
}

/// One unanswered question: the text, its quick replies, and a free-text
/// answer field.
fn question_card(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    id: u64,
    agent: &AgentRef,
    text: &str,
    options: &[String],
    actions: &mut Vec<AiPanelAction>,
) {
    let p = theme::palette_of(ui);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(asker_title(st, agent)).font(fonts::semibold(12.5)));
        ui.label(RichText::new("asks").color(p.muted).size(12.0));
    });
    ui.label(text);
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
    let mut draft: String = ui.data_mut(|d| d.get_persisted(field_id).unwrap_or_default());
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let send = ui.button("Send");
        let field = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .hint_text("Answer…")
                .desired_width(f32::INFINITY),
        );
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (send.clicked() || enter) && !draft.trim().is_empty() {
            answered = Some(draft.trim().to_string());
        }
    });
    ui.data_mut(|d| d.insert_persisted(field_id, draft));
    if let Some(answer) = answered {
        actions.push(AiPanelAction::Answer { id, answer });
    }
}

/// The status glyph and its colour for an agent row.
fn status_glyph(p: &theme::Palette, status: &AgentStatus) -> (char, Color32) {
    match status {
        AgentStatus::Running => (icons::CIRCLE_DOT, p.ai),
        AgentStatus::Waiting => (icons::CLOCK, p.pending),
        AgentStatus::Done => (icons::CIRCLE_CHECK, p.ok),
        AgentStatus::Stopped(_) => (icons::CIRCLE_X, p.bad),
    }
}

/// "Agents": one row per agent with its status, what it is doing now and
/// how many tools it called; a row expands into the agent's timeline.
/// Agent rows in stage order (the order each kind first started), groups
/// by number within a kind: `g2` before `g10` before `g22`, whatever order
/// the scheduler ran them in.
fn sorted_agent_rows(mut rows: Vec<ai_activity::AgentRow>) -> Vec<ai_activity::AgentRow> {
    let kind = |title: &str| title.split(" · ").next().unwrap_or(title).to_string();
    let group = |title: &str| {
        title
            .rsplit(" · g")
            .next()
            .filter(|_| title.contains(" · g"))
            .and_then(|n| n.parse::<u32>().ok())
    };
    let mut kinds: Vec<String> = Vec::new();
    for row in &rows {
        let k = kind(&row.title);
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    rows.sort_by_key(|row| {
        let rank = kinds.iter().position(|k| *k == kind(&row.title));
        (rank, group(&row.title).unwrap_or(u32::MAX))
    });
    rows
}

fn render_agents(ui: &mut egui::Ui, st: &AiReviewState, view: &mut PanelView) {
    let rows = sorted_agent_rows(ai_activity::agent_rows(st));
    if rows.is_empty() {
        return;
    }
    let p = theme::palette_of(ui);
    if !section(ui, view, "agents", "AGENTS", rows.len()) {
        return;
    }
    bordered(ui, |ui| {
        for (n, row) in rows.iter().enumerate() {
            if n > 0 {
                ui.separator();
            }
            let open = view.agents_open.contains(&row.entry);
            let mut toggle = false;
            ui.horizontal(|ui| {
                let (glyph, color) = status_glyph(p, &row.status);
                ui.label(RichText::new(glyph.to_string()).color(color).size(13.0));
                toggle |= ui
                    .add(
                        egui::Button::new(RichText::new(&row.title).font(fonts::semibold(12.5)))
                            .frame(false),
                    )
                    .clicked();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let chevron = if open {
                        icons::CHEVRON_DOWN
                    } else {
                        icons::CHEVRON_RIGHT
                    };
                    toggle |= icons::icon_button(
                        ui,
                        chevron,
                        if open {
                            "Hide the agent's steps"
                        } else {
                            "Show the agent's steps"
                        },
                    )
                    .clicked();
                    ui.label(
                        RichText::new(format!(
                            "{} call{}",
                            row.calls,
                            if row.calls == 1 { "" } else { "s" }
                        ))
                        .color(p.muted)
                        .size(11.5),
                    );
                    let activity_color = match row.status {
                        AgentStatus::Waiting => p.pending,
                        _ => p.muted,
                    };
                    ui.add(
                        egui::Label::new(
                            RichText::new(&row.activity)
                                .color(activity_color)
                                .size(12.0),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&row.activity);
                });
            });
            if toggle {
                if open {
                    view.agents_open.remove(&row.entry);
                } else {
                    view.agents_open.insert(row.entry);
                }
            }
            if open {
                render_agent_detail(ui, st, row.entry, view);
            }
        }
    });
}

/// One agent's timeline: a line per tool call (decided `bash` calls with
/// their badge; a click shows the whole input and why it was decided), its
/// streamed text cut to the last paragraph with "show more", and why its
/// loop ended.
fn render_agent_detail(ui: &mut egui::Ui, st: &AiReviewState, entry: usize, view: &mut PanelView) {
    let Some(TimelineEntry::Agent {
        tools, text, stop, ..
    }) = st.timeline.get(entry)
    else {
        return;
    };
    let p = theme::palette_of(ui);
    Frame::new()
        .inner_margin(Margin {
            left: 22,
            right: 0,
            top: 2,
            bottom: 4,
        })
        .show(ui, |ui| {
            for (index, line) in tools.iter().enumerate() {
                // The scout's finishing call is its whole result: shown
                // as the brief it filed, not as its (truncated) JSON input.
                if line.name == "submit_brief" {
                    render_brief(ui, st.brief.as_ref());
                    continue;
                }
                if render_result_card(ui, &line.name, &line.summary) {
                    continue;
                }
                let key = (entry, index);
                let open = view.tools_open.contains(&key);
                let clicked = ui
                    .horizontal(|ui| {
                        if line.name == "bash" {
                            match &line.decision {
                                Some(decision) => {
                                    decision_mark(
                                        ui,
                                        decision,
                                        &decision_reason(decision, line.note),
                                    );
                                }
                                None => {
                                    components::pill(ui, None, "bash", Tone::Neutral);
                                }
                            }
                        } else {
                            ui.label(RichText::new(&line.name).color(p.muted).size(11.5));
                        }
                        let brief = if line.name == "bash" {
                            ai_activity::command_text(&line.summary)
                        } else {
                            ai_activity::tool_brief(line)
                                .strip_prefix(&line.name)
                                .unwrap_or_default()
                                .trim()
                                .to_string()
                        };
                        ui.add(
                            egui::Button::new(RichText::new(brief).monospace().size(11.5))
                                .frame(false)
                                .truncate(),
                        )
                        .clicked()
                    })
                    .inner;
                if clicked {
                    if open {
                        view.tools_open.remove(&key);
                    } else {
                        view.tools_open.insert(key);
                    }
                }
                if open {
                    let full = if line.name == "bash" {
                        ai_activity::command_text(&line.summary)
                    } else {
                        line.summary.clone()
                    };
                    mono_line(ui, &full);
                    if let Some(decision) = &line.decision {
                        ui.label(
                            RichText::new(decision_reason(decision, line.note))
                                .color(p.muted)
                                .size(11.5),
                        );
                    } else if let Some(note) = line.note {
                        ui.label(
                            RichText::new(classifier_note_text(note))
                                .color(p.muted)
                                .size(11.5),
                        );
                    }
                }
            }
            if !text.trim().is_empty() {
                let full = view.text_full.contains(&entry);
                let last = ai_activity::last_paragraph(text);
                let shown = if full { text.as_str() } else { last };
                markdown::render(ui, &markdown::to_doc(shown));
                if last.len() < text.trim().len()
                    && ui
                        .link(if full { "show less" } else { "show more" })
                        .clicked()
                {
                    if full {
                        view.text_full.remove(&entry);
                    } else {
                        view.text_full.insert(entry);
                    }
                }
            }
            if let Some(stop) = stop {
                ui.label(RichText::new(stop).color(p.faint).size(11.5));
            }
        });
}

/// A card for a tool whose input is the agent's result: `done`'s summary,
/// a verifier's verdicts, the assembler's draft, the prior check's states.
/// Returns `false` (nothing painted) for any other tool, or an input that
/// does not parse (a row cut short before result tools travelled whole).
fn render_result_card(ui: &mut egui::Ui, name: &str, summary: &str) -> bool {
    let Ok(input) = serde_json::from_str::<serde_json::Value>(summary) else {
        return false;
    };
    let p = theme::palette_of(ui);
    let text = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let title = |ui: &mut egui::Ui, icon: char, text: &str| {
        ui.label(
            RichText::new(icons::icon_text(icon, text))
                .font(fonts::semibold(12.5))
                .color(p.text),
        );
    };
    match name {
        "done" => {
            let summary = text(&input, "summary");
            card(ui, |ui| {
                title(ui, icons::CIRCLE_CHECK, "Finished");
                if !summary.trim().is_empty() {
                    markdown::render(ui, &markdown::to_doc(&summary));
                }
            });
        }
        "submit_verdicts" => {
            let Some(verdicts) = input.get("verdicts").and_then(serde_json::Value::as_array) else {
                return false;
            };
            card(ui, |ui| {
                title(
                    ui,
                    icons::CHECK_CHECK,
                    &format!("Verdicts · {}", verdicts.len()),
                );
                for v in verdicts {
                    ui.horizontal_wrapped(|ui| {
                        let id = v.get("finding_id").and_then(serde_json::Value::as_i64);
                        ui.label(
                            RichText::new(id.map_or("#?".to_string(), |id| format!("#{id}")))
                                .font(fonts::semibold(11.5)),
                        );
                        let verdict = text(v, "verdict");
                        let (label, tone) = match verdict.as_str() {
                            "keep" => ("keep".to_string(), Tone::Ok),
                            "drop" => ("drop".to_string(), Tone::Bad),
                            "downgrade" => {
                                (format!("downgrade to {}", text(v, "severity")), Tone::Warn)
                            }
                            other => (other.to_string(), Tone::Neutral),
                        };
                        components::pill(ui, None, &label, tone);
                        if let Some(dup) = v.get("duplicate_of").and_then(serde_json::Value::as_i64)
                        {
                            ui.label(
                                RichText::new(format!("duplicate of #{dup}"))
                                    .size(11.0)
                                    .color(p.faint),
                            );
                        }
                    });
                    let reason = text(v, "reason");
                    if !reason.trim().is_empty() {
                        ui.label(RichText::new(reason).size(11.5).color(p.muted));
                    }
                }
            });
        }
        "submit_review_draft" => {
            let verdict = text(&input, "verdict");
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    title(ui, icons::FILE_TEXT, "Review draft");
                    let tone = match verdict.as_str() {
                        "approve" => Tone::Ok,
                        "request_changes" => Tone::Bad,
                        _ => Tone::Neutral,
                    };
                    if !verdict.is_empty() {
                        components::pill(ui, None, &verdict.replace('_', " "), tone);
                    }
                });
                let rationale = text(&input, "rationale");
                if !rationale.trim().is_empty() {
                    ui.label(RichText::new(rationale).size(11.5).color(p.muted));
                }
                let summary = text(&input, "summary");
                if !summary.trim().is_empty() {
                    markdown::render(ui, &markdown::to_doc(&summary));
                }
            });
        }
        "submit_prior_states" => {
            let Some(states) = input.get("states").and_then(serde_json::Value::as_array) else {
                return false;
            };
            card(ui, |ui| {
                title(
                    ui,
                    icons::REFRESH_CW,
                    &format!("Earlier findings · {}", states.len()),
                );
                for v in states {
                    let id = v.get("finding_id").and_then(serde_json::Value::as_i64);
                    let line = format!(
                        "{} {}{}",
                        id.map_or("#?".to_string(), |id| format!("#{id}")),
                        text(v, "state").replace('_', " "),
                        match text(v, "reason") {
                            r if r.trim().is_empty() => String::new(),
                            r => format!(": {r}"),
                        }
                    );
                    ui.label(RichText::new(line).size(11.5));
                }
            });
        }
        _ => return false,
    }
    true
}

/// The scout's brief: what the PR is for, whether that matches its
/// description, changes it could not explain, and the review groups with
/// their risk. `None` while the call is still being checked.
fn render_brief(ui: &mut egui::Ui, brief: Option<&crate::ai::stages::scout::Brief>) {
    use crate::ai::stages::scout::Risk;

    let p = theme::palette_of(ui);
    let Some(brief) = brief else {
        ui.label(RichText::new("Filing the brief…").color(p.muted).size(11.5));
        return;
    };
    card(ui, |ui| {
        ui.label(
            RichText::new(icons::icon_text(icons::FILE_TEXT, "Brief"))
                .font(fonts::semibold(12.5))
                .color(p.text),
        );
        ui.label(RichText::new(&brief.intent).size(12.0).color(p.text));
        if !brief.description_matches {
            ui.label(
                RichText::new("The PR description does not match the change.")
                    .size(11.5)
                    .color(p.warn),
            );
        }
        if !brief.unexplained.is_empty() {
            ui.label(
                RichText::new("Not explained by the description:")
                    .size(11.5)
                    .color(p.muted),
            );
            for item in &brief.unexplained {
                ui.label(RichText::new(format!("•  {item}")).size(11.5));
            }
        }
        for group in &brief.groups {
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                let (risk, tone) = match group.risk {
                    Risk::High => ("high risk", Tone::Bad),
                    Risk::Medium => ("medium risk", Tone::Warn),
                    Risk::Low => ("low risk", Tone::Neutral),
                };
                ui.label(
                    RichText::new(format!("{} · {}", group.id, group.title))
                        .font(fonts::semibold(11.5)),
                );
                components::pill(ui, None, risk, tone);
                let files = group.files.len();
                ui.label(
                    RichText::new(format!(
                        "{files} file{}{}",
                        if files == 1 { "" } else { "s" },
                        if group.skim { " · skim" } else { "" }
                    ))
                    .size(11.0)
                    .color(p.faint),
                );
            });
            if !group.reason.trim().is_empty() {
                ui.label(RichText::new(&group.reason).size(11.5).color(p.muted));
            }
        }
    });
}

/// "Commands": every decided `bash` call, newest first, one line each
/// (agent, decision badge, command), filtered by the chips; a click shows
/// the whole command and why it was decided.
fn render_commands(ui: &mut egui::Ui, log: &[CommandRow], view: &mut PanelView) {
    if log.is_empty() {
        return;
    }
    let p = theme::palette_of(ui);
    if !section(ui, view, "commands", "COMMANDS", log.len()) {
        return;
    }
    let counts = ai_activity::filter_counts(log);
    ui.horizontal_wrapped(|ui| {
        for (filter, count) in LogFilter::ALL.into_iter().zip(counts) {
            if ui
                .selectable_label(view.filter == filter, format!("{} {count}", filter.label()))
                .clicked()
            {
                view.filter = filter;
            }
        }
    });
    let (rows, more) = visible_log(log, view.filter, view.log_all);
    bordered(ui, |ui| {
        if rows.is_empty() {
            ui.weak("Nothing here.");
        }
        for row in rows {
            let key = (row.at.entry, row.at.tool);
            let open = view.log_open.contains(&key);
            let reason = decision_reason(&row.decision, row.note);
            let clicked = ui
                .horizontal(|ui| {
                    ui.add_sized(
                        [44.0, 18.0],
                        egui::Label::new(RichText::new(&row.agent).color(p.muted).size(11.5))
                            .truncate(),
                    );
                    decision_mark(ui, &row.decision, &reason);
                    ui.add(
                        egui::Button::new(RichText::new(&row.command).monospace().size(11.5))
                            .frame(false)
                            .truncate(),
                    )
                    .on_hover_text(&reason)
                    .clicked()
                })
                .inner;
            if clicked {
                if open {
                    view.log_open.remove(&key);
                } else {
                    view.log_open.insert(key);
                }
            }
            if open {
                mono_line(ui, &row.command);
                ui.label(RichText::new(reason).color(p.muted).size(11.5));
            }
        }
        if more > 0 && ui.link(format!("Show {more} more")).clicked() {
            view.log_all = true;
        }
    });
}

/// "Findings so far": the findings in arrival order (a click jumps the
/// diff there), then the summary (spec §12).
fn render_findings_section(
    ui: &mut egui::Ui,
    st: &AiReviewState,
    show_dropped: bool,
    view: &mut PanelView,
    actions: &mut Vec<AiPanelAction>,
) {
    let ids: Vec<i64> = st
        .timeline
        .iter()
        .filter_map(|entry| match entry {
            TimelineEntry::Finding { id } => Some(*id),
            _ => None,
        })
        .collect();
    let p = theme::palette_of(ui);
    if !ids.is_empty() && section(ui, view, "findings", "FINDINGS SO FAR", ids.len()) {
        for id in ids {
            let finding = st.findings.get(&id);
            let mut label = format!("Finding #{id}");
            if let Some(finding) = finding {
                label = format!("{label} · {}", finding.title);
            }
            let color = finding.map_or(p.muted, |finding| severity_color(p, finding.severity));
            // Truncate: a long finding title must not widen the pane.
            let button =
                egui::Button::new(RichText::new(label.clone()).color(color).size(11.5)).truncate();
            let button = if finding.is_some() {
                ui.add(button).on_hover_text(label)
            } else {
                ui.add(button)
            };
            if button.clicked() {
                actions.push(AiPanelAction::JumpToFinding(id));
            }
        }
    }
    render_findings(ui, st, show_dropped, actions);
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
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Findings").font(fonts::semibold(12.5)));
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

/// "Chat": the conversation with the review, newest last — the user's
/// messages and the replies as bubbles, the chat's tool calls as compact
/// lines with their decision badge. Older messages fold behind "Earlier
/// messages".
fn render_chat_log(ui: &mut egui::Ui, st: &AiReviewState, view: &mut PanelView) {
    let chat: Vec<&TimelineEntry> = st
        .timeline
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                TimelineEntry::ChatUser { .. }
                    | TimelineEntry::ChatReply { .. }
                    | TimelineEntry::ChatTool { .. }
            )
        })
        .collect();
    if chat.is_empty() {
        return;
    }
    let p = theme::palette_of(ui);
    let hidden = if view.chat_earlier {
        0
    } else {
        chat.len().saturating_sub(CHAT_RECENT)
    };
    ui.horizontal(|ui| {
        heading(ui, "CHAT", p.muted);
        if hidden > 0 {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.link(format!("Earlier messages ({hidden})")).clicked() {
                    view.chat_earlier = true;
                }
            });
        }
    });
    let width = ui.available_width();
    for entry in chat.into_iter().skip(hidden) {
        match entry {
            TimelineEntry::ChatUser { text } => {
                ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                    Frame::new()
                        .fill(p.accent)
                        .corner_radius(10.0)
                        .inner_margin(Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.set_max_width(width * 0.8);
                            ui.add(
                                egui::Label::new(RichText::new(text).color(p.accent_ink))
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        });
                });
            }
            TimelineEntry::ChatReply { text } => {
                Frame::new()
                    .fill(p.raised)
                    .corner_radius(10.0)
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_max_width(width * 0.85);
                        markdown::render(ui, &markdown::to_doc(text));
                    });
            }
            TimelineEntry::ChatTool {
                name,
                summary,
                note,
                decision,
            } => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Chat ran").color(p.muted).size(11.5));
                    match decision {
                        Some(decision) => {
                            decision_mark(ui, decision, &decision_reason(decision, *note));
                        }
                        None => {
                            components::pill(ui, None, "tool", Tone::Neutral);
                        }
                    }
                    tool_line(ui, &ai_activity::tool_brief_parts(name, summary), p.muted);
                });
            }
            _ => {}
        }
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
fn render_composer(ui: &mut egui::Ui, st: &AiReviewState, actions: &mut Vec<AiPanelAction>) {
    let text_id = ui.scope_id().with("ai-chat-input");
    let enabled = chat_enabled(&st.phase);
    // Called in a bottom-up layout, so the hint goes first to sit under
    // the field.
    if !enabled {
        ui.weak("Chat is available while a review runs or after one ends.");
    }
    ui.add_enabled_ui(enabled, |ui| {
        // Right-to-left so the send button claims its width first and the
        // field fills the rest: in a left-to-right row an `INFINITY` field
        // takes the full available width and the button overflows past it,
        // which grows the pane a little every frame. The row has a fixed
        // height: `Align::Center` would otherwise center it in all the
        // height left.
        let row = egui::vec2(ui.available_width(), ui.spacing().interact_size.y + 8.0);
        ui.allocate_ui_with_layout(
            row,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                let send = icons::icon_button(ui, icons::SEND, "Send message");
                let mut text: String =
                    ui.data_mut(|d| d.get_persisted(text_id).unwrap_or_default());
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
            },
        );
    });
}

/// A raised card for the checkout details and the suggestions.
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.raised)
        .corner_radius(8.0)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, add);
}

/// A "Needs you" card: the pending tint with an amber border.
fn ask_card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.pending_soft)
        .stroke((1.0, p.pending))
        .corner_radius(8.0)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// A bordered list (the agents and the command log).
fn bordered(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette_of(ui);
    Frame::new()
        .stroke((1.0, p.line))
        .corner_radius(8.0)
        .inner_margin(Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// A mono command block, wrapped at spaces. `max_rows` cuts it to that many
/// rows with "…" (the "Needs you" cards' compact form); returns whether it
/// was cut.
fn mono_block(ui: &mut egui::Ui, text: &str, max_rows: Option<usize>) -> bool {
    let p = theme::palette_of(ui);
    let mut elided = false;
    Frame::new()
        .fill(p.surface)
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            let mut job = egui::text::LayoutJob::simple(
                text.to_owned(),
                egui::FontId::monospace(11.5),
                p.text,
                ui.available_width(),
            );
            if let Some(rows) = max_rows {
                job.wrap.max_rows = rows;
                job.wrap.overflow_character = Some('…');
            }
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            elided = galley.elided;
            ui.add(egui::Label::new(galley));
        });
    elided
}

/// A mono command line, wrapped in full: the risk lives in a command's
/// tail (`&& rm -rf …`), so a decision's text is never cut.
fn mono_line(ui: &mut egui::Ui, text: &str) {
    mono_block(ui, text, None);
}

/// One monospace tool-call line that truncates instead of widening the
/// pane; the full text lives in the hover tooltip.
fn tool_line(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text).monospace().color(color).size(11.5)).truncate())
        .on_hover_text(text);
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
    use crate::ai::events::StageStatus;
    use crate::ai::tools::{AgentRef, AgentRole};
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
    fn step_captions_show_progress() {
        let step = |stage, state, agents| Step {
            stage,
            state,
            agents,
            planned: 0,
        };
        assert_eq!(
            step_caption(&step(StageKind::Scout, StepState::Done, (1, 1))),
            (format!("{} Scout", icons::CHECK), "done".to_string())
        );
        let review = step(StageKind::Review, StepState::Active, (1, 3));
        assert_eq!(
            step_caption(&review),
            ("Review".to_string(), "1/3 · 2 running".to_string())
        );
        assert!((step_fill(&review) - 1.0 / 3.0).abs() < 1e-6);
        // The planned count is the total from the start, not the agents
        // started so far: 2 of 22 with 2 running, never 2/4.
        let planned = Step {
            planned: 22,
            ..step(StageKind::Review, StepState::Active, (2, 4))
        };
        assert_eq!(step_caption(&planned).1, "2/22 · 2 running");
        assert!((step_fill(&planned) - 0.15).abs() < 1e-6);
        // A group run again pushes the total past the plan.
        let rerun = Step {
            planned: 3,
            ..step(StageKind::Review, StepState::Active, (3, 4))
        };
        assert_eq!(step_caption(&rerun).1, "3/4 · 1 running");
        assert_eq!(
            step_caption(&step(StageKind::Challenge, StepState::Skipped, (0, 0))),
            ("Challenge".to_string(), "skipped".to_string())
        );
        let pending = step(StageKind::Verify, StepState::Pending, (0, 0));
        assert_eq!(step_caption(&pending).1, "");
        assert_eq!(step_fill(&pending), 0.0);
        assert_eq!(
            step_fill(&step(StageKind::Assemble, StepState::Active, (0, 0))),
            0.5
        );
        assert_eq!(
            step_caption(&step(
                StageKind::Verify,
                StepState::Failed("boom".into()),
                (0, 1)
            )),
            (format!("{} Verify", icons::X), "failed".to_string())
        );
    }

    #[test]
    fn agents_list_in_stage_then_group_order() {
        let row = |entry, title: &str| ai_activity::AgentRow {
            entry,
            title: title.to_string(),
            status: AgentStatus::Running,
            calls: 0,
            activity: String::new(),
        };
        let rows = vec![
            row(0, "Scout"),
            row(1, "Reviewer · g22"),
            row(2, "Reviewer · g2"),
            row(3, "Verifier · src-a.rs"),
            row(4, "Reviewer · g10"),
        ];
        let titles: Vec<String> = sorted_agent_rows(rows)
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert_eq!(
            titles,
            [
                "Scout",
                "Reviewer · g2",
                "Reviewer · g10",
                "Reviewer · g22",
                "Verifier · src-a.rs"
            ]
        );
    }

    #[test]
    fn long_sections_start_collapsed() {
        assert!(section_default_open(SECTION_COLLAPSE_AT));
        assert!(!section_default_open(343));
    }

    #[test]
    fn decision_marks_name_who_decided() {
        use CommandDecision as D;
        let cases = [
            (
                D::Rules {
                    source: "read-only",
                },
                "safe",
                Tone::Ok,
            ),
            (D::Classifier, "classifier", Tone::Info),
            (D::Trust, "full trust", Tone::Bad),
            (D::Remembered, "you allowed it", Tone::Accent),
            (
                D::User { for_review: false },
                "you allowed it",
                Tone::Accent,
            ),
            (D::UserDenied, "you declined it", Tone::Warn),
            (D::Blocked { reason: "x".into() }, "blocked", Tone::Bad),
            (D::AutoDenied { reason: "x".into() }, "refused", Tone::Bad),
        ];
        for (decision, name, tone) in cases {
            let (_, got, got_tone) = decision_badge(&decision);
            assert_eq!((got, got_tone), (name, tone), "{decision:?}");
        }
    }

    #[test]
    fn decision_reasons_explain_and_carry_the_classifier_note() {
        assert_eq!(
            decision_reason(
                &CommandDecision::Rules {
                    source: "read-only"
                },
                None
            ),
            "Ran: a built-in check found it safe (read-only)."
        );
        assert_eq!(
            decision_reason(
                &CommandDecision::User { for_review: false },
                Some(ClassifierNote::NeedsApproval)
            ),
            "Ran: you allowed it once. Classifier: needs approval."
        );
        assert!(
            decision_reason(
                &CommandDecision::Blocked {
                    reason: "secret path".into()
                },
                None
            )
            .starts_with("Blocked: secret path")
        );
    }

    #[test]
    fn auto_approve_chips_follow_the_current_settings() {
        use crate::state::classifier::SchemaSource;
        let view = |mode, on, state| AutoApproveView {
            mode,
            classifier_on: on,
            classifier: state,
        };
        let ready = || ClassifierState::Ready(SchemaSource::BuiltIn);
        assert_eq!(
            level_chip(&view(ApprovalMode::Auto, true, ready())),
            ("Level 3 · Never ask".to_string(), Tone::Ok)
        );
        assert_eq!(
            level_chip(&view(ApprovalMode::Ask, false, ready())),
            ("Level 1 · Ask me".to_string(), Tone::Neutral)
        );
        assert_eq!(
            level_chip(&view(ApprovalMode::Trust, false, ready())).1,
            Tone::Bad,
            "full trust is red"
        );
        // A ready classifier, or a level without one, needs no chip.
        assert_eq!(
            classifier_chip(&view(ApprovalMode::Auto, true, ready()), true),
            None
        );
        assert_eq!(
            classifier_chip(
                &view(
                    ApprovalMode::Ask,
                    false,
                    ClassifierState::Failed("x".into())
                ),
                true
            ),
            None
        );
        assert_eq!(
            classifier_chip(
                &view(ApprovalMode::Ask, true, ClassifierState::Loading),
                true
            )
            .unwrap()
            .0,
            "Classifier: loading"
        );
        assert_eq!(
            classifier_chip(
                &view(
                    ApprovalMode::Lenient,
                    true,
                    ClassifierState::Failed("x".into())
                ),
                true
            )
            .unwrap()
            .0,
            "Classifier: unavailable"
        );
        assert_eq!(
            classifier_chip(&view(ApprovalMode::Auto, true, ClassifierState::Off), false)
                .unwrap()
                .0,
            "Classifier: not in this build"
        );
        let settings = AutoApproveSettings {
            mode: ApprovalMode::Ask,
            local_classifier: true,
            ..AutoApproveSettings::default()
        };
        assert_eq!(
            AutoApproveView::new(&settings, &ClassifierState::Loading),
            view(ApprovalMode::Ask, true, ClassifierState::Loading)
        );
    }

    #[test]
    fn phase_pills_and_titles() {
        assert_eq!(phase_pill(&AiPhase::Running), ("Reviewing", Tone::Accent));
        assert_eq!(
            phase_pill(&AiPhase::Failed("x".into())),
            ("Stopped", Tone::Bad)
        );
        assert_eq!(run_title("acme", "web", 142, 0), "acme/web #142");
        assert_eq!(run_title("acme", "web", 142, 1), "acme/web #142 · 1 file");
        assert_eq!(
            run_title("acme", "web", 142, 107),
            "acme/web #142 · 107 files"
        );
    }

    #[test]
    fn the_log_pages_after_eight_rows() {
        let rows: Vec<CommandRow> = (0..11)
            .map(|n| CommandRow {
                at: crate::state::ai_review::CommandRef {
                    entry: n,
                    tool: None,
                },
                agent: "g1".into(),
                command: format!("cmd {n}"),
                decision: if n % 2 == 0 {
                    CommandDecision::Classifier
                } else {
                    CommandDecision::UserDenied
                },
                note: None,
            })
            .collect();
        let (shown, more) = visible_log(&rows, LogFilter::All, false);
        assert_eq!((shown.len(), more), (LOG_PAGE, 3));
        let (shown, more) = visible_log(&rows, LogFilter::All, true);
        assert_eq!((shown.len(), more), (11, 0));
        let (shown, more) = visible_log(&rows, LogFilter::AskedYou, false);
        assert_eq!((shown.len(), more), (5, 0));
        assert!(
            shown
                .iter()
                .all(|row| row.decision == CommandDecision::UserDenied)
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
                        decision: None,
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
                        ..PanelInputs::default()
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
        // A linked folder folds into one line; its link opens the controls.
        harness.get_by_label("Commands & folder…").click();
        harness.run();
        harness.get_by_label("Change…").click();
        harness.run();
        assert_eq!(*harness.state(), vec![AiPanelAction::ChooseFolder]);

        let mut harness = panel_harness_row(AiReviewState::default(), vm.clone(), None);
        harness.get_by_label("Commands & folder…").click();
        harness.run();
        harness.get_by_label("Unlink").click();
        harness.run();
        assert_eq!(*harness.state(), vec![AiPanelAction::UnlinkFolder]);

        let mut harness = panel_harness_row(AiReviewState::default(), vm, None);
        harness.get_by_label("Commands & folder…").click();
        harness.run();
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
        harness.get_by_label("Commands & folder…").click();
        harness.run_steps(2);
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

    /// The right-pane layout from `pr_detail.rs`: a resizable panel the tab
    /// fills with its own `ScrollArea` (`auto_shrink([false, false])`), which
    /// makes the tab's min content width the pane's min width (the panel
    /// feeds its own response rect back as the next frame's size). A 400+
    /// char decided command with unbreakable path tokens and a long approval
    /// command must therefore neither widen the pane past its splitter width
    /// nor leave content outside the visible pane. The "Needs you" card cuts
    /// the approval command to two rows, and "Full command" lays the whole
    /// command out — tail included, where the risk lives.
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
                            decision: Some(CommandDecision::Rules {
                                source: "read-only",
                            }),
                        },
                        ToolLine {
                            name: "read_file".into(),
                            summary: "{\"path\":\"src/ui/widgets/merge_panel.rs\"}".into(),
                            note: None,
                            decision: None,
                        },
                    ],
                    text: String::new(),
                    stop: None,
                },
                TimelineEntry::Approval {
                    id: 3,
                    agent,
                    kind: crate::ai::ui_interaction::ApprovalKind::Command {
                        command: "git -C /Users/sercand/Developer/otsimo/web log --format=%H%n%an%n%s --since=2026-10-01 -- src/lib/data-tools/push.ts src/lib/data-tools/revision.ts src/lib/data-tools/push-notification-default-links.ts".into(),
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
                        let _ = render(ui, &st, &PanelInputs::default(), false);
                        *c.borrow_mut() = ui.min_rect();
                        *k.borrow_mut() = ui.clip_rect();
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
        let laid_out = |harness: &egui_kittest::Harness<'_>| -> String {
            harness
                .query_all_by(|n| n.role() == egui::accesskit::Role::TextRun)
                .filter_map(|n| n.value())
                .collect()
        };
        // The Label node always carries the full string, so check the
        // laid-out text runs: cut to two rows, the card loses the tail.
        let tail = "src/lib/data-tools/push-notification-default-links.ts";
        assert!(
            !laid_out(&harness).contains(tail),
            "the card shows the command cut to two rows"
        );
        harness.get_by_label("Full command").click();
        harness.run_steps(3);
        // Joined, the runs must now contain the command's last token
        // (wrapping only breaks at spaces/hyphens, which the token's
        // characters survive contiguously).
        let laid_out = laid_out(&harness);
        assert!(
            laid_out.contains(tail),
            "the approval command was not laid out in full: {laid_out:?}"
        );
    }

    /// A verifier's verdicts and an agent's `done` show as cards, with every
    /// verdict's reason; a cut-short input falls back to the plain line.
    #[test]
    fn result_tools_show_as_cards() {
        use egui_kittest::kittest::Queryable;

        let verdicts = serde_json::json!({"verdicts": [
            {"finding_id": 82, "verdict": "drop", "reason": "Re-read Cargo.lock:2738"},
            {"finding_id": 83, "verdict": "keep", "reason": "still panics"},
        ]})
        .to_string();
        let done = serde_json::json!({"summary": "Verified finding #82."}).to_string();
        let harness = egui_kittest::Harness::builder()
            .with_size([420.0, 600.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui(move |ui| {
                assert!(render_result_card(ui, "submit_verdicts", &verdicts));
                assert!(render_result_card(ui, "done", &done));
                assert!(!render_result_card(ui, "done", "{\"summary\":\"cut"));
                assert!(!render_result_card(ui, "read_file", "{}"));
            });
        harness.get_by_label("Re-read Cargo.lock:2738");
        harness.get_by_label("#83");
        harness.get_by_label_contains("Verdicts · 2");
        harness.get_by_label_contains("Finished");
    }

    /// The scout's `submit_brief` call shows as the brief it filed: the
    /// intent and each group with its risk, never the raw JSON.
    #[test]
    fn the_brief_shows_as_text() {
        use crate::ai::stages::scout::{Brief, Group, GroupFile, Risk};
        use egui_kittest::kittest::Queryable;

        let brief = Brief {
            intent: "Adds the Nemotron transcribe model".into(),
            description_matches: false,
            unexplained: vec!["a CI tweak".into()],
            groups: vec![Group {
                id: "g1".into(),
                title: "Decoder".into(),
                risk: Risk::High,
                reason: "new attention kernel".into(),
                files: vec![GroupFile {
                    path: "a.rs".into(),
                    hunks: None,
                }],
                leads: vec![],
                extra_checks: vec![],
                ci_notes: vec![],
                skim: false,
            }],
            suggested_commands: vec![],
            suggested_prepare: vec![],
        };
        let harness = egui_kittest::Harness::builder()
            .with_size([420.0, 600.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui(move |ui| render_brief(ui, Some(&brief)));
        harness.get_by_label("Adds the Nemotron transcribe model");
        harness.get_by_label("g1 · Decoder");
        harness.get_by_label("The PR description does not match the change.");
    }

    /// The triage rows count drafts per severity and report Keep all /
    /// Drop all for one severity.
    #[test]
    fn quick_triage_reports_keep_and_drop_per_severity() {
        use crate::ai::review::{FindingAnchor, FindingState, Severity};
        use crate::test_support::{ai_finding, line_anchor};
        use egui_kittest::kittest::Queryable;

        let mut st = AiReviewState::default();
        for (id, severity, state) in [
            (1, Severity::Bug, FindingState::Draft),
            (2, Severity::Bug, FindingState::Draft),
            (3, Severity::Nit, FindingState::Draft),
            (4, Severity::Nit, FindingState::Kept),
        ] {
            st.findings.insert(
                id,
                ai_finding(
                    id,
                    FindingAnchor::Line(line_anchor("a.rs", 1)),
                    severity,
                    state,
                    "t",
                    "b",
                ),
            );
        }
        assert_eq!(draft_counts(&st), [(Severity::Bug, 2), (Severity::Nit, 1)]);
        let mut harness = panel_harness(st);
        harness.run();
        harness.get_by_label("Keep all 2").click();
        harness.run();
        harness.get_by_label("Drop all 1").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![
                AiPanelAction::Triage {
                    severity: Severity::Bug,
                    keep: true
                },
                AiPanelAction::Triage {
                    severity: Severity::Nit,
                    keep: false
                },
            ]
        );
    }

    /// The composer stays pinned at the bottom of a tall pane and leaves the
    /// rest to the scroll area, frame after frame: a stopped run's Start
    /// button is painted in full and still clicks.
    #[test]
    fn the_composer_stays_at_the_bottom_of_a_tall_pane() {
        use egui_kittest::kittest::Queryable;

        let mut harness = panel_harness(AiReviewState {
            phase: AiPhase::Failed("the run was interrupted".to_string()),
            last_reviewed_sha: Some("abc".to_string()),
            ..AiReviewState::default()
        });
        for _ in 0..5 {
            harness.run();
        }
        let send = harness.get_by_label("Send message").rect();
        assert!(send.bottom() > 850.0, "composer left the bottom: {send:?}");
        let start = harness.get_by_label("Start AI review").rect();
        assert!(
            start.height() > 16.0 && start.bottom() < 300.0,
            "the Start button is clipped: {start:?}"
        );
        harness.get_by_label("Continue review");
        harness.get_by_label("Start AI review").click();
        harness.run();
        assert_eq!(*harness.state(), vec![AiPanelAction::Start]);
    }

    /// The auto-approve chips open the popover, whose button opens the
    /// settings dialog; full trust is stated in red with the sandbox note.
    #[test]
    fn the_auto_approve_chips_open_the_settings_dialog() {
        use egui_kittest::kittest::Queryable;

        let view = AutoApproveView {
            mode: ApprovalMode::Trust,
            classifier_on: false,
            classifier: ClassifierState::Off,
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size([420.0, 900.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui_state(
                move |ui, actions: &mut Vec<AiPanelAction>| {
                    let inputs = PanelInputs {
                        auto_approve: Some(&view),
                        ..PanelInputs::default()
                    };
                    let st = AiReviewState {
                        phase: AiPhase::Paused,
                        ..AiReviewState::default()
                    };
                    actions.extend(render(ui, &st, &inputs, false));
                },
                Vec::new(),
            );
        harness.run();
        harness.get_by_label("Full trust is on: every command runs without asking.");
        harness.get_by_label(TRUST_SANDBOX_NOTE);
        harness.get_by_label("Level 5 · Full trust").click();
        harness.run();
        harness.get_by_label("Auto-approve settings…").click();
        harness.run();
        assert_eq!(*harness.state(), vec![AiPanelAction::OpenAutoApprove]);
    }

    /// A decided approval leaves "Needs you": its command is in the log,
    /// so no decision buttons remain.
    #[test]
    fn decided_approvals_leave_needs_you() {
        use egui_kittest::kittest::Queryable;

        let agent = AgentRef {
            id: 1,
            role: AgentRole::Reviewer,
            label: "reviewer:g1".to_string(),
        };
        let approval = |decided: Option<&str>| TimelineEntry::Approval {
            id: 3,
            agent: agent.clone(),
            kind: ApprovalKind::Command {
                command: "cargo test".into(),
                reason: "runs code".into(),
            },
            decided: decided.map(str::to_string),
        };
        let mut harness = panel_harness(AiReviewState {
            timeline: vec![approval(None)],
            ..AiReviewState::default()
        });
        harness.run();
        harness.get_by_label("NEEDS YOU · 1");
        harness.get_by_label("For this review").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            vec![AiPanelAction::Approve {
                id: 3,
                decision: ApprovalDecision::ForReview
            }]
        );

        let mut harness = panel_harness(AiReviewState {
            timeline: vec![approval(Some("Once"))],
            ..AiReviewState::default()
        });
        harness.run();
        assert!(harness.query_by_label("For this review").is_none());
        assert!(harness.query_by_label("cargo test").is_none());
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
