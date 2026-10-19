//! The comment composer's state machine (spec §7.1 and §7.2).
//!
//! The composer mirrors GitHub's two posting modes: post a single comment
//! immediately, or add to a (new or already open) pending review. File-level
//! comments can never be pending, and the side of an anchor may only be
//! changed for a single-line comment — a range already pins both ends
//! (spec §7.3).

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
}
