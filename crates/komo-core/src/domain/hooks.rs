//! Typed hook points on the agent loop and the tool pipeline.
//!
//! Ported from the deepseek-harness event model, narrowed to komo's shape:
//! `ToolHook` mirrors `tools/pre-execute` / `tools/post-execute`, `TurnHook`
//! mirrors `turn/start` / `turn/end`. Hooks are registered at wiring time and
//! frozen with the catalog they observe.
//!
//! Cache discipline (the reason these traits are observe-or-veto, never
//! rewrite-the-prefix): a hook acts only on the request **suffix**. A denial
//! becomes the call's outcome content — append-only, so it never invalidates
//! the provider's cached prompt prefix. Hooks deliberately have no way to
//! mutate the system prompt or the tool schema set.

use async_trait::async_trait;

use crate::domain::llm::{ToolCallReq, ToolOutcome};

/// What a [`ToolHook::pre_execute`] decides about a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// Let the call proceed (subject to the remaining hooks).
    Continue,
    /// Refuse the call. The message becomes the call's outcome content — the
    /// model sees it and can adjust, exactly like a policy denial. Later hooks
    /// are not consulted (waterfall short-circuit).
    Deny(String),
}

/// Intercepts tool calls around execution. `pre_execute` runs before catalog
/// lookup and can veto; `post_execute` observes the resolved outcome
/// (including error content). Hooks run in registration order; the first
/// `Deny` wins.
///
/// Hooks must be fast: they sit on the turn's critical path and are awaited
/// serially. Anything slow belongs in a tool, not a hook.
#[async_trait]
pub trait ToolHook: Send + Sync {
    /// Stable name, for logs and diagnostics.
    fn name(&self) -> &'static str;

    /// Runs before the call executes. Default: allow.
    async fn pre_execute(&self, _call: &ToolCallReq) -> HookDecision {
        HookDecision::Continue
    }

    /// Runs after the call resolved (success, error content, or a veto by an
    /// earlier hook — whatever the model will see). Observe-only.
    async fn post_execute(&self, _call: &ToolCallReq, _outcome: &ToolOutcome) {}
}

/// Observes turn lifecycle boundaries on the agent loop. Observe-only: turn
/// control (budget, cancel, spin) stays in the loop itself.
#[async_trait]
pub trait TurnHook: Send + Sync {
    /// Stable name, for logs and diagnostics.
    fn name(&self) -> &'static str;

    /// The turn's agent loop is about to drive its first model round.
    async fn turn_started(&self, _session_id: &str) {}

    /// The turn produced its reply (not called for a failed/cancelled turn —
    /// those surface through the run ledger, which is the audit trail).
    async fn turn_finished(&self, _session_id: &str, _reply: &str) {}
}
