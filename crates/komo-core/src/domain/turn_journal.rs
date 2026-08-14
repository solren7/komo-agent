//! Turn journal — the persisted twin of an in-flight turn's provider-level
//! state, so an interrupted turn can be resumed from where it stopped instead
//! of being re-run from a digest (which re-burns every tool round it had
//! already paid for).
//!
//! Scope is deliberately **one turn**, not the session: a turn runs on a single
//! provider and model with a stable prefix, which is exactly the boundary
//! inside which provider-shaped history is safe to persist. The cross-turn
//! transcript stays provider-agnostic (re-rendered every turn — that is what
//! lets a session switch models), and the run ledger stays an audit record.
//!
//! The invariant this buys — *anything the model saw can be rebuilt from the
//! journal* — holds by construction: the journal is written from the same
//! in-memory history the requests are built from, and there is no second code
//! path that assembles a request. A unit test locks rebuild == live; no runtime
//! reconciliation is needed.
//!
//! Payloads are opaque JSON at this layer. The shapes are provider types
//! (`komo-provider`), which this crate deliberately does not depend on; the one
//! writer and the one reader both live in `komo-agent`'s llm layer, so the
//! schema cannot fork.

use async_trait::async_trait;

/// Bumped when a payload's structure changes shape. A loader that meets an
/// unknown version must refuse to rebuild (the caller falls back to the
/// digest-priming resume), never guess.
pub const JOURNAL_VERSION: u32 = 1;

/// What one journal row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalKind {
    /// Written once per turn, after assembly: model settings and the full
    /// rendered history the turn opened with (seq 0 by convention).
    Envelope,
    /// One model round-trip's assistant message, verbatim (text + tool calls +
    /// opaque reasoning).
    Assistant,
    /// The tool outcomes fed back for the preceding assistant round, exactly as
    /// the model received them (full text, plus any mid-turn interjection).
    Results,
}

impl JournalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Envelope => "envelope",
            Self::Assistant => "assistant",
            Self::Results => "results",
        }
    }
}

pub fn parse_journal_kind(s: &str) -> anyhow::Result<JournalKind> {
    match s {
        "envelope" => Ok(JournalKind::Envelope),
        "assistant" => Ok(JournalKind::Assistant),
        "results" => Ok(JournalKind::Results),
        other => Err(anyhow::anyhow!(
            "unknown journal kind `{other}` (expected envelope/assistant/results)"
        )),
    }
}

/// One journal row. `seq` orders rows within a run; `payload` is JSON whose
/// shape is owned by the writer (see module docs).
#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub seq: i64,
    pub kind: JournalKind,
    pub payload: String,
}

/// Write side, handed into the LLM backend for the duration of one turn (the
/// backend is the only place that sees provider-shaped state). Implementations
/// bind the run id and assign `seq`; recording is best-effort by contract —
/// a journal failure must never fail the turn, it only costs resumability.
#[async_trait]
pub trait TurnJournal: Send + Sync {
    async fn record(&self, kind: JournalKind, payload: String);
}

/// Storage for journal rows, keyed by run id. Lives beside the run ledger
/// (state.db — disposable): losing it degrades resume to the digest path,
/// never worse.
#[async_trait]
pub trait TurnJournalRepository: Send + Sync {
    async fn append(&self, run_id: &str, entry: &JournalEntry) -> anyhow::Result<()>;
    /// All rows for a run, ordered by `seq`.
    async fn load(&self, run_id: &str) -> anyhow::Result<Vec<JournalEntry>>;
    /// Drop a run's rows (turn finished, or pruned with its run). Returns how
    /// many rows were removed.
    async fn delete(&self, run_id: &str) -> anyhow::Result<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_through_its_string_form() {
        for kind in [
            JournalKind::Envelope,
            JournalKind::Assistant,
            JournalKind::Results,
        ] {
            assert_eq!(parse_journal_kind(kind.as_str()).unwrap(), kind);
        }
        assert!(parse_journal_kind("bogus").is_err());
    }
}
