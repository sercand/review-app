# UI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the egui app in line with the HTML mockups in `docs/design/mockups/`: a token-based dark/light theme, IBM Plex Sans and JetBrains Mono, Lucide icons instead of text buttons, file-type icons, full-language syntax highlighting, a one-file-at-a-time diff with coloured split pairs and word highlights, a popover file filter, and restyled list, login, settings and palette screens.

**Architecture:** Visual decisions live in three pure modules: `ui::theme` (colour tokens → `egui::Visuals`), `ui::fonts` + `ui::icons` (bundled fonts and the Lucide codepoint table), and `ui::file_icons` (path → bundled SVG). Everything that decides *what* to draw stays a pure, unit-tested view-model function, as in the main plan. The biggest behaviour change is the diff surface: `RowModel` is built for **one selected file** (`build_one`), and split mode builds **paired rows** so a removed line and its replacement share one table row.

**Tech Stack:** Existing stack (`eframe`/`egui` 0.37 via `eframe::egui`, `egui_extras` with `svg`, `syntect` 5.3) plus `two-face` (bat's extended syntect syntaxes). Bundled assets: IBM Plex Sans and JetBrains Mono (OFL-1.1), `lucide.ttf` from `lucide-static@0.469.0` (ISC), and SVGs from `material-icon-theme@5.11.1` (MIT).

**Spec:** `docs/superpowers/specs/2026-10-19-gh-review-desktop-design.md` (behaviour) and `docs/design/mockups/` (`styles.css`, `login.html`, `prs.html`, `pr.html`, `pr.js`): the visual source of truth. Open the mockups at `python3 -m http.server -d docs/design/mockups 8765` while working. `pr.html?file=<path>&view=unified` and `#settings`/`#palette` open specific states.

**Prerequisite:** Tasks 1–54 of `docs/superpowers/plans/2026-10-19-gh-review-desktop.md` are complete. This plan restyles and reworks what those tasks built: Task 49's image viewer, Task 50's checks and commits, Task 51's metadata panel, Task 52's palette and `next_index`, and Task 53's `Theme`, `Layout` and settings window.

## Global Constraints

- Everything in the main plan's Global Constraints and Conventions still holds. That covers `eframe::egui` only, `git add <files>` (never `-am`), and `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` before every commit.
- Colours come only from `ui::theme::Palette`. No widget may write a hex literal or call `Color32::from_rgb` directly; hex values appear only in `theme.rs` and in the syntax theme built from it.
- Icons come only from `ui::icons` constants (Lucide codepoints rendered with the bundled font). Text-only buttons remain only where the mockup uses text: primary actions ("Sign in with GitHub", "Submit review", "Add to review", "Add single comment", "Load diff", "Viewed").
- Every icon-only button has a tooltip (`on_hover_text`) naming the action and its shortcut in the mockup's format, e.g. `"Split  S"`, `"Find in file  ⌘F"`.
- Fonts and SVGs are bundled with `include_bytes!`/`include_image!` from `assets/`. The app makes no network request for UI assets, and each asset folder carries its upstream licence file.
- Sizes: base UI text 13 pt, small text 11.5 pt, code 12.5 pt (Settings can switch it to 12, 13 or 14), row height `ROW_H` stays 20.0, icon buttons are 28×28, icon glyphs 16 pt, corner radius 4 (small), 6 (controls), 10 (popovers, windows, threads).
- Copy is sentence case and matches the mockup strings quoted in each task exactly.
- The window's minimum inner size is 900×600 (`ViewportBuilder::with_min_inner_size`).

## Review Focus

1. **Switching files with the composer open.** Text typed for file A must still be there when the user returns to A, and must never appear under file B (spec S4). Test in Task 5.
2. **Changing theme mid-session.** Highlighted lines must not keep the previous theme's colours from the LRU. Test in Task 4.
3. **Empty or fully filtered PRs.** A PR with zero files, or with every file filtered out, shows an empty state, and `j`/`k`/"Viewed, next" do nothing instead of panicking. The current file stays on screen when a filter hides it. Test in Task 5.
4. **Word highlight on non-ASCII text.** Ranges must fall on `char` boundaries (e.g. `naïve`); a bad slice panics the UI thread. Test in Task 6.
5. **One huge file.** In single-file mode a 50,000-line or minified file with wrap on must keep rebuild and height work inside the frame budget. Test in Task 17.

---

## Phase R1 — Foundations

### Task 1: Colour tokens and egui visuals

**Files:**
- Modify: `src/ui/theme.rs` (Task 53 created it with `Theme`)
- Modify: `src/app.rs` (apply visuals when the resolved theme changes)
- Test: `src/ui/theme.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: main-plan Task 53 `Theme`, `Theme::resolve`.
- Produces:
  - `pub struct Palette { pub bg, surface, raised, hover, line, line_soft, text, muted, faint, accent, accent_ink, accent_soft, pending, pending_soft, ok, warn, bad, info, add_bg, add_gutter, add_word, del_bg, del_gutter, del_word, hunk_bg, filler, syn_kw, syn_str, syn_fn, syn_type, syn_num, syn_com, syn_attr: egui::Color32 }` (every field `pub`).
  - `pub const DARK: Palette` and `pub const LIGHT: Palette`, with values copied from `docs/design/mockups/styles.css` (`:root` for dark, `:root[data-theme="light"]` for light). `rgba(r,g,b,a)` becomes `Color32::from_rgba_unmultiplied(r,g,b,round(a*255))`. Exception: `filler` is a flat fill, `from_white_alpha(6)` in dark and `from_black_alpha(9)` in light (egui has no CSS hatch).
  - `pub fn palette(dark: bool) -> &'static Palette` and `pub fn palette_of(ui: &egui::Ui) -> &'static Palette` (reads `ui.visuals().dark_mode`).
  - `pub fn visuals(p: &Palette, dark: bool) -> egui::Visuals` with:
    - Fills: `panel_fill = bg`, `window_fill = surface`, `extreme_bg_color = bg`, `faint_bg_color = surface`.
    - Widgets: `widgets.inactive.weak_bg_fill = raised`, `widgets.hovered.weak_bg_fill = hover`, `widgets.active.weak_bg_fill = hover`, `noninteractive.bg_stroke = (1.0, line)`, and widget corner radius 6.
    - Selection and accents: `selection.bg_fill = accent_soft`, `selection.stroke = (1.0, accent)`.
    - Text and status colours: `hyperlink_color = accent`, `warn_fg_color = pending`, `error_fg_color = bad`, `override_text_color = None`, `widgets.noninteractive.fg_stroke.color = text`.
    - Windows: `window_corner_radius = 10`, and `window_shadow` with offset `[0, 10]`, blur 30 and `Color32::from_black_alpha(115)`.
  - `pub fn style(base: egui::Style) -> egui::Style`: `item_spacing = (6, 4)`, `button_padding = (10, 4)`, `interact_size.y = 28`. Text styles: `Body` 13, `Button` 13, `Small` 11.5, `Monospace` 12.5, `Heading` 15.
  - `app.rs`: each frame, compute `dark = settings.theme.resolve(ctx.system_theme() == Some(egui::Theme::Dark)) == egui::Theme::Dark`. When it differs from the last applied value, call `ctx.set_visuals(visuals(palette(dark), dark))`, call `ctx.set_style(style(..))`, and call `HighlightCache::set_theme(dark)` (Task 4).

- [ ] **Step 1: Write the failing tests**

```rust
fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
    fn lum(c: egui::Color32) -> f32 {
        let f = |v: u8| { let v = v as f32 / 255.0; if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[test]
fn tokens_match_the_mockup() {
    assert_eq!(DARK.bg, egui::Color32::from_hex("#1a1d23").unwrap());
    assert_eq!(DARK.accent, egui::Color32::from_hex("#7aa2f7").unwrap());
    assert_eq!(LIGHT.bg, egui::Color32::from_hex("#f6f7f9").unwrap());
    assert_eq!(LIGHT.accent, egui::Color32::from_hex("#3b63d6").unwrap());
    assert_eq!(DARK.add_bg, egui::Color32::from_rgba_unmultiplied(63, 185, 80, 33));
}

#[test]
fn text_tokens_are_readable_in_both_themes() {
    for p in [&DARK, &LIGHT] {
        assert!(contrast(p.text, p.bg) >= 7.0);
        assert!(contrast(p.muted, p.bg) >= 4.5);
        assert!(contrast(p.muted, p.surface) >= 4.5);
        assert!(contrast(p.accent_ink, p.accent) >= 4.5);
    }
}

#[test]
fn visuals_follow_the_palette() {
    let v = visuals(&DARK, true);
    assert!(v.dark_mode);
    assert_eq!(v.panel_fill, DARK.bg);
    assert_eq!(v.selection.bg_fill, DARK.accent_soft);
    assert_eq!(v.hyperlink_color, DARK.accent);
    assert!(!visuals(&LIGHT, false).dark_mode);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test tokens_match_the_mockup text_tokens_are_readable_in_both_themes visuals_follow_the_palette`. Expected: FAIL to compile (`DARK` not found).

- [ ] **Step 3: Implement** the palettes, `palette`, `palette_of`, `visuals`, `style`, and the per-frame application in `app.rs`. Replace every `ui.visuals().warn_fg_color` or ad-hoc colour in existing widgets with `palette_of(ui)` fields in the tasks that restyle them, not here.

- [ ] **Step 4: Run tests to verify they pass** — same command. Expected: PASS.

- [ ] **Step 5: Manual QA** — run the app, switch the theme in Settings, and check that the panel, window, and text-field backgrounds match `prs.html` in both themes.

- [ ] **Step 6: Commit** — `git add src/ui/theme.rs src/app.rs && git commit -m "feat: add design tokens and themed visuals"`.

---

### Task 2: Bundled fonts and the Lucide icon font

**Files:**
- Create: `assets/fonts/IBMPlexSans-Regular.ttf`, `assets/fonts/IBMPlexSans-SemiBold.ttf`, `assets/fonts/JetBrainsMono-Regular.ttf`, `assets/fonts/JetBrainsMono-Bold.ttf`, `assets/fonts/OFL.txt`, `assets/icons/lucide.ttf`, `assets/icons/LICENSE` (Lucide ISC)
- Create: `src/ui/fonts.rs`, `src/ui/icons.rs`
- Modify: `src/ui/mod.rs`, `src/app.rs` (`ReviewApp::new` calls `cc.egui_ctx.set_fonts(fonts::font_definitions())`; `run` sets `with_min_inner_size([900.0, 600.0])`)
- Test: `src/ui/fonts.rs`, `src/ui/icons.rs` (`#[cfg(test)]`)

Download sources: IBM Plex Sans from `https://github.com/IBM/plex` releases (`IBM-Plex-Sans` TTF), JetBrains Mono from `https://github.com/JetBrains/JetBrainsMono` releases (`fonts/ttf`), and `https://cdn.jsdelivr.net/npm/lucide-static@0.469.0/font/lucide.ttf`.

**Interfaces:**
- Produces:
  - `pub fn font_definitions() -> egui::FontDefinitions`, with these families:
    - `Proportional` = `["plex", "lucide"]`
    - `Monospace` = `["jetbrains", "lucide"]`
    - `FontFamily::Name("plex-semibold")` = `["plex-semibold", "lucide"]`
    - `FontFamily::Name("mono-bold")` = `["jetbrains-bold", "lucide"]`
  - `pub fn semibold(size: f32) -> egui::FontId` and `pub fn mono_bold(size: f32) -> egui::FontId`: egui has no font weights, so bold text uses these named families.
  - `src/ui/icons.rs`: one `pub const NAME: char` per icon, exactly this table (Lucide 0.469.0 codepoints):

```rust
pub const ARROW_DOWN_WIDE_NARROW: char = '\u{e04b}'; pub const ARROW_LEFT: char = '\u{e04c}';
pub const ARROW_RIGHT: char = '\u{e04d}';   pub const AT_SIGN: char = '\u{e052}';
pub const BOLD: char = '\u{e061}';          pub const BOOK_MARKED: char = '\u{e3f5}';
pub const BUILDING_2: char = '\u{e28f}';    pub const CHECK: char = '\u{e070}';
pub const CHECK_CHECK: char = '\u{e391}';   pub const CHEVRON_DOWN: char = '\u{e071}';
pub const CHEVRON_LEFT: char = '\u{e072}';  pub const CHEVRON_RIGHT: char = '\u{e073}';
pub const CHEVRON_UP: char = '\u{e074}';    pub const CHEVRONS_DOWN: char = '\u{e075}';
pub const CHEVRONS_DOWN_UP: char = '\u{e227}'; pub const CHEVRONS_UP: char = '\u{e078}';
pub const CHEVRONS_UP_DOWN: char = '\u{e210}'; pub const CIRCLE: char = '\u{e07a}';
pub const CIRCLE_CHECK: char = '\u{e225}';  pub const CIRCLE_DOT: char = '\u{e348}';
pub const CIRCLE_DOT_DASHED: char = '\u{e4b5}'; pub const CIRCLE_X: char = '\u{e088}';
pub const CLOCK: char = '\u{e08b}';         pub const CLOUD: char = '\u{e08c}';
pub const CODE: char = '\u{e097}';          pub const COLUMNS_2: char = '\u{e09c}';
pub const COMMAND: char = '\u{e09e}';       pub const COPY: char = '\u{e0a2}';
pub const DOWNLOAD: char = '\u{e0b6}';      pub const ELLIPSIS: char = '\u{e0ba}';
pub const EXTERNAL_LINK: char = '\u{e0bd}'; pub const EYE: char = '\u{e0be}';
pub const EYE_OFF: char = '\u{e0bf}';       pub const FILE_DIFF: char = '\u{e319}';
pub const FILE_TEXT: char = '\u{e0d0}';
pub const GIT_COMMIT_HORIZONTAL: char = '\u{e0e6}'; pub const GIT_PULL_REQUEST: char = '\u{e0e8}';
pub const GIT_PULL_REQUEST_DRAFT: char = '\u{e35e}'; pub const GITHUB: char = '\u{e0e9}';
pub const GLOBE: char = '\u{e0eb}';         pub const HARD_DRIVE: char = '\u{e0f0}';
pub const INBOX: char = '\u{e0fa}';         pub const INFO: char = '\u{e0fe}';
pub const KEY_ROUND: char = '\u{e4a7}';     pub const LAYERS: char = '\u{e52d}';
pub const LINK: char = '\u{e107}';          pub const LIST_FILTER: char = '\u{e464}';
pub const LOG_IN: char = '\u{e110}';        pub const LOG_OUT: char = '\u{e111}';
pub const MESSAGE_SQUARE: char = '\u{e11a}'; pub const MESSAGE_SQUARE_DASHED: char = '\u{e40f}';
pub const MESSAGE_SQUARE_PLUS: char = '\u{e410}'; pub const MESSAGES_SQUARE: char = '\u{e411}';
pub const MINUS: char = '\u{e11f}';         pub const MOON: char = '\u{e121}';
pub const PANEL_RIGHT: char = '\u{e435}';   pub const PENCIL: char = '\u{e1f8}';
pub const PENCIL_LINE: char = '\u{e4f4}';   pub const PILCROW: char = '\u{e3a6}';
pub const PLUS: char = '\u{e140}';          pub const REFRESH_CW: char = '\u{e148}';
pub const ROTATE_CCW: char = '\u{e14b}';    pub const ROWS_2: char = '\u{e43d}';
pub const SEARCH: char = '\u{e154}';        pub const SEND: char = '\u{e155}';
pub const SETTINGS: char = '\u{e157}';      pub const SQUARE: char = '\u{e16a}';
pub const SQUARE_CHECK: char = '\u{e55d}';  pub const SQUARE_SPLIT_HORIZONTAL: char = '\u{e3b9}';
pub const SUN: char = '\u{e17b}';           pub const TRASH_2: char = '\u{e18d}';
pub const UNFOLD_VERTICAL: char = '\u{e442}'; pub const USER_CHECK: char = '\u{e19f}';
pub const USERS: char = '\u{e1a3}';         pub const WRAP_TEXT: char = '\u{e247}';
pub const X: char = '\u{e1b1}';
```

  - `pub const ALL: &[char]`: every constant above, used by the glyph test.
  - Widgets in `icons.rs`:
    - `pub fn icon_button(ui: &mut egui::Ui, icon: char, tooltip: &str) -> egui::Response`: a 28×28 frameless button with the glyph at 16 pt in `muted`, `hover` fill and `text` colour on hover.
    - `pub fn icon_toggle(ui: &mut egui::Ui, icon: char, tooltip: &str, on: &mut bool) -> egui::Response`: like `icon_button`, drawn with `accent_soft` fill and `accent` glyph when `*on`; flips `*on` when clicked.
    - `pub fn icon_text(icon: char, text: &str) -> String`, returning `format!("{icon} {text}")` for labels and buttons that show both.
    - `pub fn badge(ui: &mut egui::Ui, response: &egui::Response, n: usize)`: paints the count dot (accent, 14 pt high) on the button's top-right corner when `n > 0`.

- [ ] **Step 1: Write the failing tests**

```rust
// src/ui/icons.rs
#[test]
fn icon_codepoints_are_unique_private_use() {
    let mut seen = std::collections::HashSet::new();
    for &c in ALL {
        assert!(('\u{e000}'..='\u{f8ff}').contains(&c), "{c:?} outside the private use area");
        assert!(seen.insert(c), "{c:?} listed twice");
    }
    assert_eq!(ALL.len(), 77);   // the 77 constants above
}

// src/ui/fonts.rs
#[test]
fn every_icon_and_text_family_has_glyphs() {
    let ctx = egui::Context::default();
    ctx.set_fonts(font_definitions());
    let _ = ctx.run(Default::default(), |_| {});
    ctx.fonts_mut(|f| {
        for &c in crate::ui::icons::ALL {
            assert!(f.has_glyph(&egui::FontId::proportional(16.0), c), "{c:?} missing");
            assert!(f.has_glyph(&egui::FontId::monospace(12.5), c));
        }
        assert!(f.has_glyph(&semibold(13.0), 'ï'));
        assert!(f.has_glyph(&egui::FontId::monospace(12.5), '−'));  // U+2212 used in −N counts
    });
}
```

(If egui 0.37 exposes `has_glyph` through `ctx.fonts(..)` rather than `fonts_mut`, use that; the assertion is the same.)

- [ ] **Step 2: Run tests to verify they fail** — `cargo test icon_codepoints_are_unique_private_use every_icon_and_text_family_has_glyphs`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the asset files, `font_definitions` (`FontData::from_static(include_bytes!(…))`), the icon table, and the three helpers. Install the fonts in `ReviewApp::new`, before the first frame.

- [ ] **Step 4: Run tests to verify they pass** — same command. Expected: PASS.

- [ ] **Step 5: Commit** — `git add assets/fonts assets/icons src/ui/fonts.rs src/ui/icons.rs src/ui/mod.rs src/app.rs && git commit -m "feat: bundle Plex, JetBrains Mono and Lucide icon fonts"`.

---

### Task 3: File-type and folder icons

**Files:**
- Create: `assets/file-icons/*.svg` (the names listed below, from `https://cdn.jsdelivr.net/npm/material-icon-theme@5.11.1/icons/<name>.svg`), `assets/file-icons/LICENSE` (MIT, from the same package)
- Create: `src/ui/file_icons.rs`
- Modify: `src/ui/mod.rs`
- Test: `src/ui/file_icons.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: main-plan Task 49 (`egui_extras` `svg` feature and `install_image_loaders`).
- Produces:
  - `pub fn file_icon_name(path: &str) -> &'static str`. Matching is case-insensitive on the file **name**, and the first rule that matches wins:
    1. **Whole names:**
       - `package.json`, `package-lock.json` → `nodejs`
       - `cargo.toml`, `cargo.lock` → `rust`
       - `dockerfile` → `docker`
       - `readme.md` → `readme`
       - `.gitignore`, `.gitattributes` → `git`
    2. **Test files:**
       - `*.test.ts`, `*.spec.ts` → `test-ts`
       - `*.test.js`, `*.spec.js`, `*.test.tsx`, `*.spec.tsx`, `*.test.jsx`, `*.spec.jsx` → `test-jsx` if the extension ends in `x`, else `test-js`
    3. **Extensions, languages:**
       - `ts` → `typescript`, `tsx` → `react_ts`
       - `js`/`mjs`/`cjs` → `javascript`, `jsx` → `react`
       - `rs` → `rust`, `go` → `go`, `py` → `python`, `java` → `java`, `kt` → `kotlin`, `swift` → `swift`, `cs` → `csharp`
       - `cpp`/`cc`/`hpp` → `cpp`, `c`/`h` → `c`
       - `lua` → `lua`, `svelte` → `svelte`, `vue` → `vue`, `proto` → `proto`
       - `sql` → `database`, `sh`/`bash`/`zsh` → `console`
    4. **Extensions, markup and config:**
       - `html` → `html`, `css` → `css`, `scss`/`sass` → `sass`
       - `json` → `json`, `yml`/`yaml` → `yaml`, `toml` → `settings`, `md` → `markdown`
    5. **Extensions, binary and assets:**
       - `png`/`jpg`/`jpeg`/`gif`/`webp`/`bmp`/`ico` → `image`, `svg` → `svg`
       - `lock` → `lock`
       - `ttf`/`otf`/`woff`/`woff2` → `font`
       - `pdf` → `pdf`, `zip`/`gz`/`tar` → `zip`
    6. Anything else → `file`.
  - `pub fn folder_icon_name(name: &str, open: bool) -> &'static str`: the **last** segment of a chain label (`"apps/metadata"` → `metadata`) selects the icon:
    - `src` → `folder-src`, `lib` → `folder-lib`, `components` → `folder-components`
    - `test`, `tests`, `__tests__` → `folder-test`
    - `docs` → `folder-docs`, `public` → `folder-public`, `scripts` → `folder-scripts`, `config` → `folder-config`
    - `images`, `img`, `assets` → `folder-images`, `.github` → `folder-github`
    - anything else → `folder`
    - `open` appends `-open`.
  - `pub const ALL_ICON_NAMES: &[&str]`: every name either function can return, open variants included.
  - `pub fn icon_source(name: &str) -> Option<egui::ImageSource<'static>>`: one `include_image!("../../assets/file-icons/<name>.svg")` arm per name.
  - `pub fn file_icon(ui: &mut egui::Ui, path: &str)` and `pub fn folder_icon(ui: &mut egui::Ui, name: &str, open: bool)`: draw a 16×16 `egui::Image`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn files_map_to_icons() {
    for (path, icon) in [
        ("src/lib/data-tools/push.ts", "typescript"), ("a/node-pane.tsx", "react_ts"),
        ("a/node-kinds.test.ts", "test-ts"), ("a/x.spec.tsx", "test-jsx"), ("x.test.js", "test-js"),
        ("Cargo.toml", "rust"), ("web/package-lock.json", "nodejs"), ("README.md", "readme"),
        ("docs/guide.md", "markdown"), ("public/icons/push-badge.PNG", "image"),
        ("fonts/badge-glyphs.woff2", "font"), ("config.toml", "settings"), ("db/q.sql", "database"),
        ("Makefile", "file"), ("no_extension", "file"),
    ] {
        assert_eq!(file_icon_name(path), icon, "{path}");
    }
}

#[test]
fn folders_map_to_icons() {
    assert_eq!(folder_icon_name("src", false), "folder-src");
    assert_eq!(folder_icon_name("apps/metadata", true), "folder-open");
    assert_eq!(folder_icon_name("__tests__", true), "folder-test-open");
    assert_eq!(folder_icon_name("components", true), "folder-components-open");
}

#[test]
fn every_icon_name_is_bundled() {
    for name in ALL_ICON_NAMES {
        assert!(icon_source(name).is_some(), "{name} has no include_image arm");
        let path = format!("{}/assets/file-icons/{name}.svg", env!("CARGO_MANIFEST_DIR"));
        assert!(std::path::Path::new(&path).exists(), "{path} missing");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test files_map_to_icons folders_map_to_icons every_icon_name_is_bundled`. Expected: FAIL to compile.

- [ ] **Step 3: Download the SVGs and implement** the mapping functions, `ALL_ICON_NAMES`, `icon_source`, and the two draw helpers.

- [ ] **Step 4: Run tests to verify they pass** — same command. Expected: PASS.

- [ ] **Step 5: Commit** — `git add assets/file-icons src/ui/file_icons.rs src/ui/mod.rs && git commit -m "feat: add file-type and folder icons"`.

---

### Task 4: Highlighting for every common language, in the theme's colours

syntect's built-in syntax set has no TypeScript, TSX, Kotlin, Swift, TOML or Dockerfile. That is why only some files highlight today. `two-face` ships bat's extended set for the same `syntect` version.

**Files:**
- Modify: `Cargo.toml` (`cargo add two-face@0.5 --no-default-features` plus the feature that selects syntect's fancy-regex backend, `syntect-fancy` in 0.5; afterwards `cargo tree -i onig` must print nothing)
- Modify: `src/diff/highlight.rs`, `src/app.rs`
- Test: `src/diff/highlight.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 1 `Palette`, `palette`.
- Produces:
  - `SyntectHighlighter::new(dark: bool) -> SyntectHighlighter` (replaces the no-argument constructor; `Default` uses `dark = true`). It loads `two_face::syntax::extra_newlines()` and `review_theme(palette(dark))`.
  - `pub fn review_theme(p: &Palette) -> syntect::highlighting::Theme`, built in code. The foreground is `p.text` and the background is `p.bg`. Its scope rules, in this order:
    - `comment` → `syn_com`, italic
    - `string, constant.character, string.regexp` → `syn_str`
    - `keyword, storage, storage.type, storage.modifier, constant.language, variable.language` → `syn_kw`
    - `entity.name.function, support.function, meta.function-call variable.function` → `syn_fn`
    - `entity.name.type, entity.name.class, support.type, support.class, entity.other.inherited-class` → `syn_type`
    - `constant.numeric` → `syn_num`
    - `entity.other.attribute-name, variable.other.property, meta.object-literal.key, support.type.property-name` → `syn_attr`
  - `syntax_for_path(path)` returns the lowercased extension, or the lowercased file name when there is none (`"Dockerfile"` → `"dockerfile"`). `SyntectHighlighter::highlight` tries `find_syntax_by_extension`, then `find_syntax_by_token`, then plain text.
  - `HighlightCache::set_theme(&mut self, dark: bool)`: when `dark` differs from the current value, swap in `SyntectHighlighter::new(dark)` and **clear** every cached span. `HighlightCache::new` takes the highlighter as today and starts with `dark = true`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn common_languages_resolve_to_a_real_syntax() {
    let h = SyntectHighlighter::new(true);
    for path in ["a.ts", "a.tsx", "a.js", "a.jsx", "a.rs", "a.go", "a.py", "a.java", "a.kt", "a.swift",
                 "a.cs", "a.cpp", "a.json", "a.yaml", "a.toml", "a.md", "a.sh", "a.sql", "a.css",
                 "a.html", "a.vue", "a.svelte", "Dockerfile"] {
        let spans = h.highlight(&syntax_for_path(path), "const x = 1; // note");
        let colours: std::collections::HashSet<_> = spans.iter().map(|s| s.color).collect();
        assert!(colours.len() > 1, "{path} highlighted as plain text");
    }
}

#[test]
fn typescript_keywords_use_the_palette() {
    let h = SyntectHighlighter::new(true);
    let spans = h.highlight("ts", "const prefilled = await withCreateDefaults(ctx);");
    let kw = DARK.syn_kw;
    assert_eq!(spans[0].color, [kw.r(), kw.g(), kw.b()]);            // `const`
}

#[test]
fn switching_theme_clears_cached_colours() {
    let mut cache = HighlightCache::new(64, Box::new(SyntectHighlighter::new(true)));
    let dark = cache.spans("ts", "const a = 1;")[0].color;
    cache.set_theme(false);
    assert!(cache.is_empty());
    let light = cache.spans("ts", "const a = 1;")[0].color;
    assert_ne!(dark, light);
    cache.set_theme(false);                                            // no change → keeps entries
    assert_eq!(cache.len(), 1);
}
```

If one of the listed languages turns out not to be in `two-face` 0.5's extended set, drop it from the test and say so in the commit message. Do not swap in a different crate.

- [ ] **Step 2: Run tests to verify they fail** — `cargo test common_languages_resolve_to_a_real_syntax typescript_keywords_use_the_palette switching_theme_clears_cached_colours`. Expected: FAIL (TypeScript resolves to plain text; `set_theme` missing).

- [ ] **Step 3: Implement** the dependency, `review_theme`, the constructor change, the lookup fallback, and `set_theme`. Update existing callers (`app.rs`, tests) to `SyntectHighlighter::new(true)`.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test highlight`. Expected: PASS, including the existing cache tests.

- [ ] **Step 5: Commit** — `git add Cargo.toml Cargo.lock src/diff/highlight.rs src/app.rs && git commit -m "feat: highlight all common languages with the theme palette"`.

---

## Phase R2 — The diff surface

### Task 5: One file at a time

Selecting a file shows only that file. This replaces "scroll the whole PR to the file" (spec §9.2) and removes the cost of building, annotating and measuring every file's rows on large PRs.

**Files:**
- Modify: `src/diff/row_model.rs`, `src/state/pr_detail.rs`, `src/state/filters.rs` (`file_order`), `src/test_support.rs`, `src/ui/screens/pr_detail.rs`, `src/ui/widgets/diff_view.rs`, `src/app.rs` (the `j`/`k` dispatch from main-plan Task 52)
- Test: `src/diff/row_model.rs`, `src/state/pr_detail.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: main-plan Task 21 (`build`, `RowModel`), Task 37 (`annotate`, `inline_items`), Task 49 (`Command::LoadImages`), Task 52 (`next_index`).
- Produces:
  - `pub fn build_one(files: &[LoadedFile], file: u32) -> RowModel`. It returns the rows of `files[file]` only: the placeholder or the gaps and lines, with **no `FileHeader` row**, because the header is drawn above the table in Task 7. `file_ranges.len() == files.len()`; every range is empty except `file_ranges[file] == 0..rows.len()`. An out-of-range `file` gives `RowModel::empty()`.
  - `PrSession` gains `pub current_file: Option<u32>`. `rebuild_rows` builds with `build_one` for `current_file`, or `RowModel::empty()` when it is `None`. `inline_items` keeps only items whose file is `current_file`. The composer stays in `self.composer` whatever file is shown.
  - `PrSession::select_file(&mut self, path: &str, now: Instant) -> Vec<Command>`:
    - Sets `current_file` and `last_opened_file`, calls `rebuild_rows`, and marks the session dirty.
    - Returns `Command::LoadImages { session, file }` the first time an image file is selected; this replaces Task 49's "first visible header" trigger.
    - An unknown path is a no-op that returns an empty vec.
  - After `PrFilesPage { done: true }`, when `current_file` is `None`, select `last_opened_file` if it still exists, otherwise the first file in the tree's filtered order (`file_order` below).
  - `pub fn file_order(files: &[PrFile], filters: &FilterState, viewed: &ViewedSet) -> Vec<u32>`, in `src/state/filters.rs`: file indices in the tree's display order (directories first, then by path), over the files that pass the filters. `j`/`k` and the footer use `next_index(&file_order(..), current_file, forward)`.
  - Removed: `DiffViewState::scroll_to`, `sticky_file` and the sticky-header band, and `DiffAction::ScrollToFile`. Added: `DiffAction::SelectFile(u32)`. `TreeAction::SelectFile(path)` now calls `select_file`. Find (main-plan Task 48) searches only the current file's rows.
  - Empty states, drawn in the centre pane with the Task 7 placeholder style:
    - No files: "This pull request has no changed files."
    - Every file filtered out, with no current file: "No files match these filters." plus a "Clear filters" button.
    - When a filter hides the current file, it stays displayed; the tree simply has no selected row.

- [ ] **Step 1: Write the failing tests**

```rust
// src/diff/row_model.rs
#[test]
fn build_one_holds_only_the_selected_file() {
    let files = vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n"), loaded("b.rs", "@@ -5,2 +5,2 @@\n x\n-y\n+z\n")];
    let m = build_one(&files, 1);
    assert!(m.rows.iter().all(|r| !matches!(r, RowRef::FileHeader { .. })));
    assert!(m.rows.iter().all(|r| match r { RowRef::Line { file, .. } | RowRef::Gap { file, .. } => *file == 1, _ => true }));
    assert_eq!(m.file_ranges.len(), 2);
    assert!(m.file_ranges[0].is_empty());
    assert_eq!(m.file_ranges[1], 0..m.rows.len());
    assert!(build_one(&files, 9).rows.is_empty());
}

// src/state/pr_detail.rs
#[test]
fn selecting_a_file_rebuilds_rows_and_keeps_the_composer() {
    let mut s = session_with(&[("a.rs", "@@ -1 +1 @@\n-a\n+b\n"), ("b.rs", "@@ -1 +1 @@\n-c\n+d\n")]);
    let now = Instant::now();
    assert_eq!(s.current_file, Some(0));                              // first in tree order
    s.open_comment(CommentTarget::Line(line_anchor("a.rs", 1)));
    s.composer.text = "half written".into();
    s.select_file("b.rs", now);
    assert_eq!(s.current_file, Some(1));
    assert!(!s.row_model.rows.contains(&RowRef::Composer));           // composer belongs to a.rs
    s.select_file("a.rs", now);
    assert!(s.row_model.rows.contains(&RowRef::Composer));
    assert_eq!(s.composer.text, "half written");
    assert!(s.select_file("missing.rs", now).is_empty());
    assert_eq!(s.current_file, Some(0));
}

#[test]
fn restores_the_last_opened_file_and_survives_empty_prs() {
    let s = session_with_last_opened(&[("a.rs", "@@ -1 +1 @@\n-a\n+b\n"), ("b.rs", "@@ -1 +1 @@\n-c\n+d\n")], "b.rs");
    assert_eq!(s.current_file, Some(1));
    let empty = session_with(&[]);
    assert_eq!(empty.current_file, None);
    assert!(empty.row_model.rows.is_empty());
    assert_eq!(next_index(&file_order(&[], &FilterState::default(), &ViewedSet::default()), None, true), None);
}
```

`session_with(&[(path, patch)])` and `session_with_last_opened(files, path)` are new `test_support` helpers. They open a session, feed `PrMetadataLoaded` and one `PrFilesPage { done: true }`, and the second one seeds `last_opened_file` before the page arrives.

- [ ] **Step 2: Run tests to verify they fail** — `cargo test build_one_holds_only_the_selected_file selecting_a_file_rebuilds_rows_and_keeps_the_composer restores_the_last_opened_file_and_survives_empty_prs`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `build_one`, `file_order`, `current_file`, `select_file`, the initial selection, the empty states, and the removals listed above. Update the existing diff-view and annotate tests that relied on `FileHeader` rows or `sticky_file`. Keep `build` itself: the perf test and annotate tests still use it.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test`. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff/row_model.rs src/state src/ui src/app.rs src/test_support.rs && git commit -m "feat: show one file at a time"`.

---

### Task 6: Paired split rows, diff colours and word highlights

Today split mode draws every `DiffRow` on its own table row, so a removed line and its replacement land on different rows and nothing is coloured. This task pairs them and paints the mockup's colours.

**Files:**
- Modify: `src/model/diff.rs` (move `DiffMode` here, deriving `Serialize`/`Deserialize`), `src/ui/widgets/diff_view.rs` (re-export `DiffMode`; drop `DiffViewState::mode`)
- Modify: `src/diff/row_model.rs`, `src/diff/annotate.rs`, `src/state/pr_detail.rs`, `src/ui/screens/pr_detail.rs` (`recompute_line_heights`)
- Create: `src/diff/word.rs`
- Test: `src/diff/word.rs`, `src/diff/row_model.rs`, `src/diff/annotate.rs`, `src/ui/widgets/diff_view.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 1 palette, Task 5 `build_one`.
- Produces:
  - `RowRef::Pair { file: u32, hunk: u32, left: Option<u32>, right: Option<u32> }`, used only in split mode. Context rows become `Pair { left: Some(i), right: Some(i) }`.
  - `pub fn pair_indices(rows: &[DiffRow]) -> Vec<(Option<u32>, Option<u32>)>`. It is the index form of the existing `split_pairs`: each run of removed rows followed by a run of added rows is zipped, and the shorter side is padded with `None`. `split_pairs` is reimplemented on top of it.
  - `build_one(files, file, mode: DiffMode) -> RowModel` (the signature changes from Task 5). Unified emits `Line` rows; split emits `Pair` rows. `PrSession` gains `pub mode: DiffMode` and `pub fn set_mode(&mut self, mode: DiffMode)`, which rebuilds the rows. The screen clears `PrDetailState::last_layout` on a mode change so heights are re-measured.
  - Annotate: `Placement::AfterLine { side, line }` matches a `Pair` row when its left row's `old_no == line` for `Left`, or its right row's `new_no == line` for `Right`. Comment rows stay full width in split mode.
  - Height: a wrapped `Pair` row is the larger of its two sides' `row_height`.
  - `pub fn word_diff(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)>` in `src/diff/word.rs`:
    - Returns the byte ranges that differ, after trimming the longest common prefix and suffix. Prefix and suffix are counted in `char`s, so the ranges always fall on char boundaries.
    - Returns `None` when the lines are identical, or when the changed parts are more than 60 % of `old.len() + new.len()`.
  - Drawing (`diff_view.rs`):
    - Each cell fills its rect before the text is drawn: removed code uses `del_bg` and its gutter `del_gutter`; added code uses `add_bg` and its gutter `add_gutter`; the missing side of a pair uses `filler`.
    - A sign glyph sits in a 14 pt column before the code: `+` in `ok`, `−` in `bad`, a space for context.
    - When both sides of a pair exist, `word_diff` runs per visible pair (the cost is bounded by `MAX_DISPLAY_CHARS`), and the changed range gets `TextFormat::background` = `del_word` or `add_word`. Unified mode pairs runs the same way, by index within the block.
  - `pub(crate) fn layout_job(text: &str, spans: &[HighlightSpan], mark: Option<Range<usize>>, mark_bg: egui::Color32, style: &RowStyle) -> LayoutJob`: splits sections at the mark's boundaries so that exactly the marked bytes carry `mark_bg`.

- [ ] **Step 1: Write the failing tests**

```rust
// src/diff/word.rs
#[test]
fn word_diff_finds_the_changed_middle() {
    assert_eq!(word_diff(r#"import { content } from "@/api";"#, r#"import { apps, content } from "@/api";"#),
               Some((9..9, 9..15)));
    assert_eq!(word_diff("      envelope.notification,", "      prefilled ?? envelope.notification,"),
               Some((6..6, 6..19)));
    assert_eq!(word_diff("naïve x", "naïve y"), Some((7..8, 7..8)));     // 'ï' is two bytes
    assert_eq!(word_diff("same", "same"), None);
    assert_eq!(word_diff("let a = 1;", "return foo(bar);"), None);       // mostly rewritten
}

// src/diff/row_model.rs
#[test]
fn split_mode_pairs_removed_and_added_runs() {
    let files = vec![loaded("a.rs", "@@ -1,4 +1,4 @@\n c\n-a\n-b\n+x\n d\n")];
    let m = build_one(&files, 0, DiffMode::Split);
    let pairs: Vec<_> = m.rows.iter().filter_map(|r| match r { RowRef::Pair { left, right, .. } => Some((*left, *right)), _ => None }).collect();
    assert_eq!(pairs, vec![(Some(0), Some(0)), (Some(1), Some(3)), (Some(2), None), (Some(4), Some(4))]);
    assert!(build_one(&files, 0, DiffMode::Unified).rows.iter().all(|r| !matches!(r, RowRef::Pair { .. })));
}

// src/diff/annotate.rs
#[test]
fn threads_land_after_the_pair_holding_their_line() {
    let files = vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-a\n+b\n c\n")];
    let base = build_one(&files, 0, DiffMode::Split);
    let item = InlineItem { file: 0, placement: Placement::AfterLine { side: Side::Right, line: 1 }, row: RowRef::Thread { thread: 0 } };
    let out = annotate(&base, &files, &[item]);
    assert!(matches!(out.rows[0], RowRef::Pair { right: Some(1), .. }));
    assert_eq!(out.rows[1], RowRef::Thread { thread: 0 });
}

// src/ui/widgets/diff_view.rs
#[test]
fn layout_job_marks_exactly_the_changed_bytes() {
    let style = test_row_style();
    let job = layout_job("import { apps, content }", &[], Some(9..15), DARK.add_word, &style);
    let marked: String = job.sections.iter()
        .filter(|s| s.format.background == DARK.add_word)
        .map(|s| &job.text[s.byte_range.clone()]).collect();
    assert_eq!(marked, "apps, ");
}
```

(`test_row_style()` is a small helper in the test module that builds a `RowStyle` from `egui::Style::default()`.)

- [ ] **Step 2: Run tests to verify they fail** — `cargo test word_diff_finds_the_changed_middle split_mode_pairs_removed_and_added_runs threads_land_after_the_pair_holding_their_line layout_job_marks_exactly_the_changed_bytes`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the `DiffMode` move, `Pair`, `pair_indices`, the `build_one` mode parameter, annotate matching, pair heights, `word_diff`, the cell fills and sign column, and the `layout_job` mark.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test`. Expected: PASS.

- [ ] **Step 5: Manual QA** — open a PR in split mode. Check against `pr.html` that a changed import line shows red left and green right on one row, with only `apps, ` strongly highlighted. Check that an added-only run shows the flat filler on the left.

- [ ] **Step 6: Commit** — `git add src/model/diff.rs src/diff src/state/pr_detail.rs src/ui && git commit -m "feat: pair split rows and colour diff lines"`.

---

### Task 7: File header bar, file footer, and remembered diff preferences

**Files:**
- Create: `src/ui/widgets/file_header.rs`
- Modify: `src/ui/widgets/mod.rs`, `src/ui/screens/pr_detail.rs`, `src/ui/widgets/diff_view.rs` (remove the old in-table header and toolbar), `src/persist/settings.rs`, `src/app.rs`
- Test: `src/ui/widgets/file_header.rs`, `src/persist/settings.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 2, 3, 5, 6; main-plan Task 48 (find, hide whitespace) and Task 49 (`ImageMode`).
- Produces:
  - `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] #[serde(default)] pub struct DiffPrefs { pub mode: DiffMode, pub wrap: bool, pub hide_whitespace: bool }`. The default is `Split`, `wrap: true`, `hide_whitespace: false`. `Settings` gains `pub diff: DiffPrefs`.
  - Opening a PR seeds `PrSession::mode` and `DiffViewState { wrap, hide_whitespace }` from `settings.diff`. Every change writes back to `settings.diff` and saves settings through the debounced settings writer from main-plan Task 53. The choice therefore survives restarts and applies to every PR.
  - `pub enum HeaderKind { Text, Image, Binary, TooLarge }`.
  - `pub struct FileHeaderVm { pub dir: String /* "src/lib/data-tools/" or "" */, pub name: String, pub status: FileStatus, pub previous_name: Option<String>, pub additions: u32, pub deletions: u32, pub viewed: bool, pub kind: HeaderKind }` and `pub fn file_header_vm(f: &PrFile, viewed: bool) -> FileHeaderVm`.
  - `pub enum FileHeaderAction { CopyPath, OpenInBrowser, CommentOnFile, ToggleViewed, SetMode(DiffMode), ToggleWrap, ToggleHideWhitespace, OpenFind, SetImageMode(ImageMode) }`.
  - `pub fn render_header(ui, vm: &FileHeaderVm, mode: DiffMode, wrap: bool, hide_ws: bool, image_mode: ImageMode, actions: &mut Vec<FileHeaderAction>)`. It draws a 44 pt bar in `bg` with a bottom `line` stroke, in this order:
    1. File icon, then the path (`dir` in `muted`, `name` in `mono_bold`), then a copy icon button.
    2. A status pill ("Added" `ok`, "Modified" `warn`, "Removed" `bad`, "Renamed" `info`), "from {previous_name}" for renames, and `+N`/`−N` in mono for Text and TooLarge files.
    3. The spacer, then the controls for the kind:
       - Text: a split/unified icon segment (`COLUMNS_2` "Split  S", `ROWS_2` "Unified  U"), a wrap toggle (`WRAP_TEXT`), hide whitespace (`PILCROW` "Hide whitespace-only changes"), and find (`SEARCH` "Find in file  ⌘F").
       - Image: a text segment "2-up", "Swipe", "Onion skin".
       - Binary and TooLarge: no controls.
    4. A separator, then comment on file (`MESSAGE_SQUARE_PLUS`), open on GitHub (`EXTERNAL_LINK`), and a "Viewed" button with `SQUARE`/`SQUARE_CHECK` and a `V` key hint, filled `accent_soft` when viewed.
    5. When the bar is too narrow, `dir` is shortened from the left with "…"; the controls and Viewed are never hidden.
  - `pub struct FooterVm { pub prev: Option<String>, pub next: Option<String>, pub position: usize, pub total: usize }` and `pub fn footer_vm(order: &[u32], current: Option<u32>, files: &[PrFile]) -> FooterVm` (file names, not paths).
  - `pub enum FooterAction { Prev, Next, ViewedAndNext }`. The 40 pt footer reads: "‹ {prev} K" … "{position} of {total}" … "✓ Viewed, next ⇧V" and "J {next} ›". Prev and next are disabled at the ends and show "First file" / "Last file".
  - `pub fn elide_left(text: &str, max_chars: usize) -> String`: keeps the end of the string and prefixes "…" when it is cut.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn header_vm_splits_the_path_and_names_renames() {
    let mut f = file("src/lib/data-tools/push.ts");
    let vm = file_header_vm(&f, false);
    assert_eq!((vm.dir.as_str(), vm.name.as_str()), ("src/lib/data-tools/", "push.ts"));
    assert_eq!(vm.kind, HeaderKind::Text);
    f.status = FileStatus::Renamed;
    f.previous_path = Some("src/components/notifications/kind.ts".into());
    assert_eq!(file_header_vm(&f, true).previous_name.as_deref(), Some("kind.ts"));
    assert_eq!(file_header_vm(&file("package-lock.json"), false).dir, "");
}

#[test]
fn footer_names_neighbours_and_clamps() {
    let files = files_at(&["a/x.ts", "a/y.ts", "b/z.ts"]);
    let vm = footer_vm(&[0, 1, 2], Some(1), &files);
    assert_eq!((vm.prev.as_deref(), vm.next.as_deref(), vm.position, vm.total), (Some("x.ts"), Some("z.ts"), 2, 3));
    let first = footer_vm(&[0, 1, 2], Some(0), &files);
    assert_eq!(first.prev, None);
}

#[test]
fn elide_left_keeps_the_end() {
    assert_eq!(elide_left("src/components/notifications/", 16), "…notifications/");
    assert_eq!(elide_left("src/", 16), "src/");
}

// src/persist/settings.rs
#[test]
fn diff_prefs_default_to_split_and_round_trip() {
    assert_eq!(Settings::default().diff.mode, DiffMode::Split);
    assert!(Settings::default().diff.wrap);
    let dir = tempdir();
    let mut s = Settings::default();
    s.diff.mode = DiffMode::Unified;
    s.diff.wrap = false;
    s.save(dir.path()).unwrap();
    let r = Settings::load_or_default(dir.path());
    assert_eq!((r.diff.mode, r.diff.wrap), (DiffMode::Unified, false));
    std::fs::write(dir.path().join("settings.toml"), "host = \"github.com\"\n").unwrap();
    assert_eq!(Settings::load_or_default(dir.path()).diff.mode, DiffMode::Split);  // older file
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test header_vm_splits_the_path_and_names_renames footer_names_neighbours_and_clamps elide_left_keeps_the_end diff_prefs_default_to_split_and_round_trip`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the widget and vms, the panel layout in `pr_detail.rs` (header as a top strip of the centre pane, footer as a bottom strip, diff table between), the action wiring (CopyPath uses `ctx.copy_text`; OpenInBrowser uses `ctx.open_url`; ViewedAndNext marks the current file viewed and then selects `next_index` forward), and `DiffPrefs` with its persistence.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test`. Expected: PASS.

- [ ] **Step 5: Manual QA** — switch to unified and turn wrap off, then restart the app and open a different PR: it opens unified with wrap off. Resize the window to 900 pt wide and check that the path shortens while Viewed stays visible.

- [ ] **Step 6: Commit** — `git add src/ui src/persist/settings.rs src/app.rs && git commit -m "feat: add file header bar, footer navigation and remembered diff preferences"`.

---

### Task 8: Hunk headers, context expansion, and line comment affordances

**Files:**
- Modify: `src/ui/widgets/diff_view.rs`
- Test: `src/ui/widgets/diff_view.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: main-plan Task 45 (`Gap`, `Expand`, `DiffAction::ExpandGap`), Tasks 1, 2, 6.
- Produces:
  - `pub enum GapButton { Up(u32), Down(u32), All(u32) }` and `pub fn gap_buttons(hidden: u32, tail: bool) -> Vec<GapButton>`:
    - `hidden == 0` → `[]`
    - Not a tail gap: `hidden <= 20` → `[Up(hidden)]`, otherwise `[Up(20), All(hidden)]`
    - Tail gap: `Down(min(20, hidden))`, followed by `All(hidden)` when `hidden > 20`
  - Gap and hunk row, 28 pt high on a `hunk_bg` fill:
    - Accent text buttons with `CHEVRONS_UP` / `CHEVRONS_DOWN` / `UNFOLD_VERTICAL` and labels `"{n}"` or `"All {n} lines"`.
    - Then `@@ -a,b +c,d @@ {section}` in `faint` mono.
    - Clicking emits `DiffAction::ExpandGap` with the matching `Expand`.
  - Line comment affordance:
    - Hovering a code cell shows an 18×18 accent `PLUS` button at its left edge (tooltip "Comment on line").
    - Clicking it, or clicking the line number, emits `DiffAction::StartComment(CommentTarget::Line(anchor))` for that side.
    - The row whose anchor equals the open composer's target is tinted with `accent_soft`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn gap_buttons_offer_twenty_then_all() {
    assert_eq!(gap_buttons(0, false), vec![]);
    assert_eq!(gap_buttons(6, false), vec![GapButton::Up(6)]);
    assert_eq!(gap_buttons(54, false), vec![GapButton::Up(20), GapButton::All(54)]);
    assert_eq!(gap_buttons(41, true), vec![GapButton::Down(20), GapButton::All(41)]);
    assert_eq!(gap_buttons(12, true), vec![GapButton::Down(12)]);
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test gap_buttons_offer_twenty_then_all`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `gap_buttons`, the gap row drawing, the hover plus button, the clickable gutters, and the composer-row tint.

- [ ] **Step 4: Run test to verify it passes** — `cargo test diff_view`. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui/widgets/diff_view.rs && git commit -m "feat: restyle hunk rows and add line comment affordances"`.

---

### Task 9: Threads, pending comments, and the composer

**Files:**
- Modify: `src/ui/widgets/thread_view.rs`, `src/ui/widgets/comment_composer.rs`, `src/state/composer.rs`
- Test: those files (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1, 2; main-plan Tasks 34–37.
- Produces:
  - `pub const AVATAR_COLORS: [egui::Color32; 8]` = `#5b7ee5 #c0629b #3f9d8f #c47a35 #8a6bd1 #4f97c9 #b5534f #6c9a3c`, and `pub fn avatar_color(login: &str) -> egui::Color32`. The hash is `h = h.wrapping_mul(31).wrapping_add(byte as i32)` over the UTF-8 bytes; the index is `h.unsigned_abs() % 8`. The existing `avatar_initials` keeps its behaviour. Avatars are filled circles with white initials: 20 pt in compact rows, 28 pt in threads.
  - Thread frame:
    - Corner radius 10, 1 pt `line` stroke (`pending` stroke for pending comments), fill `bg`, margin `10/14/10/64` in unified and `10/14/10/14` in split.
    - Bar: `MESSAGES_SQUARE` "{n} comments" … "✓ Resolve" or "Unresolve".
    - Each comment: avatar, bold author, `faint` relative time, then the markdown body.
    - A reply row with your avatar and a "Reply" field.
    - A suggestion block reads "Suggested change" with a copy icon, then a `del_bg` row and an `add_bg` row.
  - Resolved and outdated threads collapse to one 32 pt line: `CIRCLE_CHECK` "Resolved by {author}", the first 60 characters of the root body in `faint`, and a "Show" button.
  - Pending comments: a `pending` stroke, a "Pending" pill (`MESSAGE_SQUARE_DASHED`), and edit (`PENCIL`) and delete (`TRASH_2`) icon buttons. They have no reply row.
  - Composer, with an `accent` stroke:
    1. A toolbar: a "Write"/"Preview" segment; bold, code and link icon buttons; an insert-suggestion button (`FILE_DIFF`) on the right side only; for context lines, a "Left"/"Right" segment; and a `faint` label "Line R75", "Line L12" or "Whole file".
    2. The text area.
    3. A footer: `HARD_DRIVE` "Saved as you type", then "Cancel", then the existing mode buttons. File-level comments show only "Add single comment" as the primary action. Line comments show "Add single comment" plus a primary "Add to review" or "Start a review" with a `⌘↵` hint.
  - `pub enum Wrap { Bold, Code, Link }` and `pub fn wrap_selection(text: &str, sel: Range<usize>, kind: Wrap) -> (String, Range<usize>)`:
    - Bold wraps the selection in `**…**`, Code in `` `…` ``, and Link gives `[…](url)`.
    - The returned range selects the original text inside the markers; for Link it selects `url`.
    - An empty selection inserts the empty markers and puts the caret between them.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn avatar_colours_are_stable() {
    assert_eq!(avatar_color("sercand"), avatar_color("sercand"));
    assert!(AVATAR_COLORS.contains(&avatar_color("dogacanaydin")));
    assert_eq!(AVATAR_COLORS[0], egui::Color32::from_hex("#5b7ee5").unwrap());
}

#[test]
fn wrap_selection_inserts_markdown() {
    assert_eq!(wrap_selection("use foo here", 4..7, Wrap::Bold), ("use **foo** here".into(), 6..9));
    assert_eq!(wrap_selection("call x", 5..6, Wrap::Code), ("call `x`".into(), 6..7));
    assert_eq!(wrap_selection("see docs", 4..8, Wrap::Link), ("see [docs](url)".into(), 11..14));
    assert_eq!(wrap_selection("", 0..0, Wrap::Bold), ("****".into(), 2..2));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test avatar_colours_are_stable wrap_selection_inserts_markdown`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the helpers and restyle both widgets to the mockup (`pr.html`, threads under `push.ts` lines 8, 75 and 99). The existing render tests must keep passing.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test thread_view comment_composer composer`. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui/widgets/thread_view.rs src/ui/widgets/comment_composer.rs src/state/composer.rs && git commit -m "feat: restyle threads, pending comments and composer"`.

---

## Phase R3 — The files pane

### Task 10: Filter popover combobox

**Files:**
- Modify: `src/ui/widgets/filter_bar.rs`, `src/ui/screens/pr_detail.rs`
- Test: `src/ui/widgets/filter_bar.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 2, 3; `FilterState`, `extension`, `status_group`, `extension_label`.
- Produces:
  - `pub struct ExtOption { pub ext: String, pub count: usize, pub selected: bool }` and `pub fn ext_options(files: &[PrFile], filters: &FilterState, query: &str) -> Vec<ExtOption>`. Options are sorted by count, highest first, then by `ext`. The query matches case-insensitively, with or without a leading `.`, against `ext`, or against `"(none)"` for files with no extension.
  - `pub fn active_filter_count(f: &FilterState) -> usize` = `extensions.len() + statuses.len()`. The path query and hide-viewed have their own controls and are not counted.
  - `pub enum Chip { Ext(String), Status(FileStatus) }` and `pub fn chips(f: &FilterState) -> Vec<Chip>`: extensions first, then statuses, in set order.
  - Layout at the top of the left pane:
    1. A path field ("Filter paths, * matches any", `SEARCH` icon).
    2. A `LIST_FILTER` icon button (tooltip "Filter by type and status") with a count badge. It is drawn `on` while any filter is active.
    3. The button opens an `egui::Popup` anchored below it, 280 pt wide, closing on an outside click or `Esc`. Its contents:
       - A "File type" search field with focus.
       - The "File types" list: a checkbox, file icon, `.ext` and a `faint` count per row.
       - "Status": a 2×2 grid of A/M/D/R in status colours with labels and counts.
       - "Clear filters".
    4. Active filters show as removable chips (`accent_soft`, the icon or letter, the label, and an `X` icon) under the field. Clicking a chip removes that filter.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn ext_options_count_sort_and_search() {
    let files = files_at(&["a.ts", "b.ts", "c.tsx", "d.png", "Makefile"]);
    let mut f = FilterState::default();
    f.extensions.insert("tsx".into());
    let all = ext_options(&files, &f, "");
    assert_eq!(all.iter().map(|o| (o.ext.as_str(), o.count, o.selected)).collect::<Vec<_>>(),
               vec![("ts", 2, false), ("", 1, false), ("png", 1, false), ("tsx", 1, true)]);
    assert_eq!(ext_options(&files, &f, ".TS").len(), 2);                 // ts and tsx
    assert_eq!(ext_options(&files, &f, "none")[0].ext, "");
}

#[test]
fn chips_and_count_ignore_path_and_hide_viewed() {
    let mut f = FilterState { path_query: "src/*".into(), hide_viewed: true, ..Default::default() };
    assert_eq!(active_filter_count(&f), 0);
    f.extensions.insert("ts".into());
    f.statuses.insert(FileStatus::Added);
    assert_eq!(active_filter_count(&f), 2);
    assert_eq!(chips(&f), vec![Chip::Ext("ts".into()), Chip::Status(FileStatus::Added)]);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test ext_options_count_sort_and_search chips_and_count_ignore_path_and_hide_viewed`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions and replace the old chip-row filter bar with the field, the button, the popover and the chips.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test filter_bar`. Expected: PASS.

- [ ] **Step 5: Manual QA** — open the popover, type `ts`, tick `.tsx` and `R`, then click outside. The chips show both, the badge shows 2, and the tree is filtered.

- [ ] **Step 6: Commit** — `git add src/ui/widgets/filter_bar.rs src/ui/screens/pr_detail.rs && git commit -m "feat: replace the filter bar with a popover combobox"`.

---

### Task 11: File tree rows, progress and tools

**Files:**
- Modify: `src/ui/widgets/file_tree.rs`, `src/ui/screens/pr_detail.rs`, `src/state/pr_detail.rs` (comment counts), `src/state/filters.rs` (`file_order` order parameter)
- Test: `src/ui/widgets/file_tree.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1–3, 5, 10; `TreeRowVm`, `flatten_visible`, `TreeAction`.
- Produces:
  - `TreeRowVm` gains `pub comments: u32` (unresolved threads on the file) and `pub pending: u32` (local pending comments on the file). `flatten_visible` takes an extra `counts: &HashMap<String, (u32, u32)>` argument, built by `pub fn comment_counts(threads: &[ReviewThread], pending: &PendingReview) -> HashMap<String, (u32, u32)>` in `src/state/pr_detail.rs`.
  - `pub enum TreeOrder { Path, LargestFirst }` and a matching `flatten_visible` parameter. Within a directory, `LargestFirst` orders files by `additions + deletions`, largest first; directories keep path order. `file_order` (Task 5) gains the same parameter, becoming `file_order(files, filters, viewed, order: TreeOrder)`, so `j`/`k` follow the visible order. Update the Task 5 callers and tests to pass `TreeOrder::Path`.
  - `TreeAction::CollapseAll` expands everything when any directory is collapsed, and otherwise collapses every directory.
  - `pub fn status_letter(s: FileStatus) -> char`: `Added` → `A`, `Removed` → `D`, `Renamed` → `R`, everything else → `M`.
  - `pub fn progress_label(viewed_f: usize, total_f: usize, viewed: usize, total: usize, filtered: bool) -> String`: `"5 of 26 viewed"` when not filtered, `"3 of 9 shown viewed, 5 of 26 total"` when filtered.
  - Each row is 26 pt high, indented 14 pt per level:
    1. A chevron for directories, then the folder or file icon, then the name. Directories use `muted`, viewed files use `faint`, the rest use `text`.
    2. A right cluster:
       - `MESSAGE_SQUARE {n}` in `muted` when `comments > 0`, and `MESSAGE_SQUARE_DASHED {n}` in `pending` when `pending > 0`.
       - `+a −d` in 11 pt mono (`ok`/`bad`). Omit it when both are 0, which covers binary files and binary-only directories.
       - The status letter in its colour.
       - The viewed checkbox, drawn on hover or when not `TriState::None`, as a filled `accent` box with a check or a dash for `Some`.
    3. The selected row gets an `accent_soft` fill and a 2 pt `accent` bar on its left edge.
  - Under the filter controls:
    - The `progress_label` text with a 4 pt progress bar.
    - A tools row: `"{n} files +X −Y"` in `faint` mono, then hide viewed (`EYE_OFF`), largest first (`ARROW_DOWN_WIDE_NARROW`), collapse folders (`CHEVRONS_DOWN_UP`) and mark all viewed (`CHECK_CHECK`), each with its tooltip from the mockup.
    - The existing "N files you had viewed changed since" hint, in `pending`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn status_letters_and_progress_labels() {
    assert_eq!(status_letter(FileStatus::Added), 'A');
    assert_eq!(status_letter(FileStatus::Copied), 'M');
    assert_eq!(progress_label(5, 26, 5, 26, false), "5 of 26 viewed");
    assert_eq!(progress_label(3, 9, 5, 26, true), "3 of 9 shown viewed, 5 of 26 total");
}

#[test]
fn largest_first_orders_files_within_a_folder() {
    let mut files = files_at(&["d/small.rs", "d/big.rs"]);
    files[1].additions = 40;
    let tree = build_tree(&files, &|_| true);
    let rows = flatten_visible(&tree, &Default::default(), &files, &FilterState::default(),
                               &ViewedSet::default(), None, &HashMap::new(), TreeOrder::LargestFirst);
    let names: Vec<_> = rows.iter().filter(|r| !r.is_dir).map(|r| r.label.as_str()).collect();
    assert_eq!(names, vec!["big.rs", "small.rs"]);
}

#[test]
fn rows_carry_comment_and_pending_counts() {
    let files = files_at(&["a.rs"]);
    let counts = HashMap::from([("a.rs".to_string(), (2, 1))]);
    let rows = flatten_visible(&build_tree(&files, &|_| true), &Default::default(), &files,
                               &FilterState::default(), &ViewedSet::default(), None, &counts, TreeOrder::Path);
    assert_eq!((rows[0].comments, rows[0].pending), (2, 1));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test status_letters_and_progress_labels largest_first_orders_files_within_a_folder rows_carry_comment_and_pending_counts`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `comment_counts`, `TreeOrder`, `CollapseAll`, the helpers, and the row and progress drawing. The left pane's width persists through main-plan Task 53's `Layout::left_pane` (default 300, clamped to 220–560).

- [ ] **Step 4: Run tests to verify they pass** — `cargo test file_tree`. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui/widgets/file_tree.rs src/ui/screens/pr_detail.rs src/state/pr_detail.rs src/state/filters.rs && git commit -m "feat: restyle file tree with icons, counts and progress"`.

---

## Phase R4 — PR detail chrome

### Task 12: Top bar, review popover and banners

**Files:**
- Create: `src/ui/widgets/top_bar.rs`
- Modify: `src/ui/widgets/review_panel.rs` (becomes popover content), `src/ui/screens/pr_detail.rs` (remove the bottom review panel), `src/ui/widgets/mod.rs`, `src/diff/row_model.rs`
- Test: `src/ui/widgets/top_bar.rs`, `src/ui/widgets/review_panel.rs`, `src/diff/row_model.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1–3, 5; main-plan Tasks 42–44 (`ReviewPanelVm`, `ReviewAction`), 50 (`CheckRun`, `CiState`), 52 (palette), 53 (`Theme`).
- Produces:
  - `pub struct TopBarVm { pub draft: bool, pub repo: String /* "otsimo/journey" */, pub number: u64, pub title: Option<String>, pub ci: Option<CiState>, pub pending: usize }` and `pub fn top_bar_vm(s: &PrSession) -> TopBarVm`. `ci` comes from Task 50's checks once they are loaded.
  - `pub enum TopBarAction { Back, Palette, Refresh, ToggleTheme, Settings, ToggleDetails }`.
  - `pub fn next_theme(current: Theme, system_dark: bool) -> Theme`: Dark → Light, Light → Dark, and System → the opposite of what it currently resolves to.
  - Bar contents, left to right:
    - Back (`ARROW_LEFT`, "Back to list  Esc"), then a state pill ("Open" `ok` with `GIT_PULL_REQUEST`, or "Draft" `muted` with `GIT_PULL_REQUEST_DRAFT`).
    - The repo in `muted` with `#n` in `faint`, then the title in `semibold`, elided at the end.
    - A CI icon button (`CIRCLE_CHECK` `ok` / `CIRCLE_X` `bad` / `CIRCLE_DOT_DASHED` `warn`, hidden when `None`). It opens a popover listing check runs: an icon, the name, and the run time in `faint`.
    - The spacer, then icon buttons: palette (`COMMAND`, "Commands  ⌘K"), refresh, theme (`SUN` in dark, `MOON` in light), settings, and details (`PANEL_RIGHT` toggle, "Details  O").
    - A separator, then the primary **Review** button (`SEND` "Review") with a `pending`-coloured count pill when `pending > 0`.
  - The review popover is 380 pt wide and anchored right, under the Review button. It replaces the bottom panel and contains, in order:
    1. "Finish your review".
    2. The summary text area ("Leave a summary (optional for Approve)").
    3. Three verdict cards, each with a radio icon (`CIRCLE`/`CIRCLE_DOT`), a type icon, a label and a description:
       - "Comment", "Feedback without approving"
       - "Approve", "Ready to merge"
       - "Request changes", "Needs work before merging; needs a summary"
       - Cards the author rule disallows are dimmed and show the existing reason.
    4. "{n} pending comments" with one row per comment: file icon, file name, `R99`/`L12`, and an excerpt.
    5. A footer with a ghost danger "Discard" (keeping the existing confirmation) and a primary "Submit review" with a `⌘↵` hint.
  - `pub struct PendingItemVm { pub path: String, pub name: String, pub line: String /* "R99" */, pub excerpt: String }` and `pub fn pending_items(p: &PendingReview) -> Vec<PendingItemVm>`. The excerpt is the body's first line, cut to 60 characters with "…" appended. Clicking an item selects the file (Task 5) and scrolls to the row from `RowModel::row_of_line(side, line) -> Option<usize>`, which is added to `row_model.rs`.
  - Banners: full-width strips above the diff, with a `pending_soft` fill and `pending` text. The server-side pending review and moved-head messages move here from the old review panel, with their existing copy and buttons ("Submit it", "Discard it").

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn theme_toggle_flips_the_resolved_theme() {
    assert_eq!(next_theme(Theme::Dark, false), Theme::Light);
    assert_eq!(next_theme(Theme::Light, true), Theme::Dark);
    assert_eq!(next_theme(Theme::System, true), Theme::Light);
    assert_eq!(next_theme(Theme::System, false), Theme::Dark);
}

#[test]
fn pending_items_label_lines_and_cut_excerpts() {
    let mut p = PendingReview::default();
    add_pending(&mut p, line_anchor("src/lib/data-tools/push.ts", 99), "Should the warning include the template id? It makes the toast actionable.\nsecond line".into());
    let items = pending_items(&p);
    assert_eq!(items[0].name, "push.ts");
    assert_eq!(items[0].line, "R99");
    assert_eq!(items[0].excerpt.chars().count(), 61);
    assert!(items[0].excerpt.ends_with('…'));
}

#[test]
fn row_of_line_finds_pairs_and_lines() {
    let files = vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-a\n+b\n c\n")];
    let split = build_one(&files, 0, DiffMode::Split);
    assert_eq!(split.row_of_line(&files, Side::Right, 2), Some(1));
    assert_eq!(split.row_of_line(&files, Side::Left, 1), Some(0));
    assert_eq!(split.row_of_line(&files, Side::Right, 99), None);
}
```

(`row_of_line` takes `files` so it can resolve row numbers: `pub fn row_of_line(&self, files: &[LoadedFile], side: Side, line: u32) -> Option<usize>`. Use the existing `add_pending` from main-plan Task 39, adjusted to its real signature.)

- [ ] **Step 2: Run tests to verify they fail** — `cargo test theme_toggle_flips_the_resolved_theme pending_items_label_lines_and_cut_excerpts row_of_line_finds_pairs_and_lines`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the top bar, the popover, the banners, `pending_items`, `row_of_line`, and the scroll-to-row in `diff_view` (`TableBuilder::scroll_to_row`). Remove the bottom panel. `⌘↵` (Task 52 `submit_review`) opens the popover when it is closed and submits when it is open.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test`. Expected: PASS, including the existing review-panel rule tests.

- [ ] **Step 5: Commit** — `git add src/ui src/diff/row_model.rs && git commit -m "feat: add top bar, review popover and banners"`.

---

### Task 13: Details pane tabs

**Files:**
- Modify: `src/ui/widgets/metadata_panel.rs` (main-plan Task 51), `src/ui/widgets/commit_list.rs`, `src/ui/widgets/checks.rs` (Task 50), `src/ui/screens/pr_detail.rs`, `src/persist/settings.rs` (`Layout`)
- Test: `src/ui/widgets/metadata_panel.rs`, `src/persist/settings.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1, 2; main-plan Tasks 50, 51, 53.
- Produces:
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)] pub enum DetailsTab { #[default] Overview, Conversation, Commits, Checks }`. `Layout` gains `pub details_tab: DetailsTab`, saved like the other layout fields.
  - `pub fn tab_counts(s: &PrSession) -> [Option<usize>; 4]`: Overview `None`, then the timeline length, the commit count and the check-run count, each `None` until that data has loaded.
  - The pane is 340 pt wide on a `surface` fill.
    - A tab strip: `FILE_TEXT` "Overview", then `MESSAGES_SQUARE`, `GIT_COMMIT_HORIZONTAL` and `CIRCLE_CHECK` icon tabs, each with a `faint` count and a tooltip naming the tab.
    - **Overview:** the title in `semibold` 15, then a key–value list with "Author" (avatar and login), "Branch" (`head` → `base` as mono chips), "Labels" (pills) and "Linked" (referenced issues as links). Then the description markdown, and "Reviewers" with state pills: "Requested" `warn` with `EYE`, "Approved" `ok`, "Changes requested" `bad`, "Commented" `muted`.
    - **Conversation:** the timeline with avatars and a connecting line, ending with the note "Conversation comments are read-only here. Reply on github.com."
    - **Commits:** rows with the message, a short SHA in `accent` mono, and a relative time.
    - **Checks:** "All {n} checks passed on {sha7}" or "{failed} of {n} checks failed on {sha7}", then the check rows.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn tab_counts_appear_once_loaded() {
    let s = session_with(&[("a.rs", "@@ -1 +1 @@\n-a\n+b\n")]);
    assert_eq!(tab_counts(&s), [None, None, None, None]);
}

// src/persist/settings.rs
#[test]
fn details_tab_round_trips() {
    let dir = tempdir();
    let mut s = Settings::default();
    s.layout.details_tab = DetailsTab::Checks;
    s.save(dir.path()).unwrap();
    assert_eq!(Settings::load_or_default(dir.path()).layout.details_tab, DetailsTab::Checks);
}
```

Extend `tab_counts_appear_once_loaded` with `CommitsLoaded` (2 commits) and `ChecksLoaded` (6 runs) events so it asserts `[None, Some(_), Some(2), Some(6)]`. Use the event shapes from main-plan Task 50.

- [ ] **Step 2: Run tests to verify they fail** — `cargo test tab_counts_appear_once_loaded details_tab_round_trips`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the enum, `tab_counts`, the tab strip, and the restyled sections.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test`. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui src/persist/settings.rs && git commit -m "feat: add tabbed details pane"`.

---

### Task 14: Settings window and command palette

**Files:**
- Modify: `src/ui/widgets/settings_window.rs` (main-plan Task 53), `src/ui/widgets/command_palette.rs` (Task 52), `src/ui/command.rs`, `src/persist/settings.rs`, `src/app.rs`
- Test: `src/ui/command.rs`, `src/persist/settings.rs`, `src/ui/theme.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1–3, 7; main-plan Tasks 52, 53.
- Produces:
  - `Settings` gains `pub code_font_size: f32` (default 13.0; valid values 12.0, 13.0 and 14.0; anything else loads as 13.0). In `theme.rs`, `pub fn style(base, code_font_size: f32)` replaces Task 1's one-argument form and sets `Monospace` to that size. `ROW_H` stays 20, which fits 14 pt.
  - The settings window is 600 pt wide, with a header (`SETTINGS` "Settings" and an `X` close button) and a two-column grid of 130 pt labels. Each control keeps its natural width; the text fields fill the row. The rows:
    - "Account": avatar, login, "{host}, scopes {scopes joined with ' and '}", and a danger "Sign out" with `LOG_OUT`.
    - "Theme": a Dark / Light / System segment.
    - "Default scopes": multi-select toggle chips, each with its scope icon (`EYE` Review requested, `USER_CHECK` Assigned, `PENCIL_LINE` Authored, `USERS` Involves, `AT_SIGN` Mentions). They wrap to a second line.
    - "Diff layout": a Split / Unified segment, which writes `settings.diff.mode`.
    - "Code font size": a 12 / 13 / 14 segment.
    - "Host" and "OAuth client ID": text fields.
  - Palette: a 560 pt modal placed 12 % from the top, with a 44 pt search field and no border. Rows show an icon or file icon, the label, and key hints as `kbd` chips. `pub enum PaletteEntry { Action(&'static ActionEntry), File { index: u32, path: String } }`. `pub fn palette_entries<'a>(actions: &'a [ActionEntry], files: &[PrFile], query: &str) -> Vec<PaletteEntry>` lists the fuzzy-matched actions first, then fuzzy-matched files by path. Files appear only while a PR is open, and selecting one calls `select_file`.

- [ ] **Step 1: Write the failing tests**

```rust
// src/persist/settings.rs
#[test]
fn code_font_size_is_clamped_to_the_offered_sizes() {
    let dir = tempdir();
    std::fs::write(dir.path().join("settings.toml"), "code_font_size = 30.0\n").unwrap();
    assert_eq!(Settings::load_or_default(dir.path()).code_font_size, 13.0);
    std::fs::write(dir.path().join("settings.toml"), "code_font_size = 14.0\n").unwrap();
    assert_eq!(Settings::load_or_default(dir.path()).code_font_size, 14.0);
}

// src/ui/theme.rs
#[test]
fn style_uses_the_code_font_size() {
    let s = style(egui::Style::default(), 14.0);
    assert_eq!(s.text_styles[&egui::TextStyle::Monospace].size, 14.0);
    assert_eq!(s.text_styles[&egui::TextStyle::Body].size, 13.0);
}

// src/ui/command.rs
#[test]
fn palette_lists_actions_then_files() {
    let actions = registry();
    let files = files_at(&["src/lib/data-tools/push.ts", "src/lib/notifications/schema.ts"]);
    let e = palette_entries(&actions, &files, "push");
    assert!(matches!(e.last(), Some(PaletteEntry::File { index: 0, .. })));
    assert!(palette_entries(&actions, &[], "split").iter().all(|x| matches!(x, PaletteEntry::Action(_))));
}
```

(Clamp in `Settings::load_or_default` after parsing.)

- [ ] **Step 2: Run tests to verify they fail** — `cargo test code_font_size_is_clamped_to_the_offered_sizes style_uses_the_code_font_size palette_lists_actions_then_files`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the setting, the `style` parameter, `palette_entries`, and the restyled window and palette.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test`. Expected: PASS.

- [ ] **Step 5: Manual QA** — compare against `prs.html#settings` and `pr.html#palette` in both themes. Check that no control stretches across its row.

- [ ] **Step 6: Commit** — `git add src/ui src/persist/settings.rs src/app.rs && git commit -m "feat: restyle settings window and command palette"`.

---

## Phase R5 — PR list and login

### Task 15: PR list

**Files:**
- Modify: `src/ui/screens/pr_list.rs`, `src/ui/widgets/pr_row.rs`, `src/state/pr_list.rs`, `src/model/pr.rs`, `src/persist/settings.rs`, `src/app.rs`
- Test: `src/ui/widgets/pr_row.rs`, `src/state/pr_list.rs`, `src/persist/settings.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1–3, 9 (`avatar_color`); main-plan Tasks 11–14.
- Produces:
  - `PrSummary` gains `pub scopes: Vec<Scope>`: every selected scope whose query returned this PR. When duplicates are merged, their scopes are unioned in `Scope` declaration order. `QueryCursor` gains `pub scope: Option<Scope>`, which is `None` for the constraint-only query.
  - `pub fn scope_count(state: &PrListState, scope: Scope) -> Option<u32>`: that scope's `issueCount` (`QueryCursor::total`) once its first page has arrived.
  - Toggling a scope chip updates `state.scopes` **and** `settings.default_scopes`, then saves settings, so the selection is remembered.
  - `Settings` gains `pub recent_constraints: Vec<String>` (most recent first, at most 5) and `pub group_by_repo: bool`. `pub fn push_recent(list: &mut Vec<String>, value: &str)` moves an existing entry to the front, inserts a new one there, and truncates to 5. Empty values are ignored.
  - Toolbar:
    - Scope chips (icon, label, count) in a wrapping row.
    - A repo/org combobox: a field showing "All repositories" or the constraint, with `CHEVRONS_UP_DOWN`. Its popover has a search field, "All repositories", the "Recent" entries, and "Organizations" (owners derived from the recents).
    - The filter field "Filter title, repo or author" with a `/` hint; `/` focuses it.
    - A sort button with the current sort name. Its menu has "Recently updated", "Newest", "Most commented" and a "Group by repository" check.
  - Rows are 52 pt high on a grid of 20 · flex · 54 · 26 · 150 · 120 · 44 · 64 pt:
    1. A state icon (`GIT_PULL_REQUEST` `ok`, or `GIT_PULL_REQUEST_DRAFT` `faint`).
    2. The title in medium weight, over a 12 pt meta line: repo, `#n` in `faint`, "by", a 16 pt avatar, the author, and a "Draft" pill when draft.
    3. "Why" scope icons with tooltips.
    4. A CI icon, the same as Task 12.
    5. A review pill: "Review required" `warn` with `EYE`, "Changes requested" `bad` with `FILE_DIFF`, "Approved" `ok` with `CHECK`, or "No reviews" in `faint`.
    6. `+a −d` in mono with a 40×6 bar.
    7. A comment count with `MESSAGE_SQUARE` (`faint` at 50 % when 0).
    8. A relative time.
    - Hover fills `surface`; the keyboard selection fills `accent_soft`. With `group_by_repo`, repos get sticky group headers.
    - A 26 pt status bar: "{loaded} of ≈{total} loaded" (the existing counter label) and the `j k` / `↵` hints.
  - `pub fn relative_time(iso: &str, now: SystemTime) -> String`: under a minute "now", then "{m}m", "{h}h", "{d}d" (under 14 days), "{w}w" (under 60 days), "{mo}mo" (under 365 days), "{y}y". An unparsable timestamp returns the input unchanged.
  - `pub fn compact_count(n: u32) -> String`: under 10,000 with thousands separators ("1,381"), under 100,000 with one decimal ("13.2k"), otherwise rounded ("443k").
  - `pub fn diff_fractions(add: u32, del: u32) -> (f32, f32)`, summing to 1.0; `(0.0, 0.0)` when both are 0.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn compact_counts_and_relative_times() {
    assert_eq!(compact_count(1_381), "1,381");
    assert_eq!(compact_count(13_170), "13.2k");
    assert_eq!(compact_count(442_773), "443k");
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let ago = |s: u64| humantime_iso(now - Duration::from_secs(s));      // test helper: RFC 3339 string
    assert_eq!(relative_time(&ago(30), now), "now");
    assert_eq!(relative_time(&ago(2 * 3600), now), "2h");
    assert_eq!(relative_time(&ago(3 * 86_400), now), "3d");
    assert_eq!(relative_time(&ago(15 * 86_400), now), "2w");
    assert_eq!(relative_time(&ago(90 * 86_400), now), "3mo");
    assert_eq!(relative_time(&ago(800 * 86_400), now), "2y");
    assert_eq!(relative_time("garbage", now), "garbage");
    assert_eq!(diff_fractions(3, 1), (0.75, 0.25));
    assert_eq!(diff_fractions(0, 0), (0.0, 0.0));
}

#[test]
fn merged_results_remember_every_matching_scope() {
    // the same PR arrives from the ReviewRequested and Assigned queries
    let state = list_with_pages(&[(Scope::ReviewRequested, vec![pr_summary(1, "t", 0)]),
                                  (Scope::Assigned, vec![pr_summary(1, "t", 0)])]);
    assert_eq!(state.items.len(), 1);
    assert_eq!(state.items[0].scopes, vec![Scope::ReviewRequested, Scope::Assigned]);
    assert_eq!(scope_count(&state, Scope::Assigned), Some(1));
}

#[test]
fn recent_constraints_are_most_recent_first_and_capped() {
    let mut r = vec![];
    for v in ["a/1", "a/2", "a/3", "a/4", "a/5", "a/6", "a/3", ""] { push_recent(&mut r, v); }
    assert_eq!(r, vec!["a/3", "a/6", "a/5", "a/4", "a/2"]);
}
```

(`humantime_iso` formats a `SystemTime` as RFC 3339 UTC in the test module; implement it with plain arithmetic, not a new dependency. `list_with_pages` is a test helper that drives `reduce` with `Refresh` and then one `PrSearchPage` event per query.)

- [ ] **Step 2: Run tests to verify they fail** — `cargo test compact_counts_and_relative_times merged_results_remember_every_matching_scope recent_constraints_are_most_recent_first_and_capped`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the state changes, helpers, settings fields and the restyled screen. Parse ISO timestamps by hand (`YYYY-MM-DDTHH:MM:SSZ` is all GitHub sends) rather than adding a date crate.

- [ ] **Step 4: Run tests to verify they pass** — `cargo test`. Expected: PASS.

- [ ] **Step 5: Manual QA** — compare with `prs.html`. Turn off "Authored", restart, and check it stays off. Pick a repo, then check it appears under Recent.

- [ ] **Step 6: Commit** — `git add src/ui src/state/pr_list.rs src/model/pr.rs src/persist/settings.rs src/app.rs && git commit -m "feat: redesign the PR list"`.

---

### Task 16: Login screen

**Files:**
- Modify: `src/ui/screens/login.rs`
- Test: `src/ui/screens/login.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 1, 2; main-plan Task 9 (`LoginVm`, `LoginAction`, `LoginForm`).
- Produces:
  - `pub fn code_groups(user_code: &str) -> Vec<Vec<char>>`: splits on `-` (`"8F3A-21KD"` → `[[8,F,3,A],[2,1,K,D]]`), and returns one group when there is no dash.
  - `pub fn expiry_label(remaining: Duration) -> String`: `"Waiting for you to approve. Code expires in 14:32."` (`m:ss`), or `"The code expired. Start over to get a new one."` at zero.
  - Layout: a 420 pt column centred in the window, containing:
    1. A 44 pt `accent` rounded square with `GIT_PULL_REQUEST`.
    2. The heading "Sign in to review pull requests" (22 pt semibold), then the lede "Your token stays in the system keychain. Drafts and viewed files are kept on this machine, even after you sign out."
    3. A bordered `surface` panel:
       - "Host", with a `GITHUB` "github.com" / `BUILDING_2` "Enterprise Server" segment. Enterprise reveals "Host name" and "OAuth client ID" fields and the `pending_soft` note "The built-in client ID only works on github.com. Use your own OAuth app, or a personal access token."
       - Underline tabs "Browser sign-in" / "Access token".
       - Device flow, idle: a full-width primary "Sign in with GitHub" (`LOG_IN`) with "We'll show a one-time code to enter on github.com." below it.
       - Device flow, code shown:
         - "Enter this code at github.com/login/device", then the code as 36×46 mono boxes per character with an en dash between groups.
         - "Copy code" (`COPY`) and a primary "Open GitHub" (`EXTERNAL_LINK`).
         - A spinner with `expiry_label`, then a ghost "Start over" (`ROTATE_CCW`).
       - Access token: a password field "ghp_… or github_pat_…", a primary "Use this token", and the note "Needs the repo scope to read private repositories and post comments."
    4. A footer: "Requests scopes repo and read:org" and a "Settings" link.
    - Existing error and expiry states render in the panel in `bad`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn device_code_groups_and_expiry_copy() {
    assert_eq!(code_groups("8F3A-21KD"), vec![vec!['8', 'F', '3', 'A'], vec!['2', '1', 'K', 'D']]);
    assert_eq!(code_groups("ABCD"), vec![vec!['A', 'B', 'C', 'D']]);
    assert_eq!(expiry_label(Duration::from_secs(14 * 60 + 32)), "Waiting for you to approve. Code expires in 14:32.");
    assert_eq!(expiry_label(Duration::from_secs(5)), "Waiting for you to approve. Code expires in 0:05.");
    assert_eq!(expiry_label(Duration::ZERO), "The code expired. Start over to get a new one.");
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test device_code_groups_and_expiry_copy`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the helpers and the layout. The existing render test must keep passing.

- [ ] **Step 4: Run test to verify it passes** — `cargo test login`. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui/screens/login.rs && git commit -m "feat: redesign the login screen"`.

---

## Phase R6 — Guardrail and sign-off

### Task 17: Single-file performance guardrail and visual QA pass

**Files:**
- Modify: `tests/perf_large_pr.rs` (main-plan Task 54), `tests/common/mod.rs`
- Test: `tests/perf_large_pr.rs`

**Interfaces:**
- Consumes: Tasks 5, 6; main-plan Task 54 `synthetic_files`.
- Produces: one more `#[ignore]`d wall-clock test and the recorded QA checklist below.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "wall-clock guardrail; run with `cargo test --release -- --ignored`"]
fn selecting_a_huge_file_stays_within_frame_budget() {
    let mut files = synthetic_files(1_500, 34);
    files.push(synthetic_files(1, 50_000).remove(0));                  // one 50k-line file, minified lines included
    let huge = (files.len() - 1) as u32;
    for mode in [DiffMode::Unified, DiffMode::Split] {
        let t = Instant::now();
        let model = build_one(&files, huge, mode);
        assert!(model.rows.len() >= 25_000);
        assert!(t.elapsed() < FRAME, "{mode:?} build took {:?}", t.elapsed());
        let t = Instant::now();
        let _ = build_one(&files, 0, mode);                            // switching to a small file
        assert!(t.elapsed() < Duration::from_millis(2));
    }
}
```

- [ ] **Step 2: Run it** — `cargo test --release --test perf_large_pr -- --ignored`. Expected: it passes if Tasks 5 and 6 kept `build_one` linear in the selected file. If it fails, profile `build_one`/`pair_indices` before changing the budget.

- [ ] **Step 3: Visual QA pass** — with `python3 -m http.server -d docs/design/mockups 8765` open beside the app, compare each item in dark and light and fix differences in the owning task's files:
  - **Login:** idle, code shown, access token, Enterprise.
  - **PR list:** scopes, repo popover, sort menu, grouped view, empty state.
  - **PR detail, diffs:** split, unified, wrap off, an added file, a removed file, the image modes, binary, large diff not loaded.
  - **PR detail, files pane:** the filter popover and the tree with hide-viewed.
  - **PR detail, comments:** threads, pending comments, the composer on a context line, the review popover.
  - **PR detail, chrome:** both banners and all four details tabs.
  - **Modals:** settings and the palette.
  - **Fonts:** check the egui frame-time readout while scrolling a 50,000-line file with wrap on, and confirm no text falls back to egui's default font.

- [ ] **Step 4: Commit** — `git add tests/perf_large_pr.rs tests/common/mod.rs && git commit -m "test: add single-file performance guardrail"`.
