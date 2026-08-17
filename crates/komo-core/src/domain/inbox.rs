//! Durable inbound dedupe.
//!
//! Chat platforms deliver at-least-once. Telegram redelivers everything above
//! the last committed `getUpdates` offset, so a gateway that dies between
//! running a turn and advancing the offset sees the same message again on
//! restart. Feishu retries a message it thinks was not acked.
//!
//! Deduping in process memory does not survive that: the state that would have
//! recognised the redelivery died with the process. This is a durable record
//! instead — one row per inbound message, keyed by the platform's own id, so
//! "have I already handled this?" outlives a restart.
//!
//! The gate sits in front of *every* inbound message, not just the ones that
//! start a turn: a redelivered `/approve` would approve something twice, which
//! is worse than a duplicated question.

use async_trait::async_trait;

/// What identifies an inbound message on the platform that delivered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundOrigin {
    /// Channel name — `feishu` / `telegram` / `wechat` / `local`.
    pub platform: String,
    /// The platform's own id for this message.
    pub message_id: String,
}

impl InboundOrigin {
    pub fn new(platform: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            message_id: message_id.into(),
        }
    }

    /// An origin for input no platform delivered — a test, or a local caller
    /// that owns its own retry story. Nothing can redeliver it, so every call
    /// mints a fresh id and every claim is [`InboxClaim::Fresh`].
    pub fn local() -> Self {
        Self::new("local", uuid::Uuid::now_v7().to_string())
    }

    /// The storage key. Deterministic rather than a fresh UUIDv7 (the
    /// convention for every other key in state.db) precisely because dedupe
    /// *wants* the collision: the same platform message has to land on the
    /// same row, and the primary key is what makes that atomic.
    pub fn key(&self) -> String {
        format!("{}:{}", self.platform, self.message_id)
    }
}

/// Whether this delivery is the first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxClaim {
    /// Not seen before — the caller owns it and should handle it.
    Fresh,
    /// Already recorded by an earlier delivery. Drop it.
    Duplicate,
}

#[async_trait]
pub trait InboxRepository: Send + Sync {
    /// Record an inbound message and report whether it is new.
    ///
    /// `text` is stored alongside so a claimed-but-unfinished row carries
    /// enough to be re-delivered after a crash. Nothing reads it back yet —
    /// re-delivery arrives with the session event log, and storing the payload
    /// now is what lets that land without a schema change.
    async fn claim(
        &self,
        origin: &InboundOrigin,
        session_id: &str,
        text: &str,
    ) -> anyhow::Result<InboxClaim>;

    /// Mark a claimed message as handled.
    ///
    /// "Handled" currently means dispatched, not answered: the dispatcher
    /// spawns the turn and returns. Tightening this to "the turn's first event
    /// is durable" needs the event log that does not exist yet.
    async fn complete(&self, origin: &InboundOrigin) -> anyhow::Result<()>;
}
