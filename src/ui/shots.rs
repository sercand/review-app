//! Offscreen screenshots of every screen, for comparing against the HTML
//! mockups in `docs/design/mockups/`.
//!
//! The tests are `#[ignore]`d because they need a GPU adapter. Run them with
//! `cargo test --lib shots -- --ignored`; PNGs land in `target/ui-shots/`.
//! Missing glyphs fail the run, which also catches egui's debug-build ID
//! clash warnings (drawn with a 🔥 no bundled font has).

use eframe::egui;
use egui_kittest::Harness;

use crate::diff::highlight::{HighlightCache, SyntectHighlighter};
use crate::model::comment::CommentTarget;
use crate::model::diff::{DiffMode, Side};
use crate::model::pr::{CiState, ReviewDecision};
use crate::model::thread::{ReviewThread, SubjectType, ThreadComment};
use crate::persist::settings::Settings;
use crate::runtime::{Event, SessionId};
use crate::state::pr_detail::PrSession;
use crate::state::pr_list::PrListState;
use crate::test_support::{line_anchor, loaded, meta, pr_summary, repo};
use crate::ui::screens::{login, pr_detail, pr_list};
#[cfg(unix)]
use crate::ui::widgets::settings_ai_tab::{ModelDialog, ProviderDialog};
use crate::ui::widgets::settings_window::{self, AccountInfo, SettingsWindowState};
#[cfg(unix)]
use crate::ui::widgets::settings_window::{ModelRole, SettingsTab};
use crate::ui::{fonts, theme};

const SIZE: [f32; 2] = [1440.0, 900.0];

/// A harness set up the way `ReviewApp::new` sets up the real context.
fn harness<'a>(dark: bool, app: impl FnMut(&mut egui::Ui) + 'a) -> Harness<'a> {
    harness_sized(dark, SIZE, app)
}

/// A harness on a canvas of `size`, for a window taller than a screen (the
/// settings window's AI section scrolls, so its shot needs the room).
fn harness_sized<'a>(
    dark: bool,
    size: [f32; 2],
    app: impl FnMut(&mut egui::Ui) + 'a,
) -> Harness<'a> {
    Harness::builder()
        .with_size(size)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .with_context_setup(|ctx| {
            ctx.set_fonts(fonts::font_definitions());
            egui_extras::install_image_loaders(ctx);
            theme::install(ctx, 13.0);
        })
        .with_accessibility_check(false)
        .wgpu()
        .build_ui(app)
}

/// Run a few frames (popups and fonts settle) and write the PNG.
fn save(h: Harness<'_>, name: &str) {
    save_after(h, name, 4);
}

/// [`save`] after `steps` frames: a window whose layout feeds back into its
/// own height (a scroll area sized from the viewport) needs a frame or two
/// more to settle.
fn save_after(mut h: Harness<'_>, name: &str, steps: usize) {
    h.run_steps(steps);
    let image = h.render().expect("render");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-shots");
    std::fs::create_dir_all(&dir).unwrap();
    image.save(dir.join(format!("{name}.png"))).unwrap();
}

/// One fixture row: title, repo, number, author, review, draft, CI, +, −,
/// comments, updated.
type PrRow<'a> = (
    &'a str,
    &'a str,
    u64,
    &'a str,
    Option<ReviewDecision>,
    bool,
    Option<CiState>,
    u32,
    u32,
    u32,
    &'a str,
);

fn pr_list_state() -> PrListState {
    let rows: [PrRow; 8] = [
        (
            "Performance: fix long-session memory leaks and top bar, gallery and loading costs",
            "otsimo/kedis-studio",
            2298,
            "Cem-Celik",
            Some(ReviewDecision::ReviewRequired),
            false,
            Some(CiState::Success),
            142,
            57,
            0,
            "2026-10-19T12:52:48Z",
        ),
        (
            "Gallery: stop pixel posters from showing up twice after a push notification",
            "otsimo/kedis-studio",
            2300,
            "Cem-Celik",
            Some(ReviewDecision::ReviewRequired),
            false,
            Some(CiState::Pending),
            3,
            0,
            0,
            "2026-10-19T12:47:52Z",
        ),
        (
            "Flux3 klein",
            "sercand/shiso-llm",
            44,
            "sercand",
            None,
            false,
            Some(CiState::Success),
            13170,
            184,
            0,
            "2026-10-18T13:26:30Z",
        ),
        (
            "qwen4exp: serve NVIDIA's Qwen3.8-Flash-Next FP4-Local (MXFP8/NVFP4/CSF), FP8 KV, draft-head fixes",
            "sercand/shiso-llm",
            43,
            "sercand",
            None,
            false,
            Some(CiState::Failure),
            34429,
            1535,
            0,
            "2026-10-18T08:03:58Z",
        ),
        (
            "Upgrade Piknik v5",
            "otsimo/kedis-studio",
            2297,
            "gozdecoban",
            Some(ReviewDecision::ReviewRequired),
            false,
            Some(CiState::Success),
            47,
            336,
            0,
            "2026-10-16T14:36:51Z",
        ),
        (
            "Add Sago DevUI debug menu improvements bundle",
            "otsimo/kedis-studio",
            1902,
            "egealtar",
            Some(ReviewDecision::ChangesRequested),
            false,
            Some(CiState::Success),
            1381,
            0,
            11,
            "2026-08-25T09:14:16Z",
        ),
        (
            "Add Edit Mode unit tests for Storage, App, and Ranking (149 tests)",
            "otsimo/kedis-studio",
            1731,
            "tayfunotsimo",
            Some(ReviewDecision::ChangesRequested),
            true,
            None,
            3518,
            37,
            10,
            "2026-07-10T07:50:21Z",
        ),
        (
            "Resolve: \"Webstore: Adding premium-status-change and billing history support\"",
            "otsimo/mono",
            1218,
            "sinantomruk",
            Some(ReviewDecision::Approved),
            false,
            Some(CiState::Success),
            813,
            22,
            5,
            "2023-10-06T07:53:53Z",
        ),
    ];
    let mut state = PrListState::default();
    state.items = rows
        .iter()
        .map(
            |&(title, rp, number, author, review, draft, ci, add, del, comments, updated)| {
                let (owner, name) = rp.split_once('/').unwrap();
                let mut s = pr_summary(number, title, comments);
                s.repo = crate::model::RepoRef::new(owner, name);
                s.author = author.into();
                s.review = review;
                s.draft = draft;
                s.ci = ci;
                s.additions = add;
                s.deletions = del;
                s.updated_at = updated.into();
                s.scopes = vec![
                    if author == "sercand" {
                        "authored"
                    } else {
                        "review-requested"
                    }
                    .into(),
                ];
                s
            },
        )
        .collect();
    state
}

