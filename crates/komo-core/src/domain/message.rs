use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp: i64,
    /// Model-facing footnote on an assistant message: what tools that turn ran
    /// (`domain::run::tool_digest`). Kept **beside** `content` rather than inside
    /// it because the two have different audiences — `content` is what the user
    /// reads in every client, this is what the next turn's model needs in order
    /// to know the turn used tools at all. Empty for user messages, for assistant
    /// turns that called no tools, and for rows written before the column existed.
    #[serde(default)]
    pub tool_note: String,
}

/// One tool call, as the transcript records it.
///
/// **Not part of what the model is shown.** The model's account of a turn's tool
/// use is the assistant message's [`Message::tool_note`]; this is the account
/// for everyone else — an operator reading the file, a client rendering the
/// work, a script auditing what ran. Keeping it out of the message history is
/// what lets the transcript hold the whole conversation without changing a
/// single byte the model sees.
///
/// `args` is redacted and `result` capped, because both are taken from the same
/// values the run ledger records — written at the same point, from the same
/// data, so the file and the ledger cannot disagree about what happened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolEntry {
    pub name: String,
    /// The call's arguments, redacted by the tool (`Tool::redact_args`).
    pub args: String,
    /// What the call answered — the model-facing text, already capped.
    pub result: String,
    pub ok: bool,
    /// Wall-clock duration. 0 reads as unknown, as it does in the ledger.
    #[serde(default)]
    pub elapsed_ms: i64,
    pub timestamp: i64,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            timestamp: now(),
            tool_note: String::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            timestamp: now(),
            tool_note: String::new(),
        }
    }

    /// Attach the turn's tool digest (builder form, so the runtime can compose a
    /// finished assistant message in one expression).
    pub fn with_tool_note(mut self, note: impl Into<String>) -> Self {
        self.tool_note = note.into();
        self
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
