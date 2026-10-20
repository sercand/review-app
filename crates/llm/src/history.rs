//! Pair-safe history trimming.

use crate::{ContentBlock, Message, MessageRole};

/// A message a kept history may begin with: a user message that answers no tool call.
/// Every later tool result then answers a tool call inside the kept history.
fn safe_start(m: &Message) -> bool {
    m.role == MessageRole::User
        && !m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// The newest part of `messages` that is at most `max_messages` long and starts at a safe
/// point, so it never begins with an assistant turn or a tool result whose call was cut off.
///
/// When the last `max_messages` hold no safe start, the newest safe start before them is
/// used, so the result can be longer than `max_messages`. With no safe start at all, the
/// result is empty. Returns a sub-slice; nothing is copied.
pub fn trim_history(messages: &[Message], max_messages: usize) -> &[Message] {
    if messages.len() <= max_messages {
        return messages;
    }
    let window = messages.len() - max_messages;
    let start = (window..messages.len())
        .find(|&i| safe_start(&messages[i]))
        .or_else(|| (0..window).rev().find(|&i| safe_start(&messages[i])))
        .unwrap_or(messages.len());
    &messages[start..]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ToolResultPart;

    fn user(t: &str) -> Message {
        Message::user_text(t)
    }

    fn assistant(t: &str) -> Message {
        Message::assistant(vec![ContentBlock::text(t)])
    }

    fn call(ids: &[&str]) -> Message {
        Message::assistant(
            ids.iter()
                .map(|id| ContentBlock::ToolUse {
                    id: (*id).into(),
                    name: "tap".into(),
                    input: json!({}),
                })
                .collect(),
        )
    }

    fn results(ids: &[&str]) -> Message {
        Message::user(
            ids.iter()
                .map(|id| {
                    ContentBlock::tool_result(*id, vec![ToolResultPart::Text { text: "ok".into() }])
                })
                .collect(),
        )
    }

    fn texts(m: &[Message]) -> Vec<String> {
        m.iter().map(|m| format!("{:?}", m.role)).collect()
    }

    #[test]
    fn short_history_is_unchanged() {
        let h = vec![user("a"), assistant("b")];
        assert_eq!(trim_history(&h, 2).len(), 2);
        assert_eq!(trim_history(&h, 10).len(), 2);
    }

    #[test]
    fn plain_alternating_history() {
        let h = vec![
            user("1"),
            assistant("1"),
            user("2"),
            assistant("2"),
            user("3"),
        ];
        // The last 4 start with an assistant turn: move to the next user message.
        let t = trim_history(&h, 4);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0], user("2"));
        assert_eq!(trim_history(&h, 3), &h[2..]);
    }

    #[test]
    fn never_orphans_tool_results() {
        let h = vec![
            user("go"),
            call(&["a", "b"]),
            results(&["a", "b"]),
            call(&["c"]),
            results(&["c"]),
            assistant("done"),
            user("next"),
            assistant("ok"),
        ];
        // Windows starting on a tool result or an assistant turn move forward to `next`.
        for max in 2..=6 {
            let t = trim_history(&h, max);
            assert_eq!(t[0], user("next"), "max {max}: {:?}", texts(t));
        }
        // Room for everything from the first user message.
        assert_eq!(trim_history(&h, 8).len(), 8);
        assert_eq!(trim_history(&h, 7)[0], user("next"));
    }

    #[test]
    fn falls_back_to_newest_safe_start_before_the_window() {
        let h = vec![
            user("go"),
            call(&["a"]),
            results(&["a"]),
            call(&["b"]),
            results(&["b"]),
        ];
        // No safe start in the last 2; the newest one before them is the first message.
        assert_eq!(trim_history(&h, 2), &h[..]);
    }

    #[test]
    fn no_safe_start_is_empty() {
        let h = vec![assistant("x"), results(&["a"]), assistant("y")];
        assert!(trim_history(&h, 1).is_empty());
    }
}