const PUSH_TS: &str = "@@ -7,10 +7,11 @@\n-import { content } from \"@/api\";\n+import { apps, content } from \"@/api\";\n import type { ResourceState } from \"@/api/pb/journey/journey_resources.pb\";\n import type {\n   Notification,\n   NotificationRevision,\n   NotificationTemplate,\n } from \"@/api/pb/journey/journey_notification.pb\";\n+import { hasNoLinks, withDefaultLinks } from \"@/lib/notifications/default-links\";\n import { probeRevisions } from \"@/lib/revision\";\n import { resolveMaskPaths } from \"./mask\";\n import { PUSH_NOTIFICATION, type DataRecord, type DataToolContext, type WriteInput } from \"./types\";\n@@ -71,12 +72,20 @@ async function createPushTemplate(\n   });\n   const template = created.template ?? {};\n   let revision = created.revision ?? {};\n+  const prefilled = await withCreateDefaults(ctx, envelope.notification);\n   // createPushTemplate seeds revision 1 with an empty Notification, so a\n   // create that stopped here would hand the user an empty shell.\n   if (envelope.notification) {\n     revision = await writeRevision(\n       template.id ?? \"\",\n       revision.revision ?? 1,\n       revision.version ?? 0,\n-      envelope.notification,\n+      prefilled ?? envelope.notification,\n     );\n+  } else if (prefilled) {\n+    // Only the default links to write. The template exists already, so a failure here must not read as a failed create.\n+    try {\n+      revision = await writeRevision(template.id ?? \"\", revision.revision ?? 1, revision.version ?? 0, prefilled);\n+    } catch (err) {\n+      return { template, warning: `Default links were not saved: ${String(err)}` };\n+    }\n   }\n   return { template, revision };\n }\n";

fn session(mode: DiffMode) -> PrSession {
    let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 19);
    s.on_event(&Event::PrMetadataLoaded {
        session: SessionId(1),
        meta: meta("sha1"),
    });
    let files = [
        (
            "src/components/apps/metadata/metadata-sections.tsx",
            "@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n",
        ),
        (
            "src/components/flows/canvas/node-kinds.test.ts",
            "@@ -0,0 +1,3 @@\n+a\n+b\n+c\n",
        ),
        (
            "src/components/flows/nodes/push-action-form.tsx",
            "@@ -1,2 +1,2 @@\n-a\n+b\n c\n",
        ),
        ("src/lib/data-tools/push.ts", PUSH_TS),
        (
            "src/lib/notifications/default-links.ts",
            "@@ -0,0 +1,2 @@\n+export const A = 1;\n+export const B = 2;\n",
        ),
        ("package-lock.json", "@@ -1 +1 @@\n-{}\n+{ }\n"),
    ];
    s.on_event(&Event::PrFilesPage {
        session: SessionId(1),
        files: files.iter().map(|(p, patch)| loaded(p, patch)).collect(),
        done: true,
    });
    let push = s
        .files
        .iter()
        .position(|f| f.file.path.ends_with("push.ts"))
        .unwrap();
    s.threads = vec![ReviewThread {
        id: "T1".into(),
        is_resolved: false,
        is_outdated: false,
        path: "src/lib/data-tools/push.ts".into(),
        line: Some(75),
        start_line: None,
        side: Some(Side::Right),
        subject_type: SubjectType::Line,
        comments: vec![
            ThreadComment {
                pending: false,
                database_id: 1,
                author: "egealtar".into(),
                avatar_url: None,
                body: "This runs an `apps.getApp` lookup on every create. Can we skip it when the author already set links?".into(),
                created_at: "2026-10-17T10:00:00Z".into(),
            },
            ThreadComment {
                pending: false,
                database_id: 2,
                author: "dogacanaydin".into(),
                avatar_url: None,
                body: "It returns before the lookup when any link is set.".into(),
                created_at: "2026-10-18T10:00:00Z".into(),
            },
        ],
    }];
    // A file-level comment in the viewer's pending review on GitHub.
    s.threads.push(ReviewThread {
        id: "T2".into(),
        is_resolved: false,
        is_outdated: false,
        path: "src/lib/data-tools/push.ts".into(),
        line: None,
        start_line: None,
        side: None,
        subject_type: SubjectType::File,
        comments: vec![ThreadComment {
            pending: true,
            database_id: 3,
            author: "sercand".into(),
            avatar_url: None,
            body: "move this to under plugins with name IDMapTools".into(),
            created_at: "2026-10-16T10:00:00Z".into(),
        }],
    });
    s.conversation = vec![crate::github::conversation::ConversationComment {
        author: "dogacanaydin".into(),
        body: "Rebased on main. The Android fix in #17 depends on this.\n\nCritical:\n\n- first point\n- second point that is long enough to wrap onto another line in the pane".into(),
        created_at: "2026-10-18T10:00:00Z".into(),
    }];
    s.reviews = vec![crate::github::reviews::Review {
        author: "egealtar".into(),
        body: "Looks right. One question on the create path, left inline.".into(),
        submitted_at: Some("2026-10-17T10:00:00Z".into()),
        ..crate::test_support::review(crate::github::reviews::ReviewState::Commented)
    }];
    s.set_mode(mode);
    s.select_file(push);
    s.open_comment(CommentTarget::Line(line_anchor(
        "src/lib/data-tools/push.ts",
        83,
    )));
    s.rebuild_rows();
    s
}

