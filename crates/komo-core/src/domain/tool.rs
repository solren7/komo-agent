use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::domain::context::ToolContext;

/// A tool's result, split into the three views opencode v2 keeps separate: a
/// short human/UI `title`, the `text` the model sees, and `structured` data for
/// programmatic/ledger consumers (unused by the model, so it costs no tokens).
///
/// Most tools only produce text — [`ToolOutput::text`] is the one-liner for
/// that. `title`/`structured` are opt-in via the builders.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub title: Option<String>,
    pub text: String,
    pub structured: Value,
}

impl ToolOutput {
    /// The common case: model-facing text, no title, no structured view.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            title: None,
            text: text.into(),
            structured: Value::Null,
        }
    }

    /// Attach a short human/UI title (shown in logs and, later, the TUI).
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Attach a structured view for programmatic/ledger consumers.
    pub fn with_structured(mut self, structured: Value) -> Self {
        self.structured = structured;
        self
    }
}

/// A tool-call failure, classified so the executor can render it the right way.
///
/// [`InvalidInput`](ToolError::InvalidInput) and [`Denied`](ToolError::Denied)
/// are **recoverable**: the executor turns them into model-facing content (a
/// "rewrite the arguments" nudge, or the denial reason) and never retries. Only
/// [`Failed`](ToolError::Failed) flows through the transient-retry path — it
/// carries the underlying `anyhow::Error` (and any [`TransientError`]) so the
/// existing retry classification is unchanged. `?` on an `anyhow::Error` inside
/// a tool auto-wraps to `Failed` via the `From` impl.
#[derive(Debug)]
pub enum ToolError {
    /// Arguments did not match the tool's schema. Not retried.
    InvalidInput(String),
    /// The action was refused (approval denied / policy block). Not retried.
    Denied(String),
    /// A genuine execution failure — retried if transient (see [`RetryHint`]).
    Failed(anyhow::Error),
    /// The call never reported back, so whether it took effect is **unknown**.
    ///
    /// A wall-clock timeout, or an ambiguous transport error on a tool that is
    /// not idempotent: the request may well have arrived and been applied, and
    /// only the answer was lost. Distinct from [`Failed`](Self::Failed) because
    /// the two call for opposite next moves — a failure invites a retry, an
    /// unknown outcome requires checking the target's state first.
    ///
    /// Never retried automatically, and structurally so: the retry arm matches
    /// `Failed` alone. That used to be arranged by wording the message to dodge
    /// a substring classifier, which is a rule one careless edit can undo.
    Uncertain(anyhow::Error),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::InvalidInput(m) => write!(f, "invalid tool input: {m}"),
            ToolError::Denied(m) => write!(f, "{m}"),
            ToolError::Failed(e) => write!(f, "{e:#}"),
            ToolError::Uncertain(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<anyhow::Error> for ToolError {
    fn from(e: anyhow::Error) -> Self {
        ToolError::Failed(e)
    }
}

/// The per-call ceiling for a tool that can park on an interactive approval
/// prompt (`write`, `edit`, `apply_patch`, `shell`, `cron`, `homeassistant`,
/// `skill install`).
///
/// A chat approval waits up to five minutes for the user
/// (`agent::interaction::APPROVAL_TIMEOUT`), so a shorter ceiling would abort the
/// call *while the human is still deciding* — the worst possible outcome: the
/// user approves, and nothing happens. This is that timeout plus room for the
/// work itself.
pub const APPROVAL_BOUND: std::time::Duration = std::time::Duration::from_secs(7 * 60);

/// Decode a tool's typed arguments from the JSON `Value` the executor parsed,
/// mapping a schema mismatch to the canonical [`ToolError::InvalidInput`] — the
/// one place tool arguments are validated, replacing each tool's hand-rolled
/// `serde_json::from_str` + ad-hoc error string.
pub fn parse_args<T: DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone()).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

/// A failure's retry-safety, classified at its source (where the typed cause is
/// still intact — e.g. a `reqwest::Error`, before it is flattened to a string)
/// and carried on the error via [`TransientError`]. The retry layer
/// (`services::tool_execution`) reads this in preference to sniffing the error's
/// Display text. Mirrors the buckets that layer acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryHint {
    /// The request provably never reached the server (connection refused, DNS
    /// failure). Safe to retry for any tool — no side effect can have landed.
    Connection,
    /// Landed-or-not is ambiguous (timeout, 5xx, rate-limit). Retry only an
    /// idempotent tool, so a side effect is never applied twice.
    Ambiguous,
}

