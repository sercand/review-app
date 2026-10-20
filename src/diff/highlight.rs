//! Lazy, bounded syntax highlighting for the diff view (spec §8.4).
//!
//! Highlighting is computed **only for rows the table actually draws**, on
//! demand, and memoized in a bounded LRU keyed by `(syntax_name, hash(text))`.
//! A row longer than [`MAX_HIGHLIGHT_CHARS`] is never highlighted — a minified
//! bundle must not force a huge scope walk — and the cache is small enough that
//! scrolling a large PR cannot grow it without bound.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;

use eframe::egui;
use syntect::easy::HighlightLines;
use syntect::highlighting::ScopeSelectors;
use syntect::highlighting::{Color, FontStyle, StyleModifier, Theme, ThemeItem};
use syntect::parsing::SyntaxSet;

use crate::ui::theme::{Palette, palette};

/// One run of identically styled text inside a line.
///
/// `range` is a **byte** range into the text that was highlighted, so the
/// renderer can slice the same string without re-measuring anything.
#[derive(Clone, Debug, PartialEq)]
pub struct HighlightSpan {
    /// The span's byte range within the highlighted text.
    pub range: Range<usize>,
    /// The span's foreground colour, as sRGB bytes.
    pub color: [u8; 3],
    /// Whether the grammar marked the span bold.
    pub bold: bool,
    /// Whether the grammar marked the span italic.
    pub italic: bool,
}

/// Computes highlight spans for one line of text.
///
/// Implementations must be `Send` so a cache can live in app state without
/// pinning the highlighter to one thread; they are also expected to be
/// stateless per line (spec §8.4: highlighting is per line in isolation).
pub trait Highlighter: Send {
    /// The spans of one line, whose byte ranges index `text`.
    fn highlight(&self, syntax: &str, text: &str) -> Vec<HighlightSpan>;
}

/// The memoization key: a syntax name plus a hash of the exact text.
///
/// Hashing the text (rather than storing it) keeps the key small enough that a
/// cache of a few thousand entries costs almost nothing; a collision can only
/// mis-colour one line, never panic.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HighlightKey {
    /// The syntax name, e.g. `"rs"` (see [`syntax_for_path`]).
    pub syntax: String,
    /// The hash of the highlighted text.
    pub text_hash: u64,
}

impl HighlightKey {
    /// Build the key for one line.
    pub fn new(syntax: &str, text: &str) -> HighlightKey {
        HighlightKey {
            syntax: syntax.to_owned(),
            text_hash: hash_text(text),
        }
    }
}

/// Lines longer than this many characters are not highlighted (spec §8.4).
pub const MAX_HIGHLIGHT_CHARS: usize = 2_000;

/// A bounded LRU of computed highlight spans.
///
/// A `HashMap` plus a recency queue is enough: the cache is only touched from
/// the UI thread, and [`HighlightCache::spans`] returns the spans for the line
/// it was just asked about, so eviction can never lose a value the caller is
/// still using.
pub struct HighlightCache {
    capacity: usize,
    highlighter: Box<dyn Highlighter>,
    /// The dark-mode flag the current highlighter was built for; drives
    /// [`Self::set_theme`].
    dark: bool,
    spans: HashMap<HighlightKey, Vec<HighlightSpan>>,
    /// Least recently used first; every key appears at most once.
    order: VecDeque<HighlightKey>,
    hits: u64,
    misses: u64,
}

