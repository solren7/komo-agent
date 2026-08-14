//! komo's own conversation types, as sent to and received from a provider.
//!
//! These replace the `rig` message types komo used to build turns out of. They
//! exist because the wire codecs (`responses`, `messages`) need one shape to
//! translate *from*, and because the pieces of the agent that reason about a
//! turn's history — the context-overflow reclaim, the prefix-cache invariant —
//! are far easier to write against a flat enum than against a provider's
//! nested content model.
//!
//! Deliberately smaller than any provider's schema: komo only ever sends text,
//! tool calls, tool results, and opaque reasoning. A tool's model-facing result
//! is plain text by contract (`domain::tool::ToolOutput::text`), so a tool
//! result is a `String` rather than a content array.

use serde::{Deserialize, Serialize};

/// One message in the conversation sent to the provider.
///
/// Serde on this family (and [`Completion`]) exists for exactly one consumer:
/// the turn journal, which persists an in-flight turn's provider-level state so
/// an interrupted turn can be resumed byte-faithfully. The wire codecs never
/// serialize these shapes — each renders its own provider format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Turn {
    User(Vec<UserBlock>),
    Assistant {
        /// The provider's item id for this message, echoed back when the
        /// provider correlates on it. `None` for a message komo rendered from
        /// its own stored transcript (which keeps no provider ids).
        id: Option<String>,
        blocks: Vec<AssistantBlock>,
    },
}

impl Turn {
    /// A plain-text user message.
    pub fn user(text: impl Into<String>) -> Self {
        Turn::User(vec![UserBlock::Text(text.into())])
    }

    /// A plain-text assistant message, as re-rendered from komo's transcript.
    pub fn assistant(text: impl Into<String>) -> Self {
        Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::Text(text.into())],
        }
    }
}

/// A block inside a user message: either something the human said or the result
/// of a tool the model asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserBlock {
    Text(String),
    ToolResult {
        /// The provider's item id for the originating call (Anthropic keys on
        /// this).
        id: String,
        /// The provider's call id (the Responses API keys on this).
        call_id: Option<String>,
        text: String,
    },
}

/// A block inside an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AssistantBlock {
    Text(String),
    ToolCall {
        id: String,
        call_id: Option<String>,
        name: String,
        /// The raw JSON arguments string, exactly as the provider emitted it.
        /// Kept unparsed so a round-trip through history is byte-faithful.
        args: String,
    },
    /// Provider-opaque reasoning, echoed back verbatim on the next round so a
    /// reasoning model keeps its chain of thought across a tool loop. komo
    /// never reads the contents — `encrypted` is ciphertext, and `summary` is
    /// only for display.
    Reasoning(Reasoning),
}

/// A reasoning item as the provider issued it. Round-tripped unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Reasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Human-readable summary chunks, when the provider emits them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<String>,
    /// The opaque blob that actually carries the reasoning across rounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<String>,
}

/// A tool declaration advertised to the provider. Only the schema crosses the
/// wire — komo dispatches every call itself.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Tokens one provider response reported. Zero means *unknown* as much as it
/// means none, matching `domain::llm::TokenUsage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    /// Prompt tokens served from the provider's prefix cache, when reported.
    /// Logged rather than billed — it is how we tell whether the cache-warming
    /// work in `RigLlm::assemble` is actually paying off.
    pub cached_input: i64,
}

/// One completed model round-trip: the assistant message it produced, plus what
/// it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub id: Option<String>,
    pub blocks: Vec<AssistantBlock>,
    #[serde(default)]
    pub usage: Usage,
}

impl Completion {
    /// Concatenate the text blocks — the final answer for a tool-less call.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            if let AssistantBlock::Text(t) = block {
                out.push_str(t);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_and_completion_survive_a_serde_round_trip() {
        let turns = vec![
            Turn::User(vec![
                UserBlock::Text("hi".into()),
                UserBlock::ToolResult {
                    id: "item_1".into(),
                    call_id: Some("call_1".into()),
                    text: "result body".into(),
                },
            ]),
            Turn::Assistant {
                id: Some("msg_1".into()),
                blocks: vec![
                    AssistantBlock::Reasoning(Reasoning {
                        id: Some("rs_1".into()),
                        summary: vec!["thinking…".into()],
                        encrypted: Some("opaque-blob".into()),
                    }),
                    AssistantBlock::Text("let me check".into()),
                    AssistantBlock::ToolCall {
                        id: "item_2".into(),
                        call_id: None,
                        name: "shell".into(),
                        args: r#"{"command":"ls"}"#.into(),
                    },
                ],
            },
        ];
        let json = serde_json::to_string(&turns).unwrap();
        let back: Vec<Turn> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, turns);

        let completion = Completion {
            id: Some("msg_2".into()),
            blocks: vec![AssistantBlock::Text("done".into())],
            usage: Usage {
                input: 10,
                output: 5,
                cached_input: 8,
            },
        };
        let json = serde_json::to_string(&completion).unwrap();
        let back: Completion = serde_json::from_str(&json).unwrap();
        assert_eq!(back, completion);
    }
}
