//! Review orchestration (architecture deepening plan §8): one owner for *when*
//! a session gets the reflective reviewer, which snapshot it sees, and how the
//! watermark and concurrency behave.
//!
//! The same correctness protocol used to live twice — in the runtime's
//! post-turn cadence check and in the daemon's scheduled sweep. Both now call
//! [`ReviewCoordinator::run`] with their trigger; the coordinator owns
//! cadence, the cheap candidate projection, the full-transcript reload, the
//! per-session in-flight guard, and the best-effort watermark advance. The
//! `ReflectiveReviewer` stays what it is: transcript in, suggestions out.
//!
//! The in-flight guard is a process-local keyed set — the gateway is the sole
//! runner of reviews, and after a crash the un-advanced watermark simply makes
//! the next sweep pick the session up again, so no persisted claim is needed.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tracing::warn;

use komo_core::domain::{
    repository::{MessageRepository, SessionRepository},
    reviewer::{ReviewOutcome, Reviewer},
    session::Session,
};

/// Sessions the reviewer must never extract from: unattended sweep turns
/// (briefings and cron jobs). A sweep restates facts the agent already knows
/// (tasks, memories, device state), so extraction there re-observes the same
/// claims on every run — each run's session is a "new independent occasion" to
/// the consolidator, flooding the memory pipeline with duplicate candidates
/// instead of evidence. Both sweep runtimes already wire `review: None`; this
/// guard covers the scheduled sweep, which scans every persisted session.
fn exempt_from_review(session_id: &str) -> bool {
    session_id.starts_with("briefing:") || session_id.starts_with("cron:")
}

/// Why a review is being requested.
pub enum ReviewTrigger {
    /// A turn just finished in `session_id`: review it if the cadence
    /// (`review_interval` user turns) is due, else do nothing.
    AfterTurn { session_id: String },
    /// The maintenance sweep: review every session with user turns the
    /// reviewer hasn't seen yet.
    Scheduled,
}

/// What one coordinator run accomplished, aggregated across sessions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReviewReport {
    pub sessions_reviewed: usize,
    pub memories_written: usize,
    pub skills_written: usize,
    pub tasks_captured: usize,
}

impl ReviewReport {
    pub fn is_empty(&self) -> bool {
        self.sessions_reviewed == 0
            && self.memories_written == 0
            && self.skills_written == 0
            && self.tasks_captured == 0
    }

    fn absorb(&mut self, outcome: &ReviewOutcome) {
        self.sessions_reviewed += 1;
        self.memories_written += outcome.memories_written.len();
        self.skills_written += outcome.skills_written.len();
        self.tasks_captured += outcome.tasks_captured.len();
    }
}

/// The one review orchestrator. Both trigger paths must share a single
/// instance (wiring creates it once) — that is what makes the in-flight guard
/// effective when a post-turn review and a sweep hit the same session.
pub struct ReviewCoordinator {
    sessions: Arc<dyn SessionRepository>,
    messages: Arc<dyn MessageRepository>,
    reviewer: Arc<dyn Reviewer>,
    /// Review every N user turns on the after-turn trigger.
    review_interval: usize,
    /// Session ids currently being reviewed (either trigger).
    in_flight: Mutex<HashSet<String>>,
}

