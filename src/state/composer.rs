//! The comment composer's state machine (spec §7.1 and §7.2).
//!
//! The composer mirrors GitHub's two posting modes: post a single comment
//! immediately, or add to a (new or already open) pending review. File-level
//! comments can never be pending, and the side of an anchor may only be
//! changed for a single-line comment — a range already pins both ends
//! (spec §7.3).

use crate::github::comments::NewComment;
use crate::github::error::GhError;
use crate::model::comment::CommentTarget;
use crate::model::diff::Side;

/// Whether a comment posts on its own or joins a pending review.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CommentMode {
    /// Post as a single comment right away.
    #[default]
    Immediate,
    /// Add to the pending review.
    Pending,
}

/// Everything the composer widget carries between frames.
#[derive(Clone, Debug, Default)]
pub struct ComposerState {
    /// What the comment would attach to, once the composer is open.
    pub target: Option<CommentTarget>,
    /// The comment body.
    pub body: String,
    /// Immediate vs. pending.
    pub mode: CommentMode,
    /// Whether the preview pane is showing instead of the text edit.
    pub preview: bool,
    /// The last post error, shown above the body until the next post.
    pub error: Option<String>,
}

/// An event the composer reacts to.
pub enum ComposerMsg {
    /// Open the composer for `target`, picking the mode GitHub would.
    OpenFor {
        /// What the comment will attach to.
        target: CommentTarget,
        /// Whether a pending review is already open.
        has_open_review: bool,
    },
    /// Move the anchor to the other side; only a single-line anchor moves.
    SetSide(Side),
    /// Replace the body.
    SetBody(String),
    /// Switch posting mode; ignored for file targets.
    SetMode(CommentMode),
    /// Toggle between the text edit and the markdown preview.
    TogglePreview,
    /// The user dismissed the composer; forget everything.
    Cancel,
    /// The comment posted; forget everything.
    Submitted,
}

/// A control the composer widget reported for the app to fold in.
///
/// The mode buttons double as submit buttons, exactly as GitHub's do, so
/// `Submit` carries the mode the clicked button stands for.
#[derive(Clone, Debug, PartialEq)]
pub enum ComposerAction {
    /// Post (or add to the review) in the clicked button's mode.
    Submit(CommentMode),
    /// Dismiss the composer.
    Cancel,
    /// Move a single-line anchor to the other side.
    SetSide(Side),
    /// Swap the text edit for the markdown preview.
    TogglePreview,
    /// Append a suggestion block for the anchored rows.
    InsertSuggestion,
}

/// Whether the composer stays open after a failed post, and what it offers.
#[derive(Clone, Debug, PartialEq)]
pub enum PostOutcome {
    /// A server or network failure: the text is kept and only a manual
    /// retry button is offered — mutations are never retried automatically.
    Retryable(GhError),
    /// A 422 on a line anchor: GitHub rejected the coordinate, so posting
    /// the same text as a file-level comment can still succeed. Carries
    /// GitHub's message for the dialog.
    NeedsFileLevelFallback(String),
    /// Anything else: show the error and let the reviewer edit or cancel.
    Fatal(GhError),
}

/// Decide what a failed post means for the composer (spec §7.1).
///
/// A `Validation` (422) on a **line** target is the "line must be part of
/// the diff" case that a file-level comment survives; a 422 on a file
/// target has nowhere further to fall back to.
pub fn interpret_post_error(target: &CommentTarget, error: GhError) -> PostOutcome {
    match error {
        GhError::Validation(message) => match target {
            CommentTarget::Line(_) => PostOutcome::NeedsFileLevelFallback(message),
            CommentTarget::File { .. } => PostOutcome::Fatal(GhError::Validation(message)),
        },
        error @ (GhError::Server(_) | GhError::Network(_)) => PostOutcome::Retryable(error),
        error => PostOutcome::Fatal(error),
    }
}

/// The same comment re-anchored to the file: body and commit kept, only
/// the target gives up its coordinates (spec §7.3).
pub fn fallback_comment(c: &NewComment) -> NewComment {
    NewComment {
        commit_id: c.commit_id.clone(),
        body: c.body.clone(),
        target: CommentTarget::File {
            path: match &c.target {
                CommentTarget::Line(anchor) => anchor.path.clone(),
                CommentTarget::File { path } => path.clone(),
            },
        },
    }
}

/// The mode GitHub defaults to when the composer opens.
pub fn default_mode(has_open_review: bool) -> CommentMode {
    if has_open_review {
        CommentMode::Pending
    } else {
        CommentMode::Immediate
    }
}

/// Apply `msg` to `state`.
pub fn reduce(state: &mut ComposerState, msg: ComposerMsg) {
    match msg {
        ComposerMsg::OpenFor {
            target,
            has_open_review,
        } => {
            // A file-level comment is always posted on its own.
            state.mode = match target {
                CommentTarget::File { .. } => CommentMode::Immediate,
                CommentTarget::Line(_) => default_mode(has_open_review),
            };
            state.target = Some(target);
            state.error = None;
        }
        ComposerMsg::SetSide(side) => {
            // Only a single-line anchor can switch sides (spec §7.3): a
            // range's `start_side` must keep equaling `side`.
            if let Some(CommentTarget::Line(anchor)) = state.target.as_mut() {
                if anchor.start_line.is_none() {
                    anchor.side = side;
                }
            }
        }
        ComposerMsg::SetBody(body) => state.body = body,
        ComposerMsg::SetMode(mode) => {
            if mode == CommentMode::Pending
                && matches!(state.target, Some(CommentTarget::File { .. }))
            {
                return;
            }
            state.mode = mode;
        }
        ComposerMsg::TogglePreview => state.preview = !state.preview,
        ComposerMsg::Cancel | ComposerMsg::Submitted => *state = ComposerState::default(),
    }
}