fn detail(mode: DiffMode, dark: bool, name: &str) {
    let mut s = session(mode);
    let mut state = pr_detail::PrDetailState {
        right_open: true,
        ..Default::default()
    };
    // Unified shots also exercise word wrap and the conversation tab.
    state.diff.wrap = mode == DiffMode::Unified;
    if mode == DiffMode::Unified {
        state.details_tab = crate::ui::widgets::metadata_panel::DetailsTab::Conversation;
    }
    let mut cache = HighlightCache::new(512, Box::new(SyntectHighlighter::new(dark)));
    let h = harness(dark, move |ui| {
        let mut diff_actions = Vec::new();
        pr_detail::render(
            ui,
            &mut s,
            &mut state,
            &mut cache,
            &mut diff_actions,
            pr_detail::ScreenInputs {
                login: "sercand",
                write_blocked: None,
                theme: theme::Theme::Dark,
                system_dark: dark,
                ai_configured: false,
                #[cfg(unix)]
                local_checkout: None,
                #[cfg(unix)]
                repo_ai: None,
            },
        );
    });
    save(h, name);
}

/// A live AI review: stage strip, a running reviewer, an unanswered question
/// and a pending command approval (spec §12).
#[cfg(unix)]
fn ai_state_running() -> crate::state::ai_review::AiReviewState {
    use crate::ai::events::{StageKind, StageStatus};
    use crate::ai::review::{
        CheckResult, CoverageEntry, Finding, FindingAnchor, FindingState, Severity,
    };
    use crate::ai::tools::{AgentRef, AgentRole};
    use crate::ai::ui_interaction::ApprovalKind;
    use crate::model::comment::Anchor;
    use crate::model::diff::Side;
    use crate::state::ai_review::{AiPhase, AiReviewState, TimelineEntry};

    let reviewer = AgentRef {
        id: 1,
        role: AgentRole::Reviewer,
        label: "reviewer:g1".to_string(),
    };
    let scout = AgentRef {
        id: 2,
        role: AgentRole::Scout,
        label: "scout".to_string(),
    };
    let mut st = AiReviewState {
        phase: AiPhase::Running,
        timeline: vec![
            TimelineEntry::Stage {
                stage: StageKind::Scout,
                status: StageStatus::Done,
            },
        TimelineEntry::Agent {
            agent: scout,
            group: None,
            tools: vec![("read_file".into(), "src/lib/data-tools/push.ts".into())],
            text: "Grouped the 6 changed files into 2 review groups.".into(),
            stop: Some("finished".into()),
        },
        TimelineEntry::Stage {
            stage: StageKind::Review,
            status: StageStatus::Started,
        },
        TimelineEntry::Agent {
            agent: reviewer.clone(),
            group: Some("notifications".into()),
            tools: vec![
                (
                    "bash".into(),
                    // A real scout/reviewer tool call: 400+ chars of JSON with
                    // long unbreakable path tokens (the live smoke-test bug).
                    "{\"command\":\"for f in docs/superpowers/plans/2026-10-20-ai-review-1-foundation.md docs/superpowers/plans/2026-10-20-ai-review-2-state-machine.md docs/superpowers/plans/2026-10-20-ai-review-3-ui-chat.md docs/superpowers/specs/2026-10-19-ai-review-design.md ; do echo \\\"=== $f ===\\\"; head -40 \\\"$f\\\"; done\"}".into(),
                ),
                (
                    "read_file".into(),
                    "{\"path\":\"src/ui/widgets/merge_panel.rs\"}".into(),
                ),
            ],
            text: "The create path looks up links even when the author set them. Checking whether the lookup can fail the create.".into(),
            stop: None,
        },
        TimelineEntry::Question {
            id: 7,
            agent: reviewer.clone(),
            text: "Is an empty notification list allowed here?".into(),
            options: vec!["yes".into(), "no".into()],
            answer: None,
        },
        TimelineEntry::Approval {
            id: 3,
            agent: reviewer.clone(),
            kind: ApprovalKind::Command {
                command: "git -C /Users/sercand/Developer/otsimo/web log --format=%H%n%an%n%s --since=2026-10-01 -- src/lib/data-tools/push-notification-default-links.ts".into(),
                reason: "the agent wants the recent history of the default-links module to see whether the lookup was changed on purpose".into(),
            },
            decided: None,
        },
            TimelineEntry::Finding { id: 1 },
            TimelineEntry::Notice {
                text: "the agent shell is unsandboxed: no bwrap".into(),
            },
        ],
        coverage: vec![CoverageEntry {
            file: "package-lock.json".into(),
            item: "errors".into(),
            result: CheckResult::Skipped,
            note: String::new(),
        }],
        usage: (48_213, 3_904),
        ..AiReviewState::default()
    };
    st.findings.insert(
        1,
        Finding {
            id: 1,
            run_id: 9,
            agent: reviewer,
            anchor: FindingAnchor::Line(Anchor {
                path: "src/lib/data-tools/push.ts".into(),
                side: Side::Right,
                line: 83,
                start_line: None,
                start_side: None,
            }),
            severity: Severity::Bug,
            title: "create fails when the link lookup rejects".into(),
            body: "body".into(),
            evidence: vec![],
            checklist_item: None,
            state: FindingState::Draft,
            state_reason: None,
            pending_local_id: None,
        },
    );
    st
}

