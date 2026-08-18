//! `komo memory` — operator-facing governance over the memory library.
//!
//! Unlike the in-chat `memory` tool (scoped to the current chat), the CLI is a
//! host-side operator view: it lists and searches across *all* scopes, so you
//! can triage candidates the reviewer captured and promote/pin the durable ones.
//! Every read and write goes through [`OperatorControl`] — whether it reaches a
//! running gateway or the store directly is not this module's business.

use crate::domain::memory::{Memory, MemoryConfidence, MemoryStatus};
use crate::services::operator_control::{
    MemoryTransitionAction, OperatorCommand, OperatorCommandResult, OperatorControl, OperatorQuery,
    OperatorQueryResult,
};

/// Load every memory through the operator surface. The CLI's list/search/report
/// all filter this set client-side, so one loader serves them all (plus
/// `journey`).
pub(crate) async fn load_all(control: &OperatorControl) -> anyhow::Result<Vec<Memory>> {
    match control.query(OperatorQuery::Memories).await? {
        OperatorQueryResult::Memories(memories) => Ok(memories),
        _ => unreachable!("Memories query answers with Memories"),
    }
}

/// List stored memories, optionally filtered by status.
pub async fn list(control: &OperatorControl, status: Option<String>) -> anyhow::Result<()> {
    let filter = status
        .as_deref()
        .map(crate::domain::memory::parse_memory_status);
    let mut memories = load_all(control).await?;
    if let Some(status) = filter {
        memories.retain(|m| m.status == status);
    }
    if memories.is_empty() {
        println!("(no memories)");
        return Ok(());
    }
    // Group by status so candidates needing triage stand out.
    memories.sort_by(|a, b| {
        a.status
            .as_str()
            .cmp(b.status.as_str())
            .then(b.updated_at.cmp(&a.updated_at))
    });
    for m in &memories {
        println!("{}", line(m));
    }
    Ok(())
}

/// Substring search across all scopes (operator view — no scope enforcement).
/// Which turns a memory reached the prompt of, newest first.
///
/// The question worth asking right after correcting a memory: what did it
/// already shape? Prints run ids so `komo run inspect <id>` is the next step.
pub async fn used(control: &OperatorControl, id: &str, limit: usize) -> anyhow::Result<()> {
    let uses = match control
        .query(OperatorQuery::MemoryUsed {
            id: id.to_string(),
            limit,
        })
        .await?
    {
        OperatorQueryResult::MemoryUsed(uses) => uses,
        _ => unreachable!("MemoryUsed answers with MemoryUsed"),
    };
    if uses.is_empty() {
        // Two different nothings, and the operator needs them apart: the ledger
        // is pruned, so "never used" and "used before the ledger was trimmed"
        // look identical from here.
        println!(
            "没有记录到这条记忆进入过 prompt（run ledger 会被 `komo run prune` 清理，更早的使用查不到）。"
        );
        return Ok(());
    }
    println!("这条记忆进入过 {} 次 prompt：", uses.len());
    for use_ in &uses {
        println!(
            "  {}  {}  {}  {}",
            crate::cli::inspect::local_time(use_.started_at),
            if use_.pinned { "常驻" } else { "召回" },
            use_.run_id,
            use_.session_id
        );
    }
    println!("\n用 `komo run inspect <run-id>` 看具体那一轮。");
    Ok(())
}

pub async fn search(control: &OperatorControl, query: &str) -> anyhow::Result<()> {
    // The same hybrid query recall runs (lexical terms ∪ semantic vectors), not
    // a substring scan: an operator searching 智能设备 must find the memory
    // that says 智能插座, and a Chinese query must find an English memory.
    let hits = match control
        .query(OperatorQuery::MemorySearch {
            query: query.to_string(),
            limit: 20,
        })
        .await?
    {
        OperatorQueryResult::MemorySearch(hits) => hits,
        _ => unreachable!("MemorySearch answers with MemorySearch"),
    };
    if !control.via_gateway() {
        println!("(gateway not running — lexical match only, no semantic arm)");
    }
    if hits.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    for m in &hits {
        println!("{}", line(m));
    }
    Ok(())
}

