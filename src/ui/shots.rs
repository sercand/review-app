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
use crate::ui::widgets::settings_window::{self, AccountInfo, SettingsWindowState};
use crate::ui::{fonts, theme};

const SIZE: [f32; 2] = [1440.0, 900.0];

/// A harness set up the way `ReviewApp::new` sets up the real context.
fn harness<'a>(dark: bool, app: impl FnMut(&mut egui::Ui) + 'a) -> Harness<'a> {
    Harness::builder()
        .with_size(SIZE)
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
fn save(mut h: Harness<'_>, name: &str) {
    h.run_steps(4);
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
        ("Performance: fix long-session memory leaks and top bar, gallery and loading costs", "otsimo/kedis-studio", 2298, "Cem-Celik", Some(ReviewDecision::ReviewRequired), false, Some(CiState::Success), 142, 57, 0, "2026-10-19T12:52:48Z"),
        ("Gallery: stop pixel posters from showing up twice after a push notification", "otsimo/kedis-studio", 2300, "Cem-Celik", Some(ReviewDecision::ReviewRequired), false, Some(CiState::Pending), 3, 0, 0, "2026-10-19T12:47:52Z"),
        ("Flux3 klein", "sercand/shiso-llm", 44, "sercand", None, false, Some(CiState::Success), 13170, 184, 0, "2026-10-18T13:26:30Z"),
        ("qwen4exp: serve NVIDIA's Qwen3.8-Flash-Next FP4-Local (MXFP8/NVFP4/CSF), FP8 KV, draft-head fixes", "sercand/shiso-llm", 43, "sercand", None, false, Some(CiState::Failure), 34429, 1535, 0, "2026-10-18T08:03:58Z"),
        ("Upgrade Piknik v5", "otsimo/kedis-studio", 2297, "gozdecoban", Some(ReviewDecision::ReviewRequired), false, Some(CiState::Success), 47, 336, 0, "2026-10-16T14:36:51Z"),
        ("Add Sago DevUI debug menu improvements bundle", "otsimo/kedis-studio", 1902, "egealtar", Some(ReviewDecision::ChangesRequested), false, Some(CiState::Success), 1381, 0, 11, "2026-08-25T09:14:16Z"),
        ("Add Edit Mode unit tests for Storage, App, and Ranking (149 tests)", "otsimo/kedis-studio", 1731, "tayfunotsimo", Some(ReviewDecision::ChangesRequested), true, None, 3518, 37, 10, "2026-07-10T07:50:21Z"),
        ("Resolve: \"Webstore: Adding premium-status-change and billing history support\"", "otsimo/mono", 1218, "sinantomruk", Some(ReviewDecision::Approved), false, Some(CiState::Success), 813, 22, 5, "2023-10-06T07:53:53Z"),
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
                s.scopes = vec![if author == "sercand" {
                    "authored"
                } else {
                    "review-requested"
                }
                .into()];
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
            },
        );
    });
    save(h, name);
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
                &mut Vec::new(),
            );
        });
        save(h, &format!("settings-{t}"));
    }
}