/// A finished AI review: answered question, decided approval, a chat
/// exchange, findings including verifier drops and prior states (spec §12).
#[cfg(unix)]
fn ai_state_done() -> crate::state::ai_review::AiReviewState {
    use crate::ai::events::{StageKind, StageStatus};
    use crate::ai::review::{
        CheckResult, CoverageEntry, Finding, FindingAnchor, FindingState, Severity,
    };
    use crate::ai::tools::{AgentRef, AgentRole};
    use crate::ai::ui_interaction::ApprovalKind;
    use crate::model::comment::Anchor;
    use crate::model::diff::Side;
    use crate::state::ai_review::{AiPhase, AiReviewState, TimelineEntry};

    let reviewer = AgentRef {
        id: 1,
        role: AgentRole::Reviewer,
        label: "reviewer:g1".to_string(),
    };
    let finding = |id: i64, severity, state, title: &str| Finding {
        id,
        run_id: 12,
        agent: reviewer.clone(),
        anchor: FindingAnchor::Line(Anchor {
            path: "src/lib/data-tools/push.ts".into(),
            side: Side::Right,
            line: 83,
            start_line: None,
            start_side: None,
        }),
        severity,
        title: title.into(),
        body: "body".into(),
        evidence: vec![],
        checklist_item: None,
        state,
        state_reason: None,
        pending_local_id: None,
    };
    let mut st = AiReviewState {
        phase: AiPhase::Done,
        last_reviewed_sha: Some("sha1".into()),
        timeline: vec![
            TimelineEntry::Stage {
                stage: StageKind::Scout,
                status: StageStatus::Done,
            },
            TimelineEntry::Stage {
                stage: StageKind::Review,
                status: StageStatus::Done,
            },
            TimelineEntry::Stage {
                stage: StageKind::Verify,
                status: StageStatus::Done,
            },
            TimelineEntry::Stage {
                stage: StageKind::Assemble,
                status: StageStatus::Done,
            },
            TimelineEntry::Agent {
                agent: reviewer.clone(),
                group: Some("notifications".into()),
                tools: vec![("read_file".into(), "src/lib/data-tools/push.ts".into())],
                text:
                    "The create path fails when the link lookup rejects; the warning path is fine."
                        .into(),
                stop: Some("finished".into()),
            },
            TimelineEntry::Question {
                id: 7,
                agent: reviewer.clone(),
                text: "Is an empty notification list allowed here?".into(),
                options: vec!["yes".into(), "no".into()],
                answer: Some("yes".into()),
            },
            TimelineEntry::Approval {
                id: 3,
                agent: reviewer.clone(),
                kind: ApprovalKind::Command {
                    command: "cargo test -p notifier".into(),
                    reason: "it runs code".into(),
                },
                decided: Some("For this review".into()),
            },
            TimelineEntry::Finding { id: 1 },
            TimelineEntry::Finding { id: 2 },
            TimelineEntry::ChatUser {
                text: "focus on the create path".into(),
            },
            TimelineEntry::ChatTool {
                name: "note_agent".into(),
                summary: "{\"agent_id\":4,\"text\":\"focus on the create path\"}".into(),
            },
            TimelineEntry::ChatReply {
                text: "Both reviewers now cover the create path; one bug found.".into(),
            },
        ],
        coverage: vec![
            CoverageEntry {
                file: "package-lock.json".into(),
                item: "errors".into(),
                result: CheckResult::Skipped,
                note: String::new(),
            },
            CoverageEntry {
                file: "src/lib/revision.ts".into(),
                item: "errors".into(),
                result: CheckResult::Skipped,
                note: String::new(),
            },
            CoverageEntry {
                file: "src/lib/data-tools/push.ts".into(),
                item: "errors".into(),
                result: CheckResult::Ok,
                note: String::new(),
            },
        ],
        usage: (128_900, 12_450),
        ..AiReviewState::default()
    };
    for f in [
        finding(
            1,
            Severity::Bug,
            FindingState::Kept,
            "create fails when the link lookup rejects",
        ),
        finding(
            2,
            Severity::Risk,
            FindingState::Draft,
            "warning path hides a failed default-links write",
        ),
        finding(
            3,
            Severity::Nit,
            FindingState::DroppedByVerifier,
            "stale comment on probeRevisions",
        ),
        finding(
            4,
            Severity::Question,
            FindingState::Kept,
            "should prefilled defaults be logged?",
        ),
        finding(
            5,
            Severity::Bug,
            FindingState::Addressed,
            "mask paths resolved twice",
        ),
        finding(
            6,
            Severity::Risk,
            FindingState::StillPresent,
            "revision version race",
        ),
    ] {
        st.findings.insert(f.id, f);
    }
    // A summary-anchored draft: the AI tab's findings list is its only
    // Keep / Drop surface (spec §5.6).
    st.findings.insert(
        7,
        Finding {
            anchor: FindingAnchor::Summary {
                path: "assets/logo.png".into(),
            },
            ..finding(
                7,
                Severity::Question,
                FindingState::Draft,
                "why does the exported asset lack a license header?",
            )
        },
    );
    st
}

/// The PR detail screen with the ✦ AI tab open on `ai`.
#[cfg(unix)]
fn ai_detail(dark: bool, name: &str, ai: crate::state::ai_review::AiReviewState) {
    ai_detail_pane(dark, name, ai, None);
}

/// [`ai_detail`] with an explicit right-pane width: the layout bug this shot
/// guards was only visible at the splitter widths users actually pick.
#[cfg(unix)]
fn ai_detail_pane(
    dark: bool,
    name: &str,
    ai: crate::state::ai_review::AiReviewState,
    right_pane: Option<f32>,
) {
    let mut s = session(DiffMode::Split);
    s.ai = ai;
    let mut state = pr_detail::PrDetailState {
        right_open: true,
        ..Default::default()
    };
    if let Some(right_pane) = right_pane {
        state.right_pane = right_pane;
    }
    state.details_tab = crate::ui::widgets::metadata_panel::DetailsTab::Ai;
    let mut cache = HighlightCache::new(512, Box::new(SyntectHighlighter::new(dark)));
    let h = harness(dark, move |ui| {
        let mut diff_actions = Vec::new();
        pr_detail::render(
            ui,
            &mut s,
            &mut state,
            &mut cache,
            &mut diff_actions,
            pr_detail::ScreenInputs {
                login: "sercand",
                write_blocked: None,
                theme: theme::Theme::Dark,
                system_dark: dark,
                ai_configured: true,
                #[cfg(unix)]
                local_checkout: None,
                #[cfg(unix)]
                repo_ai: None,
            },
        );
    });
    // The right pane's width feeds back from its own response rect (the app
    // stores it as `right_pane`, egui as `PanelState`), so an oversized tab
    // needs several frames to show its full growth.
    save_after(h, name, 10);
}

