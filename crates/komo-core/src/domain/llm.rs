use std::sync::Arc;

use async_trait::async_trait;

use super::session::Session;
use super::turn_journal::{JournalEntry, TurnJournal};

/// One model round-trip's outcome inside komo's own tool loop. The loop lives
/// in `AgentRuntime` (not rig — roadmap §7), so it can insert control points
/// between rounds: either the model produced a final answer, or it requested
/// tools the runtime must execute and feed back.
pub enum Step {
    Final(String),
    ToolCalls {
        calls: Vec<ToolCallReq>,
        /// Text the model wrote in the *same* assistant turn as the calls — the
        /// "let me check the config first" narration providers emit alongside
        /// tool use. Not part of the final reply (the turn hasn't answered yet),
        /// but the only place a watcher can see the model's reasoning as it
        /// works, and the honest thing to say when the round budget cuts the
        /// turn short. Empty for a model that only emits calls.
        text: String,
    },
}

/// Tokens one turn spent, accumulated across its model round-trips. Zero means
/// *unknown* (a provider that reports no usage) as much as it means "none" —
/// the ledger treats both as absent, same convention as `elapsed_ms`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
}

impl TokenUsage {
    /// Fold another round's usage in. Saturating: a provider reporting nonsense
    /// must not panic a turn in release *or* debug.
    pub fn add(&mut self, other: TokenUsage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0 && self.output == 0
    }
}

/// A tool call the model requested. Rig-agnostic on purpose — the seam carries
/// no rig types. `id`/`call_id` are the provider's correlation handles, echoed
/// back verbatim in the tool result (Anthropic keys on `id`, OpenAI on
/// `call_id`); `args` is the JSON arguments object for the tool's `execute`.
pub struct ToolCallReq {
    pub id: String,
    pub call_id: Option<String>,
    pub name: String,
    pub args: String,
}

/// The result of executing one [`ToolCallReq`], threaded back to the model on
/// the next round. Carries the same correlation handles back.
pub struct ToolOutcome {
    pub id: String,
    pub call_id: Option<String>,
    pub content: String,
}

/// Receives assistant output as the provider produces it, mid-round.
///
/// A round's [`Step`] only exists once the round *finishes*, which for a
/// reasoning model on a long tool chain can be tens of seconds of silence. This
/// is the seam that lets a watching client see the work as it happens instead:
/// the backend calls it per streamed chunk, and the runtime forwards those onto
/// the turn's event sink.
///
/// Fire-and-forget and synchronous, like
/// [`ToolEventSink`](crate::domain::events::ToolEventSink): it is called from
/// inside the provider's stream loop, so it must never block or await. Absent
/// (`None`) for every turn with no watcher, which is the common case — an
/// unwatched turn pays nothing for this.
pub trait DeltaSink: Send + Sync {
    /// A chunk of the assistant's visible answer.
    fn text(&self, delta: &str);
    /// A chunk of the model's reasoning, when the provider streams a summary of
    /// it. Defaulted to a no-op: this is the interesting one to watch on a
    /// reasoning model (most of a round's latency is here, before any visible
    /// text exists), but a sink that only cares about the answer can ignore it.
    fn reasoning(&self, _delta: &str) {}
}

