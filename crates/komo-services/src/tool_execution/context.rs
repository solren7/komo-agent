//! Per-turn execution context glue.
//!
//! The context **value types** ([`SessionContext`], [`RunContext`],
//! [`ToolContext`]) now live in `domain::context` — they are pure values over
//! domain traits. This module re-exports them for path stability and adds two
//! service-layer concerns: the per-turn [`ToolTurnContext`] bundle the runtime
//! hands the executor, and the ambient-session task-local.
//!
//! The `SESSION` task-local now serves **only the approvers**: `ChatApprover`
//! and `PolicyApprover` resolve a prompt against the current conversation
//! without threading a context parameter through the domain `Approver` trait, so
//! the executor installs the turn's session around each spawned tool task and
//! they read [`current_session`]. **No tool reads it** — every tool takes its
//! session from the explicit `ctx.session` (tool trait v2). The run context is
//! likewise purely explicit: no ambient state decides whether a turn is
//! ledgered.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub use komo_core::domain::context::{RunContext, SessionContext, SessionOrigin, ToolContext};
use komo_core::domain::policy::Rule;

/// Everything the executor needs to know about the turn a round of tool calls
/// belongs to. Built once per turn by `AgentRuntime::run_agent_loop`.
#[derive(Clone)]
pub struct ToolTurnContext {
    pub session: SessionContext,
    /// `Some` when the turn is recorded in the run ledger (the main agent);
    /// `None` for callers without a ledger (rig's fallback path).
    pub run: Option<RunContext>,
    /// Cumulative tool-output budget for this turn. A round's tool calls share
    /// it via an `Arc`, so the total is tracked across the concurrent calls and
    /// across rounds — see [`TurnResultBudget`].
    pub budget: TurnResultBudget,
    /// Detects a turn that has stopped making progress — see [`SpinDetector`].
    pub spin: SpinDetector,
}

/// How many times in a row the same call may be requested before the executor
/// stops running it. The third one is refused: the first two already returned,
/// so their answer is in the transcript and a third execution cannot say
/// anything new.
const SPIN_REFUSE_AT: usize = 3;
/// A repeat this far in means the refusal did not land either, so the turn ends
/// rather than spending the rest of its round budget on the same call.
const SPIN_STOP_AT: usize = 4;

/// What [`SpinDetector`] says about one requested call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinVerdict {
    /// Not a repeat, or not enough of one yet — run it.
    Run,
    /// Same call as the two before it: don't run, tell the model why.
    Refuse,
    /// It kept asking after being refused; end the turn.
    Stop,
}

/// Catches a turn stuck re-issuing one tool call.
///
/// The round budget (`max_turns`) is a poor guard against this on its own: it is
/// deliberately generous (a real editing task runs dozens of rounds), so a model
/// looping on one failing call burns every round before anything notices, and
/// the user waits out the whole thing for an answer the second call already
/// determined. Identity is the call's name plus its exact arguments — same tool
/// with different arguments is progress, the same arguments twice is not — and
/// the streak resets the moment anything else is called, so a legitimate poll
/// interleaved with other work never trips it.
///
/// Counted over the turn's calls in dispatch order, across rounds. Shared with
/// the round's concurrent calls via an `Arc`, but the verdicts are decided
/// up front in order, so concurrency inside a round can't reorder the streak.
#[derive(Clone, Default)]
pub struct SpinDetector {
    /// `(signature, consecutive count)` — `None` until the first call.
    state: Arc<std::sync::Mutex<Option<(String, usize)>>>,
    /// Latched once any call reaches [`SpinVerdict::Stop`], so the agent loop
    /// can end the turn after the round finishes rather than each call having
    /// to report back.
    stopped: Arc<AtomicBool>,
}