/// The diff with one inline AI draft row: ✦ tag, severity, body, evidence
/// and the Keep / Edit / Drop / Why? controls (spec §12, AI Task 5).
#[cfg(unix)]
fn ai_inline_draft(dark: bool, name: &str) {
    use crate::ai::review::{Evidence, Finding, FindingAnchor, FindingState, Severity};
    use crate::model::comment::Anchor;

    let mut s = session(DiffMode::Unified);
    // The shared fixture opens a composer under line 83; this shot is about
    // the AI row alone.
    s.composer = crate::state::composer::ComposerState::default();
    s.ai.findings.insert(
        1,
        Finding {
            id: 1,
            run_id: 9,
            agent: crate::ai::tools::AgentRef {
                id: 1,
                role: crate::ai::tools::AgentRole::Reviewer,
                label: "reviewer:g1".to_string(),
            },
            anchor: FindingAnchor::Line(Anchor {
                path: "src/lib/data-tools/push.ts".into(),
                side: Side::Right,
                line: 8,
                start_line: None,
                start_side: None,
            }),
            severity: Severity::Bug,
            title: "create fails when the link lookup rejects".into(),
            body: "withDefaultLinks throws when the app was deleted; the create \
                   path treats that as fatal and drops the template."
                .into(),
            evidence: vec![Evidence {
                path: "src/lib/data-tools/push.ts".into(),
                line: Some(78),
                note: "the lookup sits inside the create path".into(),
            }],
            checklist_item: None,
            state: FindingState::Draft,
            state_reason: None,
            pending_local_id: None,
        },
    );
    s.rebuild_rows();

    let mut state = pr_detail::PrDetailState {
        right_open: true,
        ..Default::default()
    };
    state.diff.wrap = true;
    let mut cache = HighlightCache::new(512, Box::new(SyntectHighlighter::new(dark)));
    let h = harness(dark, move |ui| {
        let mut diff_actions = Vec::new();
        pr_detail::render(
            ui,
            &mut s,
            &mut state,
            &mut cache,
            &mut diff_actions,
            pr_detail::ScreenInputs {
                login: "sercand",
                write_blocked: None,
                theme: theme::Theme::Dark,
                system_dark: dark,
                ai_configured: true,
                #[cfg(unix)]
                local_checkout: None,
                #[cfg(unix)]
                repo_ai: None,
            },
        );
    });
    save(h, name);
}

/// The checkout confirmation dialog over the PR (spec §4 step 2, §12).
#[cfg(unix)]
fn ai_checkout_dialog(dark: bool, name: &str) {
    use crate::ai::checkout::{CheckoutPlan, PrTarget};
    use crate::ai::store::OriginalRef;
    use crate::ui::widgets::ai_dialogs::{self, AiDialog, DialogInputs};

    let mut s = session(DiffMode::Split);
    let dialog = AiDialog::ConfirmCheckout(CheckoutPlan {
        repo_path: std::path::PathBuf::from("/Users/sercand/Developer/otsimo/web"),
        remote: "origin".to_string(),
        original: OriginalRef::Branch("main".to_string()),
        dirty: true,
        skip_stash: false,
        commands: vec![
            "git fetch origin pull/19/head:refs/remotes/origin/pr-19".to_string(),
            "git switch --detach refs/remotes/origin/pr-19".to_string(),
            "git stash push --include-untracked -m \"gh-review AI #19\"".to_string(),
        ],
    });
    let pr = PrTarget {
        host: "github.com".to_string(),
        owner: "otsimo".to_string(),
        name: "web".to_string(),
        number: 19,
        head_sha: "sha1".to_string(),
        head_branch: "feature/push-defaults".to_string(),
    };
    let prepare = ["npm ci".to_string()];
    let mut state = pr_detail::PrDetailState {
        right_open: true,
        ..Default::default()
    };
    let mut cache = HighlightCache::new(512, Box::new(SyntectHighlighter::new(dark)));
    let h = harness(dark, move |ui| {
        let mut diff_actions = Vec::new();
        pr_detail::render(
            ui,
            &mut s,
            &mut state,
            &mut cache,
            &mut diff_actions,
            pr_detail::ScreenInputs {
                login: "sercand",
                write_blocked: None,
                theme: theme::Theme::Dark,
                system_dark: dark,
                ai_configured: true,
                #[cfg(unix)]
                local_checkout: None,
                #[cfg(unix)]
                repo_ai: None,
            },
        );
        ai_dialogs::render(
            ui.ctx(),
            &dialog,
            &DialogInputs {
                pr: Some(&pr),
                prepare: &prepare,
            },
            &mut Vec::new(),
        );
    });
    save_after(h, name, 8);
}

