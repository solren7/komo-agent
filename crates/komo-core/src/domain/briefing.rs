//! Briefing sweep state: which local day's briefing slot has been handled.

use async_trait::async_trait;

/// Watermark for the daily briefing: the local date (`YYYY-MM-DD`) whose slot
/// was last handled. This is what lets a gateway that was down across the slot
/// catch up once at startup — the same "asleep over a slot → run late, once"
/// behavior a cron job gets from its stored `next_run_at` — instead of
/// silently skipping the day. Lives in the disposable state store: losing it
/// costs at most one duplicate catch-up check, never a job's work.
#[async_trait]
pub trait BriefingMarkRepository: Send + Sync {
    /// The local date last handled, or `None` (never ran / state was reset).
    async fn last_handled(&self) -> anyhow::Result<Option<String>>;
    /// Record `date` (local `YYYY-MM-DD`) as handled, overwriting.
    async fn mark_handled(&self, date: &str) -> anyhow::Result<()>;
}
