//! Scheduled cron jobs: deterministic commands the gateway executes unattended
//! on a cron schedule (hermes' `no_agent` cron jobs analog).
//!
//! Jobs live in their own durable store (`~/.komo/cron.db`) — not in
//! `config.toml`, because an operator can accumulate many of them, and not in
//! the disposable `state.db`, because a job silently vanishing on a state reset
//! means its work silently stops happening. A command job is **operator-authored**
//! (added via `komo cron add` or the loopback-gated api) — the same trust
//! boundary as running `komo gateway` itself — so execution is direct: no shell
//! tool, no approver, no `[policy]` involvement at fire time.
//!
//! A job created in conversation through the agent's `cron` tool is
//! *model*-authored, so that path moves the human decision to creation time: the
//! tool gates every mutation through the `Approver` (a command job prominently,
//! since approving it approves every future execution). By the time a job is in
//! the store the sweep treats both origins identically.

use async_trait::async_trait;
use croner::Cron;

use crate::domain::policy::{Rule, RuleSpec};

/// Default wall-clock budget for a job command — hermes' cron-job budget
/// (15 min), generous enough for a script that clones a repo and pushes an MR.
pub const DEFAULT_CRON_JOB_TIMEOUT_SECS: u64 = 900;

/// Outcome of a job's most recent execution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronRunStatus {
    Ok,
    Failed,
}

impl CronRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// `""` (never ran) → `None`; anything not `ok` parses as failed.
pub fn parse_cron_run_status(s: &str) -> Option<CronRunStatus> {
    match s {
        "" => None,
        "ok" => Some(CronRunStatus::Ok),
        _ => Some(CronRunStatus::Failed),
    }
}

/// What a job does when it fires. Internally tagged (`kind`) so the HTTP path
/// and the db both round-trip it without a separate discriminator column having
/// to be threaded by hand.
///
/// - `Command` — run a fixed program and deliver its stdout verbatim (hermes'
///   `no_agent` mode). Deterministic, no LLM. The reliable default for scripts.
/// - `Agent` — run a prompt through an **unattended, tool-capable agent turn**
///   and deliver the reply. Optional `skills` are loaded first. The agent runs
///   with the full tool set but side effects are gated by the permission
///   policy: with no human to prompt, a `Risk::Normal` action passes only
///   through an `unattended = true` `[policy]` rule (identical model to the
///   daily briefing).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronAction {
    Command {
        /// Program to execute (an absolute path; run directly, not via a shell).
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Working directory. `None` = the gateway's cwd.
        #[serde(default)]
        workdir: Option<String>,
        /// Wall-clock budget in seconds; the process is killed past it.
        timeout_secs: u64,
    },
    Agent {
        /// The instruction the agent turn runs.
        prompt: String,
        /// Skills to load before running the prompt (progressive disclosure —
        /// the turn is told to `skill` view each one first).
        #[serde(default)]
        skills: Vec<String>,
    },
}

impl CronAction {
    /// Short label for listings/logs.
    pub fn kind(&self) -> &'static str {
        match self {
            CronAction::Command { .. } => "command",
            CronAction::Agent { .. } => "agent",
        }
    }
}

/// One scheduled job. `name` is the operator-facing key (unique); `id` is the
/// storage key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    /// 5-field cron expression (local timezone).
    pub schedule: String,
    /// What the job does when it fires (command vs agent turn).
    pub action: CronAction,
    /// Disabled jobs stay listed/inspectable but never fire.
    pub enabled: bool,
    /// Next scheduled fire (unix seconds). The sweep runs a job once its
    /// `next_run_at` is due, then advances it — set to "now" to trigger an
    /// off-schedule run on the next sweep tick.
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    pub last_status: Option<CronRunStatus>,
    /// Failure detail from the most recent run (empty on success / never ran).
    pub last_error: String,
    pub created_at: i64,
    /// Actions this job may take unattended, approved by a human when the job
    /// was created. Empty = no side-effecting action is granted, which is what
    /// every job created before grants existed carries.
    ///
    /// Scoped to *this job's* turns, so deleting the job revokes them — unlike a
    /// global `unattended = true` `[[policy.rule]]`, which outlives whatever it
    /// was written for. Only ever an allow list: a denial belongs in config,
    /// where it applies to everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<RuleSpec>,
}