/// Whether the composer has enough to post: a target and a non-blank body.
pub fn can_submit(state: &ComposerState) -> bool {
    state.target.is_some() && !state.body.trim().is_empty()
}

/// The mode buttons to show, in order, the way GitHub labels them.
pub fn mode_buttons(
    target: &CommentTarget,
    has_open_review: bool,
) -> Vec<(&'static str, CommentMode)> {
    match target {
        // A file-level comment is never part of a review.
        CommentTarget::File { .. } => vec![("Add single comment", CommentMode::Immediate)],
        CommentTarget::Line(_) => {
            if has_open_review {
                vec![
                    ("Add to review", CommentMode::Pending),
                    ("Add single comment", CommentMode::Immediate),
                ]
            } else {
                vec![
                    ("Add single comment", CommentMode::Immediate),
                    ("Start a review", CommentMode::Pending),
                ]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::line_anchor;

    #[test]
    fn modes_and_buttons_follow_github() {
        let line = CommentTarget::Line(line_anchor("a.rs", 1));
        let file = CommentTarget::File {
            path: "a.rs".into(),
        };
        assert_eq!(default_mode(false), CommentMode::Immediate);
        assert_eq!(default_mode(true), CommentMode::Pending);
        assert_eq!(
            mode_buttons(&line, false)[1],
            ("Start a review", CommentMode::Pending)
        );
        assert_eq!(
            mode_buttons(&line, true)[0],
            ("Add to review", CommentMode::Pending)
        );
        assert_eq!(
            mode_buttons(&file, true),
            vec![("Add single comment", CommentMode::Immediate)]
        );

        let mut s = ComposerState::default();
        reduce(
            &mut s,
            ComposerMsg::OpenFor {
                target: file,
                has_open_review: true,
            },
        );
        assert_eq!(s.mode, CommentMode::Immediate); // file-level can never be pending
        reduce(&mut s, ComposerMsg::SetMode(CommentMode::Pending));
        assert_eq!(s.mode, CommentMode::Immediate);
    }

    #[test]
    fn set_side_only_moves_single_line_anchors() {
        use crate::model::comment::Anchor;

        // A range anchor keeps its side: `start_side` must keep equalling
        // `side`, so SetSide is ignored (spec §7.3).
        let mut s = ComposerState::default();
        reduce(
            &mut s,
            ComposerMsg::OpenFor {
                target: CommentTarget::Line(Anchor {
                    path: "a.rs".into(),
                    side: Side::Right,
                    line: 14,
                    start_line: Some(10),
                    start_side: Some(Side::Right),
                }),
                has_open_review: false,
            },
        );
        reduce(&mut s, ComposerMsg::SetSide(Side::Left));
        let CommentTarget::Line(anchor) = s.target.as_ref().unwrap() else {
            panic!("expected the line anchor to remain");
        };
        assert_eq!(anchor.side, Side::Right);

        // A file target has no side at all, and SetSide changes nothing.
        let mut s = ComposerState::default();
        reduce(
            &mut s,
            ComposerMsg::OpenFor {
                target: CommentTarget::File {
                    path: "a.rs".into(),
                },
                has_open_review: false,
            },
        );
        reduce(&mut s, ComposerMsg::SetSide(Side::Left));
        assert_eq!(
            s.target,
            Some(CommentTarget::File {
                path: "a.rs".into()
            })
        );

        // A single-line anchor does move, so the rule above is the exception.
        let mut s = ComposerState::default();
        reduce(
            &mut s,
            ComposerMsg::OpenFor {
                target: CommentTarget::Line(line_anchor("a.rs", 1)),
                has_open_review: false,
            },
        );
        reduce(&mut s, ComposerMsg::SetSide(Side::Left));
        let CommentTarget::Line(anchor) = s.target.as_ref().unwrap() else {
            panic!("expected the line anchor to remain");
        };
        assert_eq!(anchor.side, Side::Left);
    }

    #[test]
    fn composer_requires_body_and_target() {
        let mut s = ComposerState::default();
        reduce(&mut s, ComposerMsg::SetBody("x".into()));
        assert!(!can_submit(&s));
        reduce(
            &mut s,
            ComposerMsg::OpenFor {
                target: CommentTarget::Line(line_anchor("a.rs", 1)),
                has_open_review: false,
            },
        );
        reduce(&mut s, ComposerMsg::SetBody("   ".into()));
        assert!(!can_submit(&s));
        reduce(&mut s, ComposerMsg::SetBody("comment".into()));
        assert!(can_submit(&s));
    }

    #[test]
    fn validation_on_a_line_offers_file_level_fallback() {
        let line = CommentTarget::Line(line_anchor("src/a.rs", 3));
        assert!(matches!(
            interpret_post_error(
                &line,
                GhError::Validation("line must be part of the diff".into())
            ),
            PostOutcome::NeedsFileLevelFallback(m) if m.contains("part of the diff")
        ));
        assert!(matches!(
            interpret_post_error(&line, GhError::Network("x".into())),
            PostOutcome::Retryable(_)
        ));
        let file = CommentTarget::File {
            path: "src/a.rs".into(),
        };
        assert!(matches!(
            interpret_post_error(&file, GhError::Validation("x".into())),
            PostOutcome::Fatal(_)
        ));

        let fb = fallback_comment(&NewComment {
            commit_id: "s".into(),
            body: "b".into(),
            target: line,
        });
        assert_eq!(
            fb.target,
            CommentTarget::File {
                path: "src/a.rs".into()
            }
        );
        assert_eq!(fb.commit_id, "s");
        assert_eq!(fb.body, "b");
    }
}