impl ReviewCoordinator {
    pub fn new(
        sessions: Arc<dyn SessionRepository>,
        messages: Arc<dyn MessageRepository>,
        reviewer: Arc<dyn Reviewer>,
        review_interval: usize,
    ) -> Self {
        Self {
            sessions,
            messages,
            reviewer,
            review_interval: review_interval.max(1),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Run one review pass for `trigger`. Callers pass no turn counts or
    /// watermarks — eligibility is this module's knowledge.
    pub async fn run(&self, trigger: ReviewTrigger) -> anyhow::Result<ReviewReport> {
        let mut report = ReviewReport::default();
        match trigger {
            ReviewTrigger::AfterTurn { session_id } => {
                if exempt_from_review(&session_id) {
                    return Ok(report);
                }
                // Cadence is driven by the true user-turn total (a cheap
                // COUNT) — a windowed in-memory count would plateau at the
                // window size and mis-fire the modulo.
                let turns = self.messages.count_user_turns(&session_id).await?;
                if turns == 0 || turns % self.review_interval != 0 {
                    return Ok(report);
                }
                // The reflective reviewer needs the whole transcript, so load
                // the full session, not the turn's working window.
                let Some(session) = self.sessions.find(&session_id).await? else {
                    return Ok(report);
                };
                self.review_one(&session, turns, &mut report).await?;
            }
            ReviewTrigger::Scheduled => {
                // Scan the cheap projection (id + counts, no transcripts) and
                // review only sessions with user turns the reviewer hasn't
                // seen yet, instead of loading and re-reviewing everything.
                let candidates = self.sessions.review_candidates().await?;
                for candidate in candidates {
                    if exempt_from_review(&candidate.id)
                        || candidate.user_turns == 0
                        || candidate.user_turns <= candidate.reviewed_through
                    {
                        continue;
                    }
                    // Materialize the full transcript only when needed.
                    let Some(session) = self.sessions.find(&candidate.id).await? else {
                        continue;
                    };
                    // Isolate per-session failures: a single bad review must
                    // not abort the whole sweep.
                    if let Err(error) = self
                        .review_one(&session, candidate.user_turns, &mut report)
                        .await
                    {
                        warn!(%error, session = %candidate.id, "session review failed (skipped)");
                    }
                }
            }
        }
        Ok(report)
    }

    /// Review one session snapshot and, on success, advance the shared
    /// watermark to `through` (best-effort: a failed mark just means the
    /// session is reviewed again next cycle — the reviewer's dedup guards make
    /// that harmless, never wrong).
    async fn review_one(
        &self,
        session: &Session,
        through: usize,
        report: &mut ReviewReport,
    ) -> anyhow::Result<()> {
        // At most one review per session at a time, across both triggers. If
        // one is already running, skip: its success advances the watermark,
        // and its failure leaves the session for the next sweep — either way
        // a second concurrent review only duplicates LLM cost.
        let Some(_guard) = InFlightGuard::claim(&self.in_flight, &session.id) else {
            return Ok(());
        };
        let outcome = self.reviewer.review(session).await?;
        report.absorb(&outcome);
        if let Err(error) = self.sessions.mark_reviewed(&session.id, through).await {
            warn!(%error, session = %session.id, "failed to advance review watermark");
        }
        Ok(())
    }
}

/// RAII claim on a session id in the in-flight set: released on drop, so a
/// panicking or failing review never wedges the session.
struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl<'a> InFlightGuard<'a> {
    fn claim(set: &'a Mutex<HashSet<String>>, id: &str) -> Option<Self> {
        set.lock()
            .unwrap()
            .insert(id.to_string())
            .then(|| InFlightGuard {
                set,
                id: id.to_string(),
            })
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use komo_core::domain::message::Message;
    use komo_core::domain::repository::ReviewCandidate;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Sessions with hand-set candidates/turn counts; records watermark marks
    /// and full-transcript loads.
    struct FakeSessions {
        candidates: Vec<ReviewCandidate>,
        user_turns: usize,
        marked: Mutex<Vec<(String, usize)>>,
        finds: AtomicUsize,
        fail_mark: bool,
    }

    impl FakeSessions {
        fn new(candidates: Vec<ReviewCandidate>, user_turns: usize) -> Self {
            Self {
                candidates,
                user_turns,
                marked: Mutex::new(Vec::new()),
                finds: AtomicUsize::new(0),
                fail_mark: false,
            }
        }
    }

    #[async_trait]
    impl SessionRepository for FakeSessions {
        async fn find(&self, id: &str) -> anyhow::Result<Option<Session>> {
            self.finds.fetch_add(1, Ordering::Relaxed);
            let mut s = Session::new(id);
            s.messages.push(Message::user("hi"));
            Ok(Some(s))
        }
        async fn find_windowed(&self, id: &str, _limit: usize) -> anyhow::Result<Option<Session>> {
            self.find(id).await
        }
        async fn list(&self) -> anyhow::Result<Vec<Session>> {
            unimplemented!("the coordinator uses review_candidates, not list")
        }
        async fn save(&self, _session: &Session) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete_empty_sessions(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn rotate(&self, _session_id: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn review_candidates(&self) -> anyhow::Result<Vec<ReviewCandidate>> {
            Ok(self
                .candidates
                .iter()
                .map(|c| ReviewCandidate {
                    id: c.id.clone(),
                    user_turns: c.user_turns,
                    reviewed_through: c.reviewed_through,
                })
                .collect())
        }
        async fn mark_reviewed(&self, session_id: &str, through: usize) -> anyhow::Result<()> {
            if self.fail_mark {
                anyhow::bail!("watermark store offline");
            }
            self.marked
                .lock()
                .unwrap()
                .push((session_id.to_string(), through));
            Ok(())
        }
    }

    #[async_trait]
    impl MessageRepository for FakeSessions {
        async fn list_by_session(&self, _session_id: &str) -> anyhow::Result<Vec<Message>> {
            Ok(Vec::new())
        }
        async fn save(&self, _session_id: &str, _message: &Message) -> anyhow::Result<()> {
            Ok(())
        }
        async fn count_user_turns(&self, _session_id: &str) -> anyhow::Result<usize> {
            Ok(self.user_turns)
        }
        async fn cancel_last_turn(&self, _session_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn record_interjection(&self, _session_id: &str, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Records reviewed session ids; can fail specific ids, block until
    /// released, and returns a fixed outcome.
    struct RecordingReviewer {
        reviewed: Mutex<Vec<String>>,
        fail_ids: Vec<String>,
        gate: Option<Arc<tokio::sync::Notify>>,
        outcome: ReviewOutcome,
    }

    impl Default for RecordingReviewer {
        fn default() -> Self {
            Self {
                reviewed: Mutex::new(Vec::new()),
                fail_ids: Vec::new(),
                gate: None,
                outcome: ReviewOutcome::default(),
            }
        }
    }

    #[async_trait]
    impl Reviewer for RecordingReviewer {
        async fn review(&self, session: &Session) -> anyhow::Result<ReviewOutcome> {
            self.reviewed.lock().unwrap().push(session.id.clone());
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            if self.fail_ids.contains(&session.id) {
                anyhow::bail!("review model unavailable");
            }
            Ok(self.outcome.clone())
        }
    }

    fn candidate(id: &str, user_turns: usize, reviewed_through: usize) -> ReviewCandidate {
        ReviewCandidate {
            id: id.into(),
            user_turns,
            reviewed_through,
        }
    }

    fn coordinator(
        sessions: Arc<FakeSessions>,
        reviewer: Arc<RecordingReviewer>,
        interval: usize,
    ) -> ReviewCoordinator {
        ReviewCoordinator::new(sessions.clone(), sessions, reviewer, interval)
    }

    #[tokio::test]
    async fn after_turn_off_interval_loads_nothing_and_reviews_nothing() {
        let sessions = Arc::new(FakeSessions::new(Vec::new(), 7));
        let reviewer = Arc::new(RecordingReviewer::default());
        let c = coordinator(sessions.clone(), reviewer.clone(), 10);

        let report = c
            .run(ReviewTrigger::AfterTurn {
                session_id: "cli:s".into(),
            })
            .await
            .unwrap();

        assert!(report.is_empty());
        assert!(reviewer.reviewed.lock().unwrap().is_empty());
        assert_eq!(
            sessions.finds.load(Ordering::Relaxed),
            0,
            "off-cadence must not load the full transcript"
        );
    }

    #[tokio::test]
    async fn after_turn_on_interval_reviews_and_advances_watermark() {
        let sessions = Arc::new(FakeSessions::new(Vec::new(), 10));
        let reviewer = Arc::new(RecordingReviewer::default());
        let c = coordinator(sessions.clone(), reviewer.clone(), 10);

        let report = c
            .run(ReviewTrigger::AfterTurn {
                session_id: "cli:s".into(),
            })
            .await
            .unwrap();

        assert_eq!(report.sessions_reviewed, 1);
        assert_eq!(
            *reviewer.reviewed.lock().unwrap(),
            vec!["cli:s".to_string()]
        );
        assert_eq!(
            *sessions.marked.lock().unwrap(),
            vec![("cli:s".to_string(), 10)]
        );
    }

    #[tokio::test]
    async fn scheduled_skips_sessions_without_new_turns() {
        let sessions = Arc::new(FakeSessions::new(
            vec![
                candidate("empty", 0, 0),     // no user turns → skipped
                candidate("caught-up", 3, 3), // already reviewed → skipped
                candidate("fresh", 5, 2),     // new turns → reviewed
            ],
            0,
        ));
        let reviewer = Arc::new(RecordingReviewer::default());
        let c = coordinator(sessions.clone(), reviewer.clone(), 10);

        let report = c.run(ReviewTrigger::Scheduled).await.unwrap();

        assert_eq!(report.sessions_reviewed, 1);
        assert_eq!(
            *reviewer.reviewed.lock().unwrap(),
            vec!["fresh".to_string()]
        );
        assert_eq!(
            *sessions.marked.lock().unwrap(),
            vec![("fresh".to_string(), 5)],
            "only the reviewed session's watermark advances, to its live count"
        );
    }

    #[tokio::test]
    async fn sweep_sessions_are_never_reviewed() {
        // Scheduled sweep: briefing and cron sessions are skipped even with
        // unseen turns.
        let sessions = Arc::new(FakeSessions::new(
            vec![
                candidate("briefing:2026-08-20", 3, 0),
                candidate("cron:alarm-rotate:1755600000", 4, 0),
                candidate("feishu:oc_1", 5, 0),
            ],
            10,
        ));
        let reviewer = Arc::new(RecordingReviewer::default());
        let c = coordinator(sessions.clone(), reviewer.clone(), 10);

        let report = c.run(ReviewTrigger::Scheduled).await.unwrap();
        assert_eq!(report.sessions_reviewed, 1);
        assert_eq!(
            *reviewer.reviewed.lock().unwrap(),
            vec!["feishu:oc_1".to_string()]
        );

        // After-turn: skipped before the cadence count even runs.
        for id in ["briefing:2026-08-20", "cron:alarm-rotate:1755600000"] {
            let report = c
                .run(ReviewTrigger::AfterTurn {
                    session_id: id.into(),
                })
                .await
                .unwrap();
            assert!(report.is_empty());
        }
        assert_eq!(reviewer.reviewed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scheduled_isolates_a_failing_session_and_continues() {
        let sessions = Arc::new(FakeSessions::new(
            vec![candidate("bad", 4, 0), candidate("good", 6, 0)],
            0,
        ));
        let reviewer = Arc::new(RecordingReviewer {
            fail_ids: vec!["bad".into()],
            ..Default::default()
        });
        let c = coordinator(sessions.clone(), reviewer.clone(), 10);

        let report = c.run(ReviewTrigger::Scheduled).await.unwrap();

        assert_eq!(report.sessions_reviewed, 1, "the good session still ran");
        assert_eq!(
            *sessions.marked.lock().unwrap(),
            vec![("good".to_string(), 6)],
            "a failed review must not advance its watermark"
        );
    }

    #[tokio::test]
    async fn watermark_failure_still_returns_the_review_result() {
        let mut fake = FakeSessions::new(vec![candidate("s", 5, 0)], 0);
        fake.fail_mark = true;
        let sessions = Arc::new(fake);
        let reviewer = Arc::new(RecordingReviewer {
            outcome: ReviewOutcome {
                memories_written: vec!["m1".into()],
                skills_written: Vec::new(),
                tasks_captured: vec!["t1".into()],
            },
            ..Default::default()
        });
        let c = coordinator(sessions, reviewer, 10);

        let report = c.run(ReviewTrigger::Scheduled).await.unwrap();
        assert_eq!(report.sessions_reviewed, 1);
        assert_eq!(report.memories_written, 1);
        assert_eq!(report.tasks_captured, 1);
        // The un-advanced watermark just means a future re-review — allowed.
    }

    #[tokio::test]
    async fn concurrent_triggers_review_a_session_once() {
        let sessions = Arc::new(FakeSessions::new(vec![candidate("s", 10, 0)], 10));
        let gate = Arc::new(tokio::sync::Notify::new());
        let reviewer = Arc::new(RecordingReviewer {
            gate: Some(gate.clone()),
            ..Default::default()
        });
        let c = Arc::new(coordinator(sessions.clone(), reviewer.clone(), 10));

        // First trigger claims the session and blocks inside the reviewer.
        let first = tokio::spawn({
            let c = c.clone();
            async move {
                c.run(ReviewTrigger::AfterTurn {
                    session_id: "s".into(),
                })
                .await
                .unwrap()
            }
        });
        // Wait until the reviewer actually started.
        for _ in 0..100 {
            if !reviewer.reviewed.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        // The scheduled sweep hits the same session while it's in flight.
        let during = c.run(ReviewTrigger::Scheduled).await.unwrap();
        assert!(
            during.is_empty(),
            "in-flight session is skipped, not re-reviewed"
        );

        gate.notify_one();
        let report = first.await.unwrap();
        assert_eq!(report.sessions_reviewed, 1);
        assert_eq!(
            reviewer.reviewed.lock().unwrap().len(),
            1,
            "exactly one review despite two triggers"
        );

        // The guard is released after completion: a later trigger reviews again.
        let gate2 = gate.clone();
        let again = tokio::spawn({
            let c = c.clone();
            async move { c.run(ReviewTrigger::Scheduled).await.unwrap() }
        });
        for _ in 0..100 {
            if reviewer.reviewed.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        gate2.notify_one();
        assert_eq!(again.await.unwrap().sessions_reviewed, 1);
    }

    #[tokio::test]
    async fn failed_review_releases_the_guard() {
        let sessions = Arc::new(FakeSessions::new(vec![candidate("s", 4, 0)], 0));
        let reviewer = Arc::new(RecordingReviewer {
            fail_ids: vec!["s".into()],
            ..Default::default()
        });
        let c = coordinator(sessions, reviewer.clone(), 10);

        let _ = c.run(ReviewTrigger::Scheduled).await.unwrap();
        let _ = c.run(ReviewTrigger::Scheduled).await.unwrap();
        assert_eq!(
            reviewer.reviewed.lock().unwrap().len(),
            2,
            "a failed review must not wedge the session"
        );
    }
}
