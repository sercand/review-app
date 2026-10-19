//! Safe markdown rendering (Review Focus #1).
//!
//! [`to_doc`] turns a comment body into a flat list of blocks with a pure,
//! total function: no input can make it panic — not unterminated fences, not
//! thousands of `*`, not null bytes. Raw HTML is dropped, and images degrade
//! to their alt text plus URL.
//!
//! [`render`] draws a document with egui. Every code block gets a copy
//! button, and suggestion blocks (fence info string `suggestion`) are drawn
//! distinctly so they stand out in a thread.

use eframe::egui;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// A parsed markdown document: its blocks, in source order.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownDoc {
    /// The document's blocks.
    pub blocks: Vec<MdBlock>,
}

/// One block-level element of a [`MarkdownDoc`].
#[derive(Clone, Debug, PartialEq)]
pub enum MdBlock {
    /// A paragraph; inline markup is flattened to plain text.
    Paragraph(String),
    /// A heading: its level (1..=6) and text.
    Heading(u8, String),
    /// A code block. `suggestion` marks a GitHub review suggestion (a fenced
    /// block whose info string is `suggestion`).
    Code {
        /// The fence's info string, empty for indented blocks.
        lang: String,
        /// The code, lines ending in `\n`.
        text: String,
        /// Whether this is a review suggestion.
        suggestion: bool,
    },
    /// A list; a nested list's text is flattened into its parent item.
    List(Vec<String>),
    /// A block quote; nested quotes and their paragraphs merge into one.
    Quote(String),
    /// A thematic break (`---`).
    Rule,
}

/// Parse `md` into blocks.
///
/// Pure and total: whatever the input, this returns — it never panics. Raw
/// HTML (`Html`/`InlineHtml` events) is dropped; images become their alt
/// text plus URL.
pub fn to_doc(md: &str) -> MarkdownDoc {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;

    let mut b = Builder::default();
    for event in Parser::new_ext(md, options) {
        match event {
            Event::Start(tag) => b.start(tag),
            Event::End(end) => b.end(end),
            Event::Text(text) | Event::Code(text) => b.text(&text),
            // Raw HTML is dropped: the renderer never shows it.
            Event::Html(_) | Event::InlineHtml(_) => {}
            Event::SoftBreak | Event::HardBreak => b.text("\n"),
            Event::Rule => b.blocks.push(MdBlock::Rule),
            Event::TaskListMarker(checked) => b.text(if checked { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(name) => b.text(&name),
            // Math events only occur with options we do not enable.
            _ => {}
        }
    }
    b.finish()
}

/// Draw `doc` into `ui`.
pub fn render(ui: &mut egui::Ui, doc: &MarkdownDoc) {
    for (i, block) in doc.blocks.iter().enumerate() {
        // Per-block id scope: two code blocks each get a distinct "Copy".
        ui.push_id(i, |ui| match block {
            MdBlock::Paragraph(text) => {
                ui.label(text);
            }
            MdBlock::Heading(level, text) => {
                let size = match level {
                    1 => 22.0,
                    2 => 19.0,
                    3 => 17.0,
                    _ => 15.0,
                };
                ui.label(egui::RichText::new(text.as_str()).size(size).strong());
            }
            MdBlock::Code {
                lang,
                text,
                suggestion,
            } => {
                render_code(ui, lang, text, *suggestion);
            }
            MdBlock::List(items) => {
                ui.indent("md_list", |ui| {
                    for item in items {
                        ui.label(format!("• {item}"));
                    }
                });
            }
            MdBlock::Quote(text) => {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(10, 2))
                    .fill(ui.visuals().faint_bg_color)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(text.as_str()).italics());
                    });
            }
            MdBlock::Rule => {
                ui.separator();
            }
        });
        ui.add_space(4.0);
    }
}