/// Apply one governance transition to one id through the already-resolved
/// operator backend.
async fn transition(
    control: &OperatorControl,
    id: &str,
    action: MemoryTransitionAction,
) -> anyhow::Result<()> {
    control
        .command(OperatorCommand::MemoryTransition {
            id: id.to_string(),
            action,
        })
        .await?;
    Ok(())
}

/// Run a transition over a batch of ids, reporting per id and failing the
/// command (after trying every id) if any failed. The backend was resolved
/// once by the caller, so the batch never re-probes or reconnects per id.
async fn transition_batch(
    control: &OperatorControl,
    ids: &[String],
    action: MemoryTransitionAction,
    done: &str,
) -> anyhow::Result<()> {
    let mut failed = 0usize;
    for id in ids {
        match transition(control, id, action).await {
            Ok(()) => println!("{done} {id}."),
            Err(error) => {
                failed += 1;
                eprintln!("✗ {id}: {error}");
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} of {} failed", ids.len());
    }
    Ok(())
}

/// Promote candidates to active, confirmed memories.
pub async fn promote(control: &OperatorControl, ids: &[String]) -> anyhow::Result<()> {
    transition_batch(control, ids, MemoryTransitionAction::Promote, "Promoted").await
}

/// Reject candidates (won't surface in recall or injection).
pub async fn reject(control: &OperatorControl, ids: &[String]) -> anyhow::Result<()> {
    transition_batch(control, ids, MemoryTransitionAction::Reject, "Rejected").await
}

/// Interactively triage the candidate pile: one prompt per candidate,
/// **oldest first** — the oldest are closest to dreaming's 30-day archive
/// line, so they get the operator's eye before the sweep quietly retires
/// them. `p` promote / `r` reject / `s` skip / `q` quit.
pub async fn triage(control: &OperatorControl) -> anyhow::Result<()> {
    let mut candidates: Vec<Memory> = load_all(control)
        .await?
        .into_iter()
        .filter(|m| m.status == MemoryStatus::Candidate)
        .collect();
    if candidates.is_empty() {
        println!("(no candidates to triage)");
        return Ok(());
    }
    candidates.sort_by_key(|m| m.created_at);

    let total = candidates.len();
    let (mut promoted, mut rejected, mut skipped, mut failed) = (0usize, 0usize, 0usize, 0usize);
    println!("{total} candidate(s) to triage — p=promote  r=reject  s=skip  q=quit\n");
    'items: for (i, m) in candidates.iter().enumerate() {
        println!("[{}/{total}] {}", i + 1, line(m));
        let (action, bucket): (MemoryTransitionAction, &mut usize) = loop {
            match triage_choice(read_choice("  p/r/s/q> ").await?.as_deref()) {
                TriageChoice::Quit => break 'items,
                TriageChoice::Promote => break (MemoryTransitionAction::Promote, &mut promoted),
                TriageChoice::Reject => break (MemoryTransitionAction::Reject, &mut rejected),
                TriageChoice::Skip => {
                    skipped += 1;
                    continue 'items;
                }
                TriageChoice::Invalid => println!("  (p=promote  r=reject  s=skip  q=quit)"),
            }
        };
        match transition(control, &m.id, action).await {
            Ok(()) => *bucket += 1,
            Err(error) => {
                failed += 1;
                eprintln!("  ✗ {error}");
            }
        }
    }

    println!("\npromoted {promoted}, rejected {rejected}, skipped {skipped}");
    if failed > 0 {
        anyhow::bail!("{failed} transition(s) failed");
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum TriageChoice {
    Promote,
    Reject,
    Skip,
    Quit,
    Invalid,
}

/// The triage keymap, on an already trimmed+lowercased line (`None` = EOF).
/// EOF quits (piped stdin ran dry, Ctrl-D); a bare Enter skips — the idle
/// keystroke must never mutate.
fn triage_choice(input: Option<&str>) -> TriageChoice {
    match input {
        None | Some("q") => TriageChoice::Quit,
        Some("p") => TriageChoice::Promote,
        Some("r") => TriageChoice::Reject,
        Some("s") | Some("") => TriageChoice::Skip,
        Some(_) => TriageChoice::Invalid,
    }
}

/// One trimmed, lowercased line from stdin (`None` on EOF). The blocking read
/// runs off the async runtime, same as the CLI approver's prompt.
async fn read_choice(prompt: &str) -> anyhow::Result<Option<String>> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let line = tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .map(|n| (n > 0).then_some(buf))
    })
    .await??;
    Ok(line.map(|s| s.trim().to_lowercase()))
}