/// The Local checkout row at the top of the PR AI tab (AI settings Task 7)
/// in its three states: `linked` shows a usable folder (the app's own
/// checkout, so the clone check is real), otherwise the folder is absent —
/// `missing` names one that was linked and has since gone, unlinked shows
/// the Start-gating reason.
#[cfg(unix)]
fn ai_local_checkout(dark: bool, name: &str, linked: bool, missing: bool) {
    use crate::ai::repo_link::FolderStatus;
    use crate::persist::settings::RepoAiSettings;
    use crate::ui::widgets::ai_panel::LocalCheckoutVm;

    let path = if linked {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    } else if missing {
        std::path::PathBuf::from("/Users/sercand/Developer/src/widgets")
    } else {
        std::path::PathBuf::new()
    };
    let vm = LocalCheckoutVm {
        repo: "acme/widgets".to_string(),
        path: (!path.as_os_str().is_empty()).then(|| path.clone()),
        status: match (linked, missing) {
            (true, _) => FolderStatus::Ok,
            (false, true) => FolderStatus::Missing,
            (false, false) => FolderStatus::Missing,
        },
        error: None,
        blockers: if linked {
            Vec::new()
        } else if missing {
            vec!["The linked folder for github.com/acme/widgets is missing.".to_string()]
        } else {
            vec!["Link a local folder for github.com/acme/widgets.".to_string()]
        },
        change_blocked: None,
    };
    let repo = RepoAiSettings {
        repo: "github.com/acme/widgets".to_string(),
        path,
        commands: vec!["cargo test".to_string(), "cargo clippy".to_string()],
        prepare: vec!["cargo fetch".to_string()],
        ..Default::default()
    };

    let mut s = session(DiffMode::Split);
    let mut state = pr_detail::PrDetailState {
        right_open: true,
        ..Default::default()
    };
    state.details_tab = crate::ui::widgets::metadata_panel::DetailsTab::Ai;
    let mut cache = HighlightCache::new(512, Box::new(SyntectHighlighter::new(dark)));
    let h = harness(dark, move |ui| {
        let mut diff_actions = Vec::new();
        pr_detail::render(
            ui,
            &mut s,
            &mut state,
            &mut cache,
            &mut diff_actions,
            pr_detail::ScreenInputs {
                login: "sercand",
                write_blocked: None,
                theme: theme::Theme::Dark,
                system_dark: dark,
                ai_configured: true,
                local_checkout: Some(&vm),
                repo_ai: Some(&repo),
            },
        );
    });
    save_after(h, name, 10);
}

