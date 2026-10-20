//! Builds finished blocks from streamed deltas, and drives a wire event stream into
//! [`StreamEvent`]s.

use std::{collections::BTreeMap, collections::VecDeque, time::Duration};

use futures::{Stream, StreamExt, stream::BoxStream};
use serde_json::Value;

use crate::{ApiType, BlockStart, ContentBlock, Error, Result, StreamEvent};

/// A block still being streamed.
pub(crate) enum Partial {
    Text(String),
    Thinking {
        api: ApiType,
        id: Option<String>,
        text: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Raw {
        api: ApiType,
        value: Value,
    },
}

/// Tracks open blocks by index and keeps finished ones in index order.
#[derive(Default)]
pub(crate) struct Assembler {
    open: BTreeMap<usize, Partial>,
    done: BTreeMap<usize, ContentBlock>,
}

impl Assembler {
    pub fn start(&mut self, index: usize, partial: Partial) -> StreamEvent {
        let block = match &partial {
            Partial::Text(_) => BlockStart::Text,
            Partial::Thinking { .. } => BlockStart::Thinking,
            Partial::ToolUse { id, name, .. } => BlockStart::ToolUse {
                id: id.clone(),
                name: name.clone(),
            },
            Partial::Raw { .. } => BlockStart::Raw,
        };
        self.open.insert(index, partial);
        StreamEvent::BlockStart { index, block }
    }

    pub fn text(&mut self, index: usize, delta: &str) -> Option<StreamEvent> {
        match self.open.get_mut(&index) {
            Some(Partial::Text(t)) => {
                t.push_str(delta);
                Some(StreamEvent::TextDelta {
                    index,
                    text: delta.to_owned(),
                })
            }
            _ => None,
        }
    }

    pub fn thinking(&mut self, index: usize, delta: &str) -> Option<StreamEvent> {
        match self.open.get_mut(&index) {
            Some(Partial::Thinking { text, .. }) => {
                text.push_str(delta);
                Some(StreamEvent::ThinkingDelta {
                    index,
                    text: delta.to_owned(),
                })
            }
            _ => None,
        }
    }

    pub fn signature(&mut self, index: usize, delta: &str) {
        if let Some(Partial::Thinking { signature, .. }) = self.open.get_mut(&index) {
            signature.get_or_insert_with(String::new).push_str(delta);
        }
    }

    pub fn tool_input(&mut self, index: usize, delta: &str) -> Option<StreamEvent> {
        match self.open.get_mut(&index) {
            Some(Partial::ToolUse { json, .. }) => {
                json.push_str(delta);
                Some(StreamEvent::ToolInputDelta {
                    index,
                    partial_json: delta.to_owned(),
                })
            }
            _ => None,
        }
    }

    /// Finishes a block from its accumulated deltas.
    pub fn stop(&mut self, index: usize) -> Result<StreamEvent> {
        let partial = self
            .open
            .remove(&index)
            .ok_or_else(|| Error::Decode(format!("stop for unknown block {index}")))?;
        let block = finish(partial)?;
        Ok(self.stop_with(index, block))
    }

    /// Finishes a block with an authoritative value sent by the API.
    pub fn stop_with(&mut self, index: usize, block: ContentBlock) -> StreamEvent {
        self.open.remove(&index);
        self.done.insert(index, block.clone());
        StreamEvent::BlockStop { index, block }
    }