impl CronJob {
    /// A new enabled job with the given action. The caller (the shared operator
    /// action) validates the schedule and computes the initial `next_run_at` —
    /// this stays parse-free so komo-core needs no cron dependency.
    pub fn new(name: &str, schedule: &str, action: CronAction, next_run_at: i64) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            name: name.to_string(),
            schedule: schedule.to_string(),
            action,
            enabled: true,
            next_run_at,
            last_run_at: None,
            last_status: None,
            last_error: String::new(),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            grants: Vec::new(),
        }
    }

    /// Attach the grants a human approved when this job was created.
    pub fn with_grants(mut self, grants: Vec<RuleSpec>) -> Self {
        self.grants = grants;
        self
    }

    /// Convenience constructor for a command-mode job with default timeout.
    pub fn new_command(name: &str, schedule: &str, command: &str, next_run_at: i64) -> Self {
        Self::new(
            name,
            schedule,
            CronAction::Command {
                command: command.to_string(),
                args: Vec::new(),
                workdir: None,
                timeout_secs: DEFAULT_CRON_JOB_TIMEOUT_SECS,
            },
            next_run_at,
        )
    }

    /// Due = enabled and the scheduled fire time has arrived.
    pub fn is_due(&self, now: i64) -> bool {
        self.enabled && self.next_run_at <= now
    }

    /// This job's grants as policy rules.
    ///
    /// An entry that no longer parses is **dropped with a warning** rather than
    /// failing the run: grants are validated where a job is created, so the only
    /// way to get one here is a hand-edited db or a downgrade, and in both cases
    /// the safe reading of "I don't understand this permission" is to withhold
    /// it — never to fail the job in a way that looks like the schedule broke.
    pub fn granted_rules(&self) -> Vec<Rule> {
        self.grants
            .iter()
            .filter_map(|spec| match spec.to_rule() {
                Some(rule) => Some(rule),
                None => {
                    tracing::warn!(
                        job = %self.name,
                        category = %spec.category,
                        value = %spec.value,
                        "unparseable job grant ignored"
                    );
                    None
                }
            })
            .collect()
    }
}

/// Longest job name accepted. Names appear in notification titles, `komo cron`
/// listings and the per-run session id, so an essay is never one.
pub const MAX_CRON_JOB_NAME_LEN: usize = 64;

/// Shape floor for a job name. A name is a key: it identifies the job in every
/// `komo cron` subcommand and becomes part of an agent job's session id
/// (`cron:<name>:<unix>`), so whitespace and the separators that structure those
/// strings are refused. Everything else — including CJK — is allowed, because a
/// name is for the operator to read. Enforced in the shared create action, so
/// the CLI, the api channel and the agent's `cron` tool agree.
pub fn valid_cron_job_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_CRON_JOB_NAME_LEN
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ':' | '/' | '\\'))
}

/// The operator's request to create a job (`komo cron add` / `POST
/// /api/cron/add`). Validation and `next_run_at` computation happen in the
/// shared operator action, not here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJobSpec {
    pub name: String,
    pub schedule: String,
    pub action: CronAction,
}

#[async_trait]
pub trait CronJobRepository: Send + Sync {
    async fn save(&self, job: &CronJob) -> anyhow::Result<()>;
    /// Every job, enabled or not, ordered by name.
    async fn list(&self) -> anyhow::Result<Vec<CronJob>>;
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<CronJob>>;
    /// Update every mutable field of an existing job (matched by `id`).
    async fn update(&self, job: &CronJob) -> anyhow::Result<()>;
    /// Remove a job by name; `false` = no such job.
    async fn delete(&self, name: &str) -> anyhow::Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_command_job_is_enabled_with_default_timeout() {
        let job = CronJob::new_command("weekly", "0 14 * * 5", "/opt/rotate.py", 1000);
        assert!(job.enabled);
        assert_eq!(job.action.kind(), "command");
        let CronAction::Command { timeout_secs, .. } = &job.action else {
            panic!("command job");
        };
        assert_eq!(*timeout_secs, DEFAULT_CRON_JOB_TIMEOUT_SECS);
        assert_eq!(job.next_run_at, 1000);
        assert!(job.last_status.is_none());
        assert!(!job.id.is_empty());
    }