/// Drives one user turn as a sequence of model round-trips. Created by
/// [`LlmClient::begin_turn`], which assembles the per-turn system prompt and
/// memory injection *once* (not per round). The runtime calls [`first`] to get
/// the opening round, executes any requested tools, then [`step`]s their
/// results back until a [`Step::Final`].
///
/// [`first`]: TurnDriver::first
/// [`step`]: TurnDriver::step
#[async_trait]
pub trait TurnDriver: Send {
    /// The first model round-trip for this turn.
    async fn first(&mut self) -> anyhow::Result<Step>;
    /// Feed the previous round's tool results back and get the next round-trip.
    ///
    /// `interjected` carries anything the user said while the round was running
    /// (see [`InterjectSource`](crate::domain::gateway::InterjectSource)),
    /// already merged into one message. It rides in the **same** user message as
    /// the tool results rather than a separate one: a turn's history has to stay
    /// user/assistant-alternating, and appending bytes to the message that was
    /// being written anyway costs the provider's prompt cache nothing.
    async fn step(
        &mut self,
        results: Vec<ToolOutcome>,
        interjected: Option<String>,
    ) -> anyhow::Result<Step>;
    /// Tokens spent across every round this driver has run so far. Read by the
    /// runtime once the turn ends, for the ledger. Defaults to zero — a backend
    /// whose provider reports no usage simply records nothing.
    fn usage(&self) -> TokenUsage {
        TokenUsage::default()
    }
}

/// Abstraction over a large-language-model backend.
///
/// The domain layer only knows this trait; concrete providers (DeepSeek,
/// OpenAI, an internal gateway, ...) live in `infra/`.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Produce an assistant reply for a tool-less sub-agent conversation — the
    /// `delegate` tool, the reflective reviewer, the briefing sweep. These
    /// expose no tools, so the whole exchange is a single completion.
    async fn complete(&self, session: &Session) -> anyhow::Result<String>;

    /// Begin a tool-using turn for the main agent. The returned [`TurnDriver`]
    /// lets `AgentRuntime` own the multi-step tool loop, so planner control
    /// points (budget, clarify, resume — roadmap §7) live there.
    ///
    /// `deltas` is where the backend streams this turn's output as it is
    /// produced; `None` when nothing is watching. A backend is free to ignore it
    /// (the default one-shot driver does).
    ///
    /// `journal` is the turn journal to record this turn's provider-level state
    /// into (see `domain::turn_journal`); `None` for callers that don't keep
    /// one (aux paths, tests). Backends without provider-shaped state ignore it.
    ///
    /// The default is a single-shot driver wrapping [`complete`](LlmClient::complete):
    /// it answers in one round with no tool calls. Tool-less backends (and test
    /// stubs) inherit this for free; the real provider client overrides it with a
    /// tool-looping driver.
    async fn begin_turn(
        &self,
        session: &Session,
        _deltas: Option<Arc<dyn DeltaSink>>,
        _journal: Option<Arc<dyn TurnJournal>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        let reply = self.complete(session).await?;
        Ok(Box::new(OneShotDriver(Some(reply))))
    }

    /// Reopen an interrupted turn from its journal rows, returning a driver
    /// that continues from exactly where the turn stopped (same model, same
    /// prompt bytes, tool rounds already paid for replayed from the journal
    /// instead of re-run).
    ///
    /// `journal` records the *continuation* under its own run, so a second
    /// interruption resumes from the continuation rather than from scratch.
    ///
    /// Default: unsupported — the caller falls back to the digest-primed fresh
    /// turn (`domain::run::resume_prompt`). Only the provider-backed client
    /// can rebuild provider-shaped state.
    async fn resume_turn(
        &self,
        _session: &Session,
        _entries: &[JournalEntry],
        _deltas: Option<Arc<dyn DeltaSink>>,
        _journal: Option<Arc<dyn TurnJournal>>,
    ) -> anyhow::Result<Box<dyn TurnDriver>> {
        anyhow::bail!("this backend cannot resume a turn from a journal")
    }
}

/// The default [`TurnDriver`]: yields one [`Step::Final`] (the precomputed
/// `complete` reply) and never requests tools.
struct OneShotDriver(Option<String>);

#[async_trait]
impl TurnDriver for OneShotDriver {
    async fn first(&mut self) -> anyhow::Result<Step> {
        Ok(Step::Final(self.0.take().unwrap_or_default()))
    }
    async fn step(
        &mut self,
        _results: Vec<ToolOutcome>,
        _interjected: Option<String>,
    ) -> anyhow::Result<Step> {
        Ok(Step::Final(self.0.take().unwrap_or_default()))
    }
}