#[test]
#[ignore = "needs a GPU adapter; writes target/ui-shots/*.png"]
fn shots() {
    for dark in [true, false] {
        let t = if dark { "dark" } else { "light" };

        let mut state = pr_list_state();
        let mut form = pr_list::PrListForm::default();
        let h = harness(dark, move |ui| {
            pr_list::render_title_bar(ui, &mut Vec::new(), &mut Vec::new());
            egui::CentralPanel::default().show(ui, |ui| {
                pr_list::render(
                    ui,
                    &mut state,
                    &mut form,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    "2026-10-20T00:00:00Z",
                );
            });
        });
        save(h, &format!("prs-{t}"));

        detail(DiffMode::Split, dark, &format!("pr-split-{t}"));
        detail(DiffMode::Unified, dark, &format!("pr-unified-{t}"));

        // The ✦ AI tab (spec §12), rendered only on Unix like the feature.
        #[cfg(unix)]
        {
            ai_detail(dark, &format!("pr-ai-running-{t}"), ai_state_running());
            ai_detail(dark, &format!("pr-ai-done-{t}"), ai_state_done());
            // The same running tab in a narrow right pane: the layout bug
            // this guards (pane growth past the splitter + clipped content)
            // only shows at user-real widths.
            ai_detail_pane(
                dark,
                &format!("pr-ai-running-narrow-{t}"),
                ai_state_running(),
                Some(420.0),
            );
            ai_inline_draft(dark, &format!("pr-ai-inline-draft-{t}"));
            ai_checkout_dialog(dark, &format!("pr-checkout-dialog-{t}"));
            // The Local checkout row in its three states (AI settings Task 7).
            ai_local_checkout(
                dark,
                &format!("pr-ai-local-checkout-linked-{t}"),
                true,
                false,
            );
            ai_local_checkout(
                dark,
                &format!("pr-ai-local-checkout-unlinked-{t}"),
                false,
                false,
            );
            ai_local_checkout(
                dark,
                &format!("pr-ai-local-checkout-missing-{t}"),
                false,
                true,
            );
            // The Local repos tab (AI settings Task 9): collapsed, one
            // expanded, empty, and a missing folder.
            local_repos_shot(
                dark,
                &format!("pr-list-local-repos-{t}"),
                false,
                false,
                false,
            );
            local_repos_shot(
                dark,
                &format!("pr-list-local-repos-expanded-{t}"),
                true,
                false,
                false,
            );
            local_repos_shot(
                dark,
                &format!("pr-list-local-repos-empty-{t}"),
                false,
                true,
                false,
            );
            local_repos_shot(
                dark,
                &format!("pr-list-local-repos-missing-{t}"),
                false,
                false,
                true,
            );
        }

        let settings = Settings::default();
        let vm = login::login_vm(&crate::auth::device_flow::AuthState::Idle, &settings);
        let mut form = login::LoginForm::for_settings(&settings);
        let h = harness(dark, move |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                login::render(ui, &vm, &mut form, &mut Vec::new(), None);
            });
        });
        save(h, &format!("login-{t}"));

        let mut win = SettingsWindowState::default();
        win.open_with(&settings);
        let scopes: Vec<String> = [
            "admin:org",
            "gist",
            "notifications",
            "project",
            "repo",
            "read:org",
            "user",
            "workflow",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut list = pr_list_state();
        let mut form = pr_list::PrListForm::default();
        let settings_for_shot = settings.clone();
        let h = harness(dark, move |ui| {
            pr_list::render_title_bar(ui, &mut Vec::new(), &mut Vec::new());
            egui::CentralPanel::default().show(ui, |ui| {
                pr_list::render(
                    ui,
                    &mut list,
                    &mut form,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    "2026-10-20T00:00:00Z",
                );
            });
            settings_window::render(
                ui.ctx(),
                &mut win,
                Some(AccountInfo {
                    login: "sercand",
                    host: "github.com",
                    scopes: &scopes,
                }),
                &settings_for_shot.ai,
                &std::collections::BTreeMap::new(),
                &mut Vec::new(),
            );
        });
        save(h, &format!("settings-{t}"));

        // The AI tab's providers table and provider dialog (spec §10, Task 5),
        // rendered only on Unix like the feature.
        #[cfg(unix)]
        {
            let ai_settings = ai_shot_settings();
            let keys = ai_shot_keys();
            let scopes_ai: Vec<String> =
                ["repo", "read:org"].iter().map(|s| s.to_string()).collect();

            let mut providers = SettingsWindowState::default();
            providers.open_with(&ai_settings);
            providers.tab = SettingsTab::Ai;
            providers
                .ai_tab
                .test_results
                .insert("work".to_owned(), Ok("claude-sonnet-5-5".to_owned()));
            let mut list = pr_list_state();
            let mut form = pr_list::PrListForm::default();
            let settings_for_shot = ai_settings.clone();
            let keys_for_shot = keys.clone();
            let scopes_for_dialog = scopes_ai.clone();
            let scopes_for_models = scopes_ai.clone();
            let scopes_for_model_dialog = scopes_ai.clone();
            let h = harness_sized(dark, SIZE, move |ui| {
                pr_list::render_title_bar(ui, &mut Vec::new(), &mut Vec::new());
                egui::CentralPanel::default().show(ui, |ui| {
                    pr_list::render(
                        ui,
                        &mut list,
                        &mut form,
                        &mut Vec::new(),
                        &mut Vec::new(),
                        "2026-10-20T00:00:00Z",
                    );
                });
                settings_window::render(
                    ui.ctx(),
                    &mut providers,
                    Some(AccountInfo {
                        login: "sercand",
                        host: "github.com",
                        scopes: &scopes_ai,
                    }),
                    &settings_for_shot.ai,
                    &keys_for_shot,
                    &mut Vec::new(),
                );
            });
            save_after(h, &format!("settings-ai-providers-{t}"), 8);

            let mut dialog = SettingsWindowState::default();
            dialog.open_with(&ai_settings);
            dialog.tab = SettingsTab::Ai;
            dialog
                .ai_tab
                .test_results
                .insert("work".to_owned(), Ok("claude-sonnet-5-5".to_owned()));
            if let Some(provider) = ai_settings.ai.providers.first() {
                dialog.ai_tab.dialog = Some(ProviderDialog::for_provider(provider));
            }
            let mut list = pr_list_state();
            let mut form = pr_list::PrListForm::default();
            let settings_for_shot = ai_settings.clone();
            let keys_for_shot = keys;
            let h = harness_sized(dark, SIZE, move |ui| {
                pr_list::render_title_bar(ui, &mut Vec::new(), &mut Vec::new());
                egui::CentralPanel::default().show(ui, |ui| {
                    pr_list::render(
                        ui,
                        &mut list,
                        &mut form,
                        &mut Vec::new(),
                        &mut Vec::new(),
                        "2026-10-20T00:00:00Z",
                    );
                });
                settings_window::render(
                    ui.ctx(),
                    &mut dialog,
                    Some(AccountInfo {
                        login: "sercand",
                        host: "github.com",
                        scopes: &scopes_for_dialog,
                    }),
                    &settings_for_shot.ai,
                    &keys_for_shot,
                    &mut Vec::new(),
                );
            });
            save_after(h, &format!("settings-ai-provider-dialog-{t}"), 8);

            // The Models table below the providers (spec §10, Task 6): two
            // fixed roles, both filled like the approved mockup.
            let mut models = SettingsWindowState::default();
            models.open_with(&ai_settings);
            models.tab = SettingsTab::Ai;
            models
                .ai_tab
                .test_results
                .insert("work".to_owned(), Ok("claude-sonnet-5-5".to_owned()));
            let mut list = pr_list_state();
            let mut form = pr_list::PrListForm::default();
            let settings_for_shot = ai_settings.clone();
            let keys_for_shot = ai_shot_keys();
            let h = harness_sized(dark, SIZE, move |ui| {
                pr_list::render_title_bar(ui, &mut Vec::new(), &mut Vec::new());
                egui::CentralPanel::default().show(ui, |ui| {
                    pr_list::render(
                        ui,
                        &mut list,
                        &mut form,
                        &mut Vec::new(),
                        &mut Vec::new(),
                        "2026-10-20T00:00:00Z",
                    );
                });
                settings_window::render(
                    ui.ctx(),
                    &mut models,
                    Some(AccountInfo {
                        login: "sercand",
                        host: "github.com",
                        scopes: &scopes_for_models,
                    }),
                    &settings_for_shot.ai,
                    &keys_for_shot,
                    &mut Vec::new(),
                );
            });
            save_after(h, &format!("settings-ai-models-{t}"), 8);

            // The model dialog over the Models table, editing the default role.
            let mut model_dialog = SettingsWindowState::default();
            model_dialog.open_with(&ai_settings);
            model_dialog.tab = SettingsTab::Ai;
            model_dialog.ai_tab.model_dialog =
                Some(ModelDialog::new(ModelRole::Default, &ai_settings.ai));
            let mut list = pr_list_state();
            let mut form = pr_list::PrListForm::default();
            let settings_for_shot = ai_settings.clone();
            let keys_for_shot = ai_shot_keys();
            let h = harness_sized(dark, SIZE, move |ui| {
                pr_list::render_title_bar(ui, &mut Vec::new(), &mut Vec::new());
                egui::CentralPanel::default().show(ui, |ui| {
                    pr_list::render(
                        ui,
                        &mut list,
                        &mut form,
                        &mut Vec::new(),
                        &mut Vec::new(),
                        "2026-10-20T00:00:00Z",
                    );
                });
                settings_window::render(
                    ui.ctx(),
                    &mut model_dialog,
                    Some(AccountInfo {
                        login: "sercand",
                        host: "github.com",
                        scopes: &scopes_for_model_dialog,
                    }),
                    &settings_for_shot.ai,
                    &keys_for_shot,
                    &mut Vec::new(),
                );
            });
            save_after(h, &format!("settings-ai-model-dialog-{t}"), 8);
        }
    }
}