    /// Finished blocks in index order. Blocks the API never closed are dropped.
    pub fn into_content(self) -> Vec<ContentBlock> {
        self.done.into_values().collect()
    }
}

fn finish(partial: Partial) -> Result<ContentBlock> {
    Ok(match partial {
        Partial::Text(text) => ContentBlock::Text { text },
        Partial::Thinking {
            api,
            id,
            text,
            signature,
        } => ContentBlock::Thinking {
            api,
            id,
            text,
            signature,
        },
        Partial::ToolUse { id, name, json } => ContentBlock::ToolUse {
            input: parse_tool_input(&json)?,
            id,
            name,
        },
        Partial::Raw { api, value } => ContentBlock::Raw { api, value },
    })
}

/// Parses tool-call arguments; an empty string is an empty object.
pub(crate) fn parse_tool_input(json: &str) -> Result<Value> {
    if json.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(json).map_err(|e| Error::Decode(format!("tool input: {e}")))
}

/// Runs a wire event stream through `parse` until it yields [`StreamEvent::Done`].
///
/// Fails with [`Error::Timeout`] when no wire event arrives within `idle`, and with
/// [`Error::Decode`] when the wire stream ends before `Done`.
pub(crate) fn drive<S, P>(
    wire: S,
    parse: P,
    idle: Duration,
) -> BoxStream<'static, Result<StreamEvent>>
where
    S: Stream<Item = Result<Value>> + Send + Unpin + 'static,
    P: FnMut(Value) -> Result<Vec<StreamEvent>> + Send + 'static,
{
    struct State<S, P> {
        wire: S,
        parse: P,
        queue: VecDeque<StreamEvent>,
        finished: bool,
    }
    let state = State {
        wire,
        parse,
        queue: VecDeque::new(),
        finished: false,
    };
    futures::stream::unfold(state, move |mut st| async move {
        loop {
            if let Some(ev) = st.queue.pop_front() {
                if matches!(ev, StreamEvent::Done(_)) {
                    st.finished = true;
                    st.queue.clear();
                }
                return Some((Ok(ev), st));
            }
            if st.finished {
                return None;
            }
            let next = match tokio::time::timeout(idle, st.wire.next()).await {
                Err(_) => Err(Error::Timeout),
                Ok(None) => Err(Error::Decode("stream ended before completion".into())),
                Ok(Some(Err(e))) => Err(e),
                Ok(Some(Ok(v))) => (st.parse)(v),
            };
            match next {
                Ok(events) => st.queue.extend(events),
                Err(e) => {
                    st.finished = true;
                    return Some((Err(e), st));
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn assembles_interleaved_tool_calls() {
        let mut a = Assembler::default();
        a.start(
            0,
            Partial::ToolUse {
                id: "a".into(),
                name: "tap".into(),
                json: String::new(),
            },
        );
        a.start(
            1,
            Partial::ToolUse {
                id: "b".into(),
                name: "tap".into(),
                json: String::new(),
            },
        );
        a.tool_input(0, "{\"id\":");
        a.tool_input(1, "{\"id\":\"y\"}");
        a.tool_input(0, "\"x\"}");
        let StreamEvent::BlockStop { block: b1, .. } = a.stop(1).unwrap() else {
            panic!()
        };
        a.stop(0).unwrap();
        assert_eq!(
            b1,
            ContentBlock::ToolUse {
                id: "b".into(),
                name: "tap".into(),
                input: json!({"id": "y"})
            }
        );
        let content = a.into_content();
        assert_eq!(content.len(), 2);
        assert!(matches!(&content[0], ContentBlock::ToolUse { id, .. } if id == "a"));
        assert_eq!(content[1], b1);
    }

    #[test]
    fn empty_tool_input_is_empty_object() {
        assert_eq!(parse_tool_input("").unwrap(), json!({}));
        assert!(parse_tool_input("{").is_err());
    }

    #[tokio::test]
    async fn drive_times_out_and_detects_early_end() {
        let wire = futures::stream::pending::<Result<Value>>();
        let mut s = drive(wire, |_| Ok(vec![]), Duration::from_millis(10));
        assert!(matches!(s.next().await, Some(Err(Error::Timeout))));
        assert!(s.next().await.is_none());

        let wire = futures::stream::iter(vec![Ok(json!({}))]);
        let mut s = drive(wire, |_| Ok(vec![]), Duration::from_secs(1));
        assert!(matches!(s.next().await, Some(Err(Error::Decode(_)))));
    }
}
