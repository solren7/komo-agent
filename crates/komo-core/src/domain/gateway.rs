use std::sync::Arc;

use async_trait::async_trait;

use super::run::Run;

/// Processes one inbound message for a session and returns the agent's reply.
///
/// This is the seam between an ingress channel (which knows a transport — a unix
/// socket, HTTP, a chat platform) and the agent. Channels depend only on this
/// trait; `AgentRuntime` is the production implementation.
#[async_trait]
pub trait MessageHandler: Send + Sync {
    async fn handle(&self, session_id: &str, input: String) -> anyhow::Result<String>;

    /// Continue an interrupted run from its turn journal, in place — no new
    /// user message, the tool rounds already paid for replayed rather than
    /// re-run. `Ok(None)` = this handler can't (no journal, or the run isn't
    /// continuable); the caller falls back to a digest-primed [`handle`] turn.
    ///
    /// [`handle`]: MessageHandler::handle
    async fn resume_interrupted(&self, _run: &Run) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// Sends a message back into the conversation a turn belongs to, on the same
/// channel it arrived on.
///
/// A turn's eventual reply is returned from [`MessageHandler::handle`], but some
/// work needs to emit prose *mid-turn* — e.g. an approval prompt the user must
/// answer before a tool proceeds. The channel provides a `ReplySink` for the
/// active turn; the agent reaches it through the ambient session context (see
/// `services::tool_execution::current_session`).
#[async_trait]
pub trait ReplySink: Send + Sync {
    async fn send(&self, text: &str) -> anyhow::Result<()>;

    /// Send an image (e.g. a login QR) back to the conversation. Defaults to an
    /// error for text-only channels; callers should treat failure as "this
    /// channel can't show an image" and fall back to text.
    async fn send_photo(&self, _png: Vec<u8>, _caption: &str) -> anyhow::Result<()> {
        anyhow::bail!("this channel does not support sending images")
    }
}

/// Hands a running turn the user messages that arrived while it was working.
///
/// A turn takes minutes; the user watching it work will say things mid-flight —
/// most valuably a correction ("no, B not A"). Queuing those for the *next*
/// turn means the correction only lands after the agent finished going the
/// wrong way, which is the one moment it is worthless. The agent loop drains
/// this between rounds instead and folds what it gets into the round's
/// messages, so the model sees it before choosing its next move.
///
/// Implemented by the gateway dispatcher over the same queue
/// [`MessageHandler`]-dispatched turns drain from, so a message is delivered
/// exactly once: whoever takes it first — this turn, or the next one — owns it.
pub trait InterjectSource: Send + Sync {
    /// Take everything queued for this turn's session right now, oldest first;
    /// empty when nothing is waiting. Never blocks — it is polled on the hot
    /// path between rounds.
    fn take(&self) -> Vec<String>;
}

/// Drives an interactive WeChat QR login, delivering the QR to `sink` (as a
/// photo where the channel supports it). Implemented in infra and invoked by
/// the gateway dispatcher on `/wechat login`, so the WeChat channel can be
/// provisioned from an existing chat (e.g. Telegram) without host shell access.
/// Returns the logged-in user id on success.
#[async_trait]
pub trait WeChatLogin: Send + Sync {
    async fn run(&self, sink: Arc<dyn ReplySink>) -> anyhow::Result<String>;
}