/// Screen-level guard for the narrow-pane layout bug: the right pane feeds
/// its own response rect back as the next frame's `default_size`, so an AI
/// tab whose min content width exceeds the splitter width grows the pane a
/// little every frame until the diff is crushed. At a 420 pt pane the tab's
/// long tool JSON and approval commands must keep the pane at 420.
///
/// Not `#[ignore]`d: this is a layout assertion, not a screenshot, and the
/// software harness needs no GPU.
#[cfg(unix)]
#[test]
fn ai_tab_keeps_the_right_pane_at_its_splitter_width() {
    let mut s = session(DiffMode::Split);
    s.ai = ai_state_running();
    let mut state = pr_detail::PrDetailState {
        right_open: true,
        right_pane: 420.0,
        ..Default::default()
    };
    state.details_tab = crate::ui::widgets::metadata_panel::DetailsTab::Ai;
    let mut cache = HighlightCache::new(512, Box::new(SyntectHighlighter::new(true)));
    let seen = std::rc::Rc::new(std::cell::RefCell::new(420.0));
    let w = seen.clone();
    let mut harness = Harness::builder()
        .with_size(SIZE)
        .with_theme(egui::Theme::Dark)
        .with_context_setup(|ctx| {
            ctx.set_fonts(fonts::font_definitions());
            egui_extras::install_image_loaders(ctx);
            theme::install(ctx, 13.0);
        })
        .with_accessibility_check(false)
        .build_ui(move |ui| {
            let mut diff_actions = Vec::new();
            pr_detail::render(
                ui,
                &mut s,
                &mut state,
                &mut cache,
                &mut diff_actions,
                pr_detail::ScreenInputs {
                    login: "sercand",
                    write_blocked: None,
                    theme: theme::Theme::Dark,
                    system_dark: true,
                    ai_configured: true,
                    #[cfg(unix)]
                    local_checkout: None,
                    #[cfg(unix)]
                    repo_ai: None,
                },
            );
            *w.borrow_mut() = state.right_pane;
        });
    // The growth compounds per frame; ten frames grew the pane ~360 pt before
    // the fix.
    harness.run_steps(10);
    let width = *seen.borrow();
    assert!(
        width <= 421.0,
        "the right pane grew to {width:.0} pt past its 420 pt splitter width"
    );
}

/// The settings the AI shots open with: three providers (two with a stored key
/// in [`ai_shot_keys`]) and both model roles, like the approved mockup.
#[cfg(unix)]
fn ai_shot_settings() -> Settings {
    use crate::persist::settings::{LlmApi, ModelSettings, ProviderSettings, ReasoningLevel};
    let mut settings = Settings::default();
    settings.ai.providers = vec![
        ProviderSettings {
            name: "work".into(),
            api: LlmApi::AnthropicMessages,
            base_url: String::new(),
            headers: Default::default(),
        },
        ProviderSettings {
            name: "openai-eu".into(),
            api: LlmApi::OpenaiResponses,
            base_url: "api.openai.com".into(),
            headers: Default::default(),
        },
        ProviderSettings {
            name: "local".into(),
            api: LlmApi::OpenaiResponses,
            base_url: "localhost:11434".into(),
            headers: Default::default(),
        },
    ];
    settings.ai.models.default = Some(ModelSettings {
        provider: "work".into(),
        model: "claude-sonnet-5-5".into(),
        context_window: 200_000,
        max_tokens: 16_384,
        reasoning: Some(ReasoningLevel::Medium),
    });
    settings.ai.models.lookup = Some(ModelSettings {
        provider: "work".into(),
        model: "claude-haiku-5-5".into(),
        context_window: 200_000,
        max_tokens: 8_192,
        reasoning: None,
    });
    settings
}

/// The keychain presence the AI shots show: `work` and `openai-eu` have a
/// key, `local` does not (mockup: "✓ stored" / "no key").
#[cfg(unix)]
fn ai_shot_keys() -> std::collections::BTreeMap<String, bool> {
    [
        ("work".to_owned(), true),
        ("openai-eu".to_owned(), true),
        ("local".to_owned(), false),
    ]
    .into_iter()
    .collect()
}

/// The Local repos tab (AI settings Task 9) in its four shapes: rows
/// collapsed, one row expanded over its PRs, nothing linked, and a linked
/// folder that has gone missing (its PRs still listed).
#[cfg(unix)]
fn local_repos_shot(dark: bool, name: &str, expanded: bool, empty: bool, missing_first: bool) {
    use crate::persist::settings::{AiSettings, RepoAiSettings};
    use crate::state::local_repos::{ListTab, LocalReposState, RepoPrs};
    use crate::ui::widgets::local_repos;

    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let gone = std::path::PathBuf::from("/Users/sercand/Developer/acme/infra");
    let ai = if empty {
        AiSettings::default()
    } else {
        AiSettings {
            repos: vec![
                RepoAiSettings {
                    repo: "github.com/acme/widgets".to_string(),
                    path: if missing_first {
                        gone.clone()
                    } else {
                        manifest.clone()
                    },
                    ..Default::default()
                },
                RepoAiSettings {
                    repo: "github.com/acme/api".to_string(),
                    path: manifest,
                    ..Default::default()
                },
                RepoAiSettings {
                    repo: "github.com/acme/infra".to_string(),
                    path: gone,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    };
    let mut state = LocalReposState::default();
    if !empty {
        let pr = |number: u64, title: &str| {
            let mut s = pr_summary(number, title, 0);
            s.repo = crate::model::RepoRef::new("acme", "widgets");
            s
        };
        state.repos.insert(
            "github.com/acme/widgets".to_string(),
            RepoPrs::Loaded {
                total: 3,
                items: vec![
                    pr(412, "Fix flaky retry in uploader"),
                    pr(409, "Add CSV export"),
                    pr(401, "Bump deps"),
                ],
            },
        );
        state.repos.insert(
            "github.com/acme/api".to_string(),
            RepoPrs::Loaded {
                total: 7,
                items: Vec::new(),
            },
        );
        if expanded {
            state.expanded.insert("github.com/acme/widgets".to_string());
        }
    }
    let blocked = |_: &str| None;
    let h = harness(dark, move |ui| {
        pr_list::render_title_bar(ui, &mut Vec::new(), &mut Vec::new());
        egui::CentralPanel::default().show(ui, |ui| {
            pr_list::render_tabs(ui, ListTab::Local, &mut Vec::new(), &mut Vec::new());
            local_repos::render(
                ui,
                &ai,
                "github.com",
                &state,
                &blocked,
                "2026-10-20T00:00:00Z",
                &mut Vec::new(),
            );
        });
    });
    save(h, name);
}