/// An error that classifies its own retry-safety via a [`RetryHint`]. A tool
/// builds one at the failure's source (see `tools::http`) so the retry layer
/// decides from a typed signal rather than a heuristic string match; anything
/// that doesn't classify itself falls back to that heuristic.
#[derive(Debug)]
pub struct TransientError {
    pub hint: RetryHint,
    pub message: String,
}

impl TransientError {
    pub fn new(hint: RetryHint, message: impl Into<String>) -> Self {
        Self {
            hint,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TransientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransientError {}

/// What a model is told when a call's outcome is unknown — it was issued, and
/// whether it landed cannot be established.
///
/// One sentence, one place, because there are two ways to arrive here and they
/// must not give opposite advice: a call that timed out or failed ambiguously
/// (`ToolError::Uncertain`), and a call whose result was lost to a crash and is
/// replayed on resume. The second used to say "re-issue the call if you still
/// need it" — the most dangerous moment to invite a blind retry, since a
/// mutation may already have been applied.
pub const UNCERTAIN_OUTCOME_ADVICE: &str = "It may or may not have taken effect — check the target's state before calling it again; \
     repeating it blindly can apply the same change twice.";

/// Marks a failure as one where the call may still have taken effect.
///
/// Carried through `anyhow` the same way [`TransientError`] is, so the fact
/// survives being flattened into an error chain: by the time a call reaches the
/// run ledger the [`ToolError`] variant is long gone, and "failed" and "may have
/// landed" need different answers from whoever reads it later.
#[derive(Debug)]
pub struct UncertainOutcome {
    pub message: String,
}

impl UncertainOutcome {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Whether this error (or anything it wraps) is an uncertain outcome.
    pub fn marks(error: &anyhow::Error) -> bool {
        error.downcast_ref::<Self>().is_some()
    }
}

impl std::fmt::Display for UncertainOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UncertainOutcome {}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// JSON Schema describing this tool's arguments, exposed to the LLM for
    /// function calling. Defaults to "no arguments". Tools that take arguments
    /// override this and parse the matching JSON object from `execute`'s input.
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    /// Execute the tool with the parsed JSON `input` and the explicit per-call
    /// [`ToolContext`] (session + run + approver). Returns a structured
    /// [`ToolOutput`]; recoverable problems use [`ToolError::InvalidInput`] /
    /// [`ToolError::Denied`], which the executor turns into content the model
    /// can act on rather than retrying.
    ///
    /// Arguments are decoded with [`parse_args`] — one canonical
    /// "rewrite the arguments" error instead of each tool's own phrasing.
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;

    /// Wall-clock ceiling for **one call** of this tool, overriding the
    /// executor's default (`tool_timeout_secs`, 120s).
    ///
    /// `None` — the default — accepts that ceiling, which exists to catch a
    /// *hang*. Override it when waiting is the normal case rather than a
    /// symptom, or the call is killed mid-legitimate-work:
    ///
    /// * a whole sub-agent completion (`delegate`) can outlast one round-trip;
    /// * `shell` honors a caller-supplied `timeout` of up to ten minutes, and its
    ///   own timeout must fire first so the model gets "retry with a bigger
    ///   timeout" instead of an opaque abort;
    /// * anything that prompts for approval has to outlast a human reading it —
    ///   see [`APPROVAL_BOUND`].
    fn max_duration(&self) -> Option<std::time::Duration> {
        None
    }

    /// Whether `call` is safe to retry after a transient failure whose
    /// side-effect status is *ambiguous* — a timeout or 5xx that may already
    /// have landed and applied server-side. Read-only tools (`web_fetch`,
    /// `web_search`) return `true`; any tool that can mutate external state
    /// keeps the default `false`, so a retry can never double-apply an effect
    /// (e.g. fire a Home Assistant service or run a shell command twice).
    ///
    /// Connection-level failures (the request provably never reached the
    /// server — connection refused, DNS failure) are retried regardless of
    /// this flag; see `services::tool_execution`.
    fn idempotent(&self) -> bool {
        false
    }

    /// Sanitize the raw arguments before they are written to the run ledger
    /// (`services::tool_execution`). The ledger stores tool
    /// args verbatim by default (this identity impl); tools carrying sensitive
    /// payloads override it so secrets/large bodies never land in `state.db`.
    /// `shell` scrubs secret-looking substrings, `file` drops write bodies.
    fn redact_args(&self, args: &str) -> String {
        args.to_string()
    }
}