impl SpinDetector {
    /// Whether a call this turn spun far enough that the turn should end.
    pub fn should_stop(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Record a requested call and say what to do with it.
    pub fn observe(&self, name: &str, args: &str) -> SpinVerdict {
        let signature = format!("{name}\u{0}{args}");
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let count = match state.as_mut() {
            Some((previous, count)) if *previous == signature => {
                *count += 1;
                *count
            }
            _ => {
                *state = Some((signature, 1));
                1
            }
        };
        match count {
            n if n >= SPIN_STOP_AT => {
                self.stopped.store(true, Ordering::Relaxed);
                SpinVerdict::Stop
            }
            n if n >= SPIN_REFUSE_AT => SpinVerdict::Refuse,
            _ => SpinVerdict::Run,
        }
    }
}

/// Per-turn cap on the *cumulative* bytes of tool output fed back to the model.
///
/// `max_tool_result_bytes` bounds one result; this bounds the whole turn, so a
/// long tool chain (dozens of rounds, each returning a capped result) can't
/// quietly accumulate past the context window and fail the turn only after its
/// side effects have already run. Once the running total crosses the cap, the
/// executor swaps each further result for a short note telling the model to stop
/// gathering and answer with what it has. Shared across a round's concurrent
/// calls via an `Arc<AtomicUsize>`; the counter is approximate under that
/// concurrency (a small overshoot is fine for a backstop).
#[derive(Clone)]
pub struct TurnResultBudget {
    consumed: Arc<AtomicUsize>,
    /// `0` disables the budget (unlimited).
    cap: usize,
}

impl TurnResultBudget {
    /// A budget capping cumulative tool output at `cap` bytes (`0` = unlimited).
    pub fn new(cap: usize) -> Self {
        Self {
            consumed: Arc::new(AtomicUsize::new(0)),
            cap,
        }
    }

    /// A disabled budget, for tests that assert on something other than the cap.
    /// Every production path seeds its budget from the executor's configured cap
    /// (`ToolExecutor::turn_result_cap`), so nothing outside tests opts out.
    #[cfg(test)]
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    /// Account for a tool result about to be returned. `Ok(out)` when there is
    /// still budget (the result is admitted and its size recorded); `Err(note)`
    /// once the turn is over budget — the note replaces the result. Disabled
    /// (`cap == 0`) always admits.
    pub(super) fn admit(&self, out: String) -> Result<String, String> {
        if self.cap == 0 {
            return Ok(out);
        }
        let already = self.consumed.load(Ordering::Relaxed);
        if already >= self.cap {
            return Err(format!(
                "[tool result omitted: this turn has already returned ~{} KB of tool output, \
                 over the {} KB per-turn budget. Stop calling tools and answer the user with \
                 what you already have; start a new turn if you genuinely need more.]",
                already / 1024,
                self.cap / 1024
            ));
        }
        self.consumed.fetch_add(out.len(), Ordering::Relaxed);
        Ok(out)
    }
}

tokio::task_local! {
    pub(super) static SESSION: SessionContext;
    static JOB_GRANTS: Arc<Vec<Rule>>;
}

/// Run `future` with `grants` as the ambient job-grant list — the actions a
/// human approved for one scheduled job when they created it.
///
/// Installed by `CronJobSweep` around the job's turn and read by
/// `PolicyApprover`, the same shape as [`with_session`] and for the same reason:
/// the domain `Approver` trait takes only the request, and threading a grant
/// list through it would touch every approver for the sake of one caller.
///
/// Outside this scope there are no grants at all — that is what keeps one job's
/// approvals from reaching another job, the briefing, or a conversation.
pub async fn with_job_grants<F: std::future::Future>(grants: Vec<Rule>, future: F) -> F::Output {
    JOB_GRANTS.scope(Arc::new(grants), future).await
}

/// The grants of the job whose turn is running; empty outside a job's turn.
pub fn current_job_grants() -> Arc<Vec<Rule>> {
    JOB_GRANTS
        .try_with(|g| g.clone())
        .unwrap_or_else(|_| Arc::new(Vec::new()))
}

/// Run `future` with `ctx` as the ambient session context. Called by the turn
/// entry points (the gateway dispatcher, the api channel, `handle_input`); the
/// executor re-installs the context around each spawned tool task.
pub async fn with_session<F: std::future::Future>(ctx: SessionContext, future: F) -> F::Output {
    SESSION.scope(ctx, future).await
}

/// The ambient session context, if the current task is running inside one.
/// `None` for aux sub-agents and the sweeps that never run an agent turn.
///
/// Note that the sweeps which *do* run one (cron, briefing) install a context
/// here like any other caller — "unattended" is [`SessionContext::origin`], not
/// the absence of a session. A consumer that cares whether a human is behind the
/// turn must read the origin; `is_none()` does not answer that question.
pub fn current_session() -> Option<SessionContext> {
    SESSION.try_with(|c| c.clone()).ok()
}