/// Draw one code block: a framed monospace body with a copy button, and a
/// green frame plus a "Suggestion" tag for suggestion blocks.
fn render_code(ui: &mut egui::Ui, lang: &str, text: &str, suggestion: bool) {
    let mut frame = egui::Frame::new()
        .inner_margin(8.0)
        .corner_radius(6.0)
        .fill(ui.visuals().faint_bg_color);
    if suggestion {
        frame = frame.stroke(egui::Stroke::new(
            1.5,
            egui::Color32::from_rgb(46, 160, 67), // GitHub's suggestion green
        ));
    }
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            if suggestion {
                ui.label(
                    egui::RichText::new("Suggestion")
                        .small()
                        .color(egui::Color32::from_rgb(46, 160, 67)),
                );
            } else if !lang.is_empty() {
                ui.label(egui::RichText::new(lang).small().weak());
            }
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(text.to_owned());
            }
        });
        let mut job = egui::text::LayoutJob::default();
        job.append(
            text,
            0.0,
            egui::TextFormat::simple(egui::FontId::monospace(12.0), ui.visuals().text_color()),
        );
        ui.label(job);
    });
}

/// The event-stream state machine behind [`to_doc`].
///
/// Inline text is routed to the innermost open sink (image alt, code block,
/// table cell, list item, quote, paragraph), so any nesting of inline
/// elements terminates and produces plain text.
#[derive(Default)]
struct Builder {
    blocks: Vec<MdBlock>,
    para: String,
    heading: Option<u8>,
    quote_depth: usize,
    quote: String,
    list_depth: usize,
    items: Vec<String>,
    /// Open list items, outermost first; text goes to the innermost one.
    open_items: Vec<String>,
    /// Items of a list inside a quote, flushed after the enclosing `Quote`.
    quote_items: Vec<String>,
    code: Option<CodeAcc>,
    image: Option<ImageAcc>,
    cell: Option<String>,
    row: Option<Vec<String>>,
}

#[derive(Default)]
struct CodeAcc {
    lang: String,
    text: String,
    suggestion: bool,
}

#[derive(Default)]
struct ImageAcc {
    alt: String,
    url: String,
}