/// Pin a memory into the L1 per-turn profile (the manual, explicit path —
/// automated extraction never pins). Raises confidence so it actually surfaces.
pub async fn pin(control: &OperatorControl, id: &str) -> anyhow::Result<()> {
    transition(control, id, MemoryTransitionAction::Pin).await?;
    println!("Pinned {id} into the L1 profile.");
    Ok(())
}

/// Memory quality report (roadmap §9): bucket the whole library by status and
/// confidence, then surface the piles that need attention — candidates awaiting
/// triage, the pinned L1 set, low-confidence actives, long-unused actives, and
/// expired memories. Read-only; suggests `promote`/`reject`/`archive`/`pin`.
///
/// Recall counts (the dreaming usage signal) are shown per line; `komo dream`
/// previews which candidates that signal would promote or archive.
pub async fn report(control: &OperatorControl) -> anyhow::Result<()> {
    let memories = load_all(control).await?;
    if memories.is_empty() {
        println!("(no memories)");
        return Ok(());
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let total = memories.len();

    // Counts by status, in lifecycle order.
    println!("total: {total}");
    println!("\nby status:");
    for status in [
        MemoryStatus::Candidate,
        MemoryStatus::Active,
        MemoryStatus::Archived,
        MemoryStatus::Rejected,
    ] {
        let n = memories.iter().filter(|m| m.status == status).count();
        if n > 0 {
            println!("  {:<10} {n}", status.as_str());
        }
    }

    println!("\nby confidence:");
    for confidence in [
        MemoryConfidence::UserWritten,
        MemoryConfidence::Confirmed,
        MemoryConfidence::Inferred,
        MemoryConfidence::Extracted,
    ] {
        let n = memories
            .iter()
            .filter(|m| m.confidence == confidence)
            .count();
        if n > 0 {
            println!("  {:<12} {n}", confidence.as_str());
        }
    }

    // The piles that need an operator's eye.
    let active = |m: &&Memory| m.status == MemoryStatus::Active;
    let candidates: Vec<_> = memories
        .iter()
        .filter(|m| m.status == MemoryStatus::Candidate)
        .collect();
    let pinned: Vec<_> = memories.iter().filter(|m| m.pinned).collect();
    let low_conf: Vec<_> = memories
        .iter()
        .filter(active)
        .filter(|m| {
            matches!(
                m.confidence,
                MemoryConfidence::Extracted | MemoryConfidence::Inferred
            )
        })
        .collect();
    // Active but never surfaced, or not surfaced in 90+ days — archival candidates.
    const STALE_SECS: i64 = 90 * 24 * 60 * 60;
    let mut unused: Vec<_> = memories
        .iter()
        .filter(active)
        .filter(|m| m.last_used_at.is_none_or(|t| now - t > STALE_SECS))
        .collect();
    let expired: Vec<_> = memories.iter().filter(|m| m.is_expired(now)).collect();

    report_bucket("candidates awaiting triage (→ promote/reject)", &candidates);
    report_bucket("pinned into L1 profile", &pinned);
    report_bucket("low-confidence active (extracted/inferred)", &low_conf);
    unused.sort_by_key(|m| m.last_used_at.unwrap_or(0));
    report_bucket("active, long unused (90d+ → consider archive)", &unused);
    report_bucket("expired (past expires_at)", &expired);
    Ok(())
}

/// Print a named bucket: a header with the count, then up to 10 sample lines.
fn report_bucket(label: &str, items: &[&Memory]) {
    if items.is_empty() {
        return;
    }
    println!("\n{label}: {}", items.len());
    for m in items.iter().take(10) {
        println!("  {}", line(m));
    }
    if items.len() > 10 {
        println!("  … and {} more", items.len() - 10);
    }
}

fn line(m: &Memory) -> String {
    let pin = if m.pinned { " 📌" } else { "" };
    // Belief is shown only when it is not `current`: an operator scanning the
    // library needs to see a contested or superseded memory at a glance.
    let belief = if m.is_injectable() {
        String::new()
    } else {
        format!("/{}", m.belief.as_str())
    };
    let mut s = format!(
        "{}  [{}/{}/{}{}{}]  {}",
        m.id,
        m.status.as_str(),
        m.kind.as_str(),
        m.scope.type_str(),
        belief,
        pin,
        m.content
    );
    if m.support_count > 0 || m.contradiction_count > 0 {
        s.push_str(&format!(
            "  (support={} against={})",
            m.support_count, m.contradiction_count
        ));
    }
    if m.recall_count > 0 {
        s.push_str(&format!("  (recalls={})", m.recall_count));
    }
    if !m.source.is_empty() {
        s.push_str(&format!("  (from {})", m.source));
    }
    s
}

/// Widen memories stranded in a per-conversation `api` scope to global.
///
/// The `api` channel (TUI, desktop, web) mints a fresh chat id per
/// conversation, so a memory scoped to one is unreachable from every later
/// turn. This repairs the ones written before that was fixed; real chat
/// channels keep their scope, which is a privacy boundary rather than an
/// accident.
/// Embed every memory still missing a current vector.
pub async fn backfill(control: &OperatorControl) -> anyhow::Result<()> {
    match control.command(OperatorCommand::MemoryBackfill).await? {
        OperatorCommandResult::MemoryBackfilled { embedded: 0 } => {
            println!("every memory already has a current embedding");
        }
        OperatorCommandResult::MemoryBackfilled { embedded } => {
            println!("embedded {embedded} memories");
        }
        _ => unreachable!("MemoryBackfill answers with MemoryBackfilled"),
    }
    Ok(())
}

pub async fn repair_scopes(control: &OperatorControl) -> anyhow::Result<()> {
    match control.command(OperatorCommand::MemoryRepairScopes).await? {
        OperatorCommandResult::MemoryScopesRepaired { repaired: 0 } => {
            println!("no memories needed repair");
        }
        OperatorCommandResult::MemoryScopesRepaired { repaired } => {
            println!("widened {repaired} memories to global scope");
        }
        _ => unreachable!("MemoryRepairScopes answers with MemoryScopesRepaired"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triage_keymap_maps_choices_and_defaults_safely() {
        assert_eq!(triage_choice(Some("p")), TriageChoice::Promote);
        assert_eq!(triage_choice(Some("r")), TriageChoice::Reject);
        assert_eq!(triage_choice(Some("s")), TriageChoice::Skip);
        assert_eq!(triage_choice(Some("q")), TriageChoice::Quit);
        assert_eq!(triage_choice(None), TriageChoice::Quit, "EOF quits");
        assert_eq!(
            triage_choice(Some("")),
            TriageChoice::Skip,
            "bare Enter must never mutate"
        );
        assert_eq!(triage_choice(Some("x")), TriageChoice::Invalid);
    }
}