impl HighlightCache {
    /// A cache holding at most `capacity` lines, using `h` to compute misses.
    ///
    /// A capacity of zero disables caching: every line is recomputed and
    /// immediately dropped.
    pub fn new(capacity: usize, h: Box<dyn Highlighter>) -> HighlightCache {
        HighlightCache {
            capacity,
            highlighter: h,
            dark: true,
            spans: HashMap::new(),
            order: VecDeque::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// The spans of `text` under `syntax`, computing them on a miss.
    ///
    /// Text longer than [`MAX_HIGHLIGHT_CHARS`] returns `&[]` **without**
    /// calling the highlighter, and is never cached.
    pub fn spans(&mut self, syntax: &str, text: &str) -> &[HighlightSpan] {
        if text.chars().count() > MAX_HIGHLIGHT_CHARS {
            return &[];
        }

        let key = HighlightKey::new(syntax, text);
        if self.spans.contains_key(&key) {
            self.hits += 1;
            self.touch(&key);
        } else {
            self.misses += 1;
            let spans = self.highlighter.highlight(syntax, text);
            self.spans.insert(key.clone(), spans);
            self.order.push_back(key.clone());
            self.evict();
        }

        self.spans.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// How many lines are cached right now.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// How many lookups were served from the cache.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// How many lookups had to call the highlighter.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Rebuild the highlighter for the given theme and drop every cached
    /// span, so a mid-session theme switch cannot keep old colours.
    pub fn set_theme(&mut self, dark: bool) {
        if self.dark != dark {
            self.dark = dark;
            self.highlighter = Box::new(SyntectHighlighter::new(dark));
            self.spans.clear();
            self.order.clear();
        }
    }

    /// Move `key` to the most-recently-used end of the queue.
    fn touch(&mut self, key: &HighlightKey) {
        if let Some(position) = self.order.iter().position(|entry| entry == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.clone());
    }

    /// Drop least-recently-used entries until the cache fits its capacity.
    fn evict(&mut self) {
        while self.spans.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                // Only reachable if `spans` and `order` ever disagreed, which
                // they cannot; clearing keeps the cache bounded regardless.
                self.spans.clear();
                return;
            };
            self.spans.remove(&oldest);
        }
    }
}

/// Hash one line of text for [`HighlightKey`].
fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The syntax name to highlight `path` with.
///
/// The lowercased extension, or the lowercased file name when there is none
/// (`"Dockerfile"` → `"dockerfile"`); the same string is handed to the
/// [`Highlighter`], so the cache key and the grammar agree.
pub fn syntax_for_path(path: &str) -> String {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    match std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension) => extension.to_ascii_lowercase(),
        None => name.to_ascii_lowercase(),
    }
}

/// Build the syntect theme from a design [`Palette`]: the foreground is
/// `p.text`, the background `p.bg`, and the scope rules carry the `syn_*`
/// tokens.
pub fn review_theme(p: &Palette) -> Theme {
    fn rgb(c: egui::Color32) -> Color {
        Color {
            r: c.r(),
            g: c.g(),
            b: c.b(),
            a: 255,
        }
    }
    fn rule(scopes: &str, color: egui::Color32, font: FontStyle) -> ThemeItem {
        use std::str::FromStr;
        ThemeItem {
            scope: ScopeSelectors::from_str(scopes).expect("static scope selector"),
            style: StyleModifier {
                foreground: Some(rgb(color)),
                background: None,
                font_style: (font != FontStyle::empty()).then_some(font),
            },
        }
    }
    let mut theme = Theme {
        name: Some("gh-review".into()),
        ..Theme::default()
    };
    theme.settings.foreground = Some(rgb(p.text));
    theme.settings.background = Some(rgb(p.bg));
    theme.scopes = vec![
        rule("comment", p.syn_com, FontStyle::ITALIC),
        rule(
            "string, constant.character, string.regexp",
            p.syn_str,
            FontStyle::empty(),
        ),
        rule(
            "keyword, storage, storage.type, storage.modifier, constant.language, variable.language",
            p.syn_kw,
            FontStyle::empty(),
        ),
        rule(
            "entity.name.function, support.function, meta.function-call variable.function",
            p.syn_fn,
            FontStyle::empty(),
        ),
        rule(
            "entity.name.type, entity.name.class, support.type, support.class, entity.other.inherited-class",
            p.syn_type,
            FontStyle::empty(),
        ),
        rule("constant.numeric", p.syn_num, FontStyle::empty()),
        rule(
            "entity.other.attribute-name, variable.other.property, meta.object-literal.key, support.type.property-name",
            p.syn_attr,
            FontStyle::empty(),
        ),
    ];
    theme
}

/// The real highlighter: bat's extended syntax set (via `two-face`, which
/// covers TypeScript, TSX, Kotlin, Swift, TOML, Dockerfile and the rest)
/// and a theme built from the design palette.
///
/// Both are loaded once, in [`SyntectHighlighter::new`], because building a
/// `SyntaxSet` is far too slow for a per-line call.
pub struct SyntectHighlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl SyntectHighlighter {
    /// Load the extended syntaxes and the palette theme for `dark`.
    pub fn new(dark: bool) -> SyntectHighlighter {
        SyntectHighlighter {
            syntaxes: two_face::syntax::extra_newlines(),
            theme: review_theme(palette(dark)),
        }
    }
}

impl SyntectHighlighter {
    /// Whether `syntax` (a [`syntax_for_path`] name) resolves to a real
    /// grammar rather than plain text.
    pub fn has_syntax(&self, syntax: &str) -> bool {
        self.syntaxes
            .find_syntax_by_extension(syntax)
            .or_else(|| self.syntaxes.find_syntax_by_token(syntax))
            .is_some_and(|definition| {
                !std::ptr::eq(definition, self.syntaxes.find_syntax_plain_text())
            })
    }
}