impl Builder {
    /// Append inline text to the innermost open sink.
    fn text(&mut self, s: &str) {
        if let Some(image) = self.image.as_mut() {
            image.alt.push_str(s);
        } else if let Some(code) = self.code.as_mut() {
            code.text.push_str(s);
        } else if let Some(cell) = self.cell.as_mut() {
            cell.push_str(s);
        } else if let Some(item) = self.open_items.last_mut() {
            item.push_str(s);
        } else if self.quote_depth > 0 {
            self.quote.push_str(s);
        } else {
            self.para.push_str(s);
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => self.heading = Some(level as u8),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.trim().to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                let suggestion = lang == "suggestion";
                self.code = Some(CodeAcc {
                    lang,
                    text: String::new(),
                    suggestion,
                });
            }
            Tag::List(_) => self.list_depth += 1,
            Tag::Item => self.open_items.push(String::new()),
            Tag::BlockQuote(_) => self.quote_depth += 1,
            Tag::Image { dest_url, .. } => {
                self.image = Some(ImageAcc {
                    alt: String::new(),
                    url: dest_url.into_string(),
                });
            }
            Tag::TableHead | Tag::TableRow => self.row = Some(Vec::new()),
            Tag::TableCell => self.cell = Some(String::new()),
            // Links keep their text; emphasis and friends keep theirs too.
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => self.flush_para(),
            TagEnd::Heading(_) => {
                let level = self.heading.take().unwrap_or(1);
                let text = std::mem::take(&mut self.para);
                if !text.is_empty() {
                    self.blocks.push(MdBlock::Heading(level, text));
                }
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.blocks.push(MdBlock::Code {
                        lang: code.lang,
                        text: code.text,
                        suggestion: code.suggestion,
                    });
                }
            }
            TagEnd::Item => {
                if let Some(item) = self.open_items.pop() {
                    let text = item.trim().to_owned();
                    match self.open_items.last_mut() {
                        // A nested item is flattened into its parent so the
                        // parent's own text is never lost.
                        Some(parent) => {
                            if !text.is_empty() {
                                if !parent.trim().is_empty() {
                                    parent.push_str("; ");
                                }
                                parent.push_str(&text);
                            }
                        }
                        // The outermost item completes the visible item.
                        None => self.items.push(text),
                    }
                }
            }
            TagEnd::List(_) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                if self.list_depth == 0 && !self.items.is_empty() {
                    let items = std::mem::take(&mut self.items);
                    if self.quote_depth > 0 {
                        // Inside a quote: hold the list until the quote
                        // closes so the blocks keep their source order.
                        self.quote_items.extend(items);
                    } else {
                        self.blocks.push(MdBlock::List(items));
                    }
                }
            }
            TagEnd::BlockQuote(_) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.quote_depth == 0 {
                    let text = std::mem::take(&mut self.quote);
                    if !text.is_empty() {
                        self.blocks.push(MdBlock::Quote(text));
                    }
                    if !self.quote_items.is_empty() {
                        self.blocks
                            .push(MdBlock::List(std::mem::take(&mut self.quote_items)));
                    }
                }
            }
            TagEnd::Image => {
                // Images render as their alt text plus URL — never fetched.
                if let Some(image) = self.image.take() {
                    let rendered = if image.alt.is_empty() {
                        format!("({})", image.url)
                    } else {
                        format!("{} ({})", image.alt, image.url)
                    };
                    self.text(&rendered);
                }
            }
            TagEnd::TableCell => {
                if let Some(cell) = self.cell.take() {
                    if let Some(row) = self.row.as_mut() {
                        row.push(cell.trim().to_owned());
                    }
                }
            }
            TagEnd::TableRow | TagEnd::TableHead => {
                // Tables have no block of their own: each row becomes a
                // paragraph of `cell | cell`.
                if let Some(row) = self.row.take() {
                    if !row.is_empty() {
                        self.blocks.push(MdBlock::Paragraph(row.join(" | ")));
                    }
                }
            }
            _ => {}
        }
    }

    fn flush_para(&mut self) {
        let text = std::mem::take(&mut self.para);
        if !text.is_empty() {
            self.blocks.push(MdBlock::Paragraph(text));
        }
    }

    /// Flush anything still open. The parser closes every block at end of
    /// input, so this is belt-and-braces.
    fn finish(mut self) -> MarkdownDoc {
        self.flush_para();
        MarkdownDoc {
            blocks: self.blocks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_markdown_never_panics() {
        let stars = "*".repeat(10_000);
        let nested = ">".repeat(5_000) + " deep";
        let brackets = "[".repeat(10_000);
        let big = "a".repeat(100_000);
        for bad in [
            "```unterminated",
            stars.as_str(),
            nested.as_str(),
            brackets.as_str(),
            "[](#",
            "# \u{0} null",
            big.as_str(),
        ] {
            let _ = to_doc(bad); // must not panic
        }
    }

    #[test]
    fn classifies_suggestion_code_block() {
        let doc = to_doc("```suggestion\nx\n```");
        assert!(matches!(
            &doc.blocks[0],
            MdBlock::Code {
                suggestion: true,
                ..
            }
        ));
    }

    #[test]
    fn drops_raw_html() {
        let doc = to_doc("<script>alert(1)</script>\n\nhi");
        assert!(doc
            .blocks
            .iter()
            .all(|b| !matches!(b, MdBlock::Paragraph(p) if p.contains("script"))));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, MdBlock::Paragraph(p) if p.contains("hi"))));
        // Inline HTML is dropped too: the tag markup must not survive.
        let doc = to_doc("hi <b>there</b>");
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, MdBlock::Paragraph(p) if p.contains("hi") && !p.contains("<b>"))));
    }

    #[test]
    fn nested_list_keeps_parent_text() {
        let doc = to_doc("- outer\n  - inner\n- next");
        let MdBlock::List(items) = &doc.blocks[0] else {
            panic!("expected a list, got {:?}", doc.blocks);
        };
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(items[0].contains("outer"), "{items:?}");
        assert!(items[0].contains("inner"), "{items:?}");
        assert_eq!(items[1], "next");
    }

    #[test]
    fn list_in_quote_flushes_after_the_quote() {
        // The quote's text comes first in the source, so it must come first
        // in the blocks too.
        let doc = to_doc("> intro\n>\n> - a");
        assert_eq!(
            doc.blocks,
            vec![
                MdBlock::Quote("intro".into()),
                MdBlock::List(vec!["a".into()]),
            ]
        );
        // A list-only quote still produces just the list.
        assert_eq!(
            to_doc("> - a").blocks,
            vec![MdBlock::List(vec!["a".into()])]
        );
    }
}