    #[test]
    fn agent_action_roundtrips_through_json() {
        let action = CronAction::Agent {
            prompt: "summarize my day".into(),
            skills: vec!["calendar".into()],
        };
        let job = CronJob::new("brief", "0 8 * * *", action, 0);
        let json = serde_json::to_string(&job).unwrap();
        assert!(json.contains("\"kind\":\"agent\""));
        let back: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.action.kind(), "agent");
        let CronAction::Agent { prompt, skills } = &back.action else {
            panic!("agent job");
        };
        assert_eq!(prompt, "summarize my day");
        assert_eq!(skills, &vec!["calendar".to_string()]);
    }

    #[test]
    fn due_requires_enabled_and_elapsed() {
        let mut job = CronJob::new_command("j", "* * * * *", "/bin/true", 100);
        assert!(job.is_due(100));
        assert!(job.is_due(101));
        assert!(!job.is_due(99));
        job.enabled = false;
        assert!(!job.is_due(200), "a disabled job is never due");
    }

    #[test]
    fn job_names_must_stay_key_shaped() {
        assert!(valid_cron_job_name("morning-brief"));
        assert!(valid_cron_job_name("weekly_alarm.rotation"));
        // A name is for the operator to read, so CJK is fine.
        assert!(valid_cron_job_name("每日简报"));
        assert!(!valid_cron_job_name(""));
        assert!(
            !valid_cron_job_name("morning brief"),
            "whitespace splits it"
        );
        assert!(
            !valid_cron_job_name("cron:brief"),
            "`:` structures the session id"
        );
        assert!(!valid_cron_job_name("a/b"));
        assert!(!valid_cron_job_name("a\\b"));
        assert!(!valid_cron_job_name("a\nb"));
        assert!(!valid_cron_job_name(&"x".repeat(MAX_CRON_JOB_NAME_LEN + 1)));
        assert!(valid_cron_job_name(&"x".repeat(MAX_CRON_JOB_NAME_LEN)));
    }

    #[test]
    fn run_status_roundtrip() {
        assert_eq!(parse_cron_run_status(""), None);
        assert_eq!(parse_cron_run_status("ok"), Some(CronRunStatus::Ok));
        assert_eq!(parse_cron_run_status("failed"), Some(CronRunStatus::Failed));
        assert_eq!(
            parse_cron_run_status("garbage"),
            Some(CronRunStatus::Failed)
        );
    }

    #[test]
    fn ids_are_unique_across_rapid_creation() {
        let a = CronJob::new_command("a", "* * * * *", "/bin/true", 0);
        let b = CronJob::new_command("b", "* * * * *", "/bin/true", 0);
        assert_ne!(a.id, b.id);
    }
}

/// Compute the next occurrence of a cron expression strictly after `after`.
/// Timezone-generic so tests can use `FixedOffset` for determinism while
/// production uses `Local`.
pub fn next_occurrence_in<Tz>(
    expr: &str,
    after: chrono::DateTime<Tz>,
) -> anyhow::Result<chrono::DateTime<Tz>>
where
    Tz: chrono::TimeZone + Clone,
{
    let cron = expr
        .parse::<Cron>()
        .map_err(|e| anyhow::anyhow!("invalid cron expression `{expr}`: {e}"))?;
    Ok(cron.find_next_occurrence(&after, false)?)
}

/// Production wrapper: compute the next local-time occurrence after `after_unix`
/// and return it as a Unix timestamp. Computes from the given time (usually
/// `now`) so a resting daemon always jumps to the next future slot.
pub fn next_occurrence_local(expr: &str, after_unix: i64) -> anyhow::Result<i64> {
    let after_utc = chrono::DateTime::from_timestamp(after_unix, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid unix timestamp: {after_unix}"))?;
    let after_local = after_utc.with_timezone(&chrono::Local);
    let next = next_occurrence_in(expr, after_local)?;
    Ok(next.timestamp())
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Timelike};

    #[test]
    fn next_occurrence_in_rejects_invalid_expr() {
        let result = next_occurrence_in("not a cron", chrono::Utc::now());
        assert!(result.is_err());
    }

    #[test]
    fn next_occurrence_in_computes_strictly_future_fire() {
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let expr = "0 9 * * *"; // 9 AM daily

        // 8 AM local → next occurrence is 9 AM the same day
        let at_8am = tz.with_ymd_and_hms(2024, 1, 1, 8, 0, 0).unwrap();
        let next = next_occurrence_in(expr, at_8am).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.day(), 1);

        // exactly 9 AM local → next is 9 AM the following day (strictly future)
        let at_9am = tz.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        let next = next_occurrence_in(expr, at_9am).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.day(), 2);
    }
}