impl Default for SyntectHighlighter {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Highlighter for SyntectHighlighter {
    fn highlight(&self, syntax: &str, text: &str) -> Vec<HighlightSpan> {
        let definition = self
            .syntaxes
            .find_syntax_by_extension(syntax)
            .or_else(|| self.syntaxes.find_syntax_by_token(syntax))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());

        let mut lines = HighlightLines::new(definition, &self.theme);
        let Ok(operations) = lines.highlight_line(text, &self.syntaxes) else {
            return Vec::new();
        };

        let mut spans = Vec::new();
        let mut start = 0;
        for (style, piece) in operations {
            let end = start + piece.len();
            spans.push(HighlightSpan {
                range: start..end,
                color: [style.foreground.r, style.foreground.g, style.foreground.b],
                bold: style.font_style.contains(FontStyle::BOLD),
                italic: style.font_style.contains(FontStyle::ITALIC),
            });
            start = end;
        }

        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CountingHighlighter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Highlighter for CountingHighlighter {
        fn highlight(&self, _: &str, text: &str) -> Vec<HighlightSpan> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![HighlightSpan {
                range: 0..text.len(),
                color: [0, 0, 0],
                bold: false,
                italic: false,
            }]
        }
    }

    #[test]
    fn caches_by_key_evicts_and_skips_long_lines() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut c = HighlightCache::new(2, Box::new(CountingHighlighter(calls.clone())));
        c.spans("rs", "let x=1;");
        c.spans("rs", "let x=1;");
        assert_eq!((c.hits(), c.misses()), (1, 1));
        c.spans("rs", "b");
        c.spans("rs", "c");
        assert!(c.len() <= 2);

        let minified = "a".repeat(MAX_HIGHLIGHT_CHARS + 1);
        assert!(c.spans("js", &minified).is_empty());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3); // long line never highlighted
    }

    #[test]
    fn real_highlighter_marks_rust_and_falls_back_to_plain_text() {
        let highlighter = SyntectHighlighter::new(true);
        let spans = highlighter.highlight("rs", "let x = 1;\n");
        assert!(!spans.is_empty());
        assert_eq!(spans[0].range.start, 0);
        assert_eq!(spans.last().unwrap().range.end, "let x = 1;\n".len());

        // An unknown extension falls back to plain text rather than panicking.
        let spans = highlighter.highlight("nope", "not code");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].range, 0.."not code".len());

        assert_eq!(syntax_for_path("src/lib.rs"), "rs");
        assert_eq!(syntax_for_path("README"), "readme");
        assert_eq!(syntax_for_path("dist/bundle.min.JS"), "js");
    }

    #[test]
    fn common_languages_resolve_to_a_real_syntax() {
        let h = SyntectHighlighter::new(true);
        // `jsx` is not in two-face 0.5's extended set (bat ships no JSX
        // grammar), so it is not listed; it falls back to plain text.
        for path in [
            "a.ts",
            "a.tsx",
            "a.js",
            "a.rs",
            "a.go",
            "a.py",
            "a.java",
            "a.kt",
            "a.swift",
            "a.cs",
            "a.cpp",
            "a.json",
            "a.yaml",
            "a.toml",
            "a.md",
            "a.sh",
            "a.sql",
            "a.css",
            "a.html",
            "a.vue",
            "a.svelte",
            "Dockerfile",
        ] {
            assert!(
                h.has_syntax(&syntax_for_path(path)),
                "{path} resolves to plain text"
            );
        }
    }

    #[test]
    fn typescript_keywords_use_the_palette() {
        let h = SyntectHighlighter::new(true);
        let spans = h.highlight("ts", "const prefilled = await withCreateDefaults(ctx);");
        let kw = crate::ui::theme::DARK.syn_kw;
        assert_eq!(spans[0].color, [kw.r(), kw.g(), kw.b()]); // `const`
    }

    #[test]
    fn switching_theme_clears_cached_colours() {
        let mut cache = HighlightCache::new(64, Box::new(SyntectHighlighter::new(true)));
        let dark = cache.spans("ts", "const a = 1;")[0].color;
        cache.set_theme(false);
        assert!(cache.is_empty());
        let light = cache.spans("ts", "const a = 1;")[0].color;
        assert_ne!(dark, light);
        cache.set_theme(false); // no change → keeps entries
        assert_eq!(cache.len(), 1);
    }
}
