//! Cron-job mutations shared by every caller that creates or changes a job.
//!
//! The `cron` tool (in conversation), the gateway's `/api/cron/*` handlers and
//! the direct CLI adapter all funnel through these functions, so validation —
//! schedule parsing, name uniqueness, the initial `next_run_at` — cannot fork
//! between the paths. `OperatorActions` wraps them for the operator-control
//! surface; it does not reimplement them.

use komo_core::domain::cron::{
    CronAction, CronJob, CronJobRepository, CronJobSpec, CronJobStatus,
    DEFAULT_CRON_JOB_TIMEOUT_SECS, MAX_CRON_JOB_NAME_LEN, next_occurrence_local,
    valid_cron_job_name,
};
use komo_core::domain::policy::{Matcher, RuleSpec};

/// Validate a job spec and create it — schedule parsed with the same cron
/// parser the sweep uses (so nothing invalid ever reaches the store), name
/// uniqueness enforced, and the initial `next_run_at` computed from now.
/// Shared by the gateway's `/api/cron/add` handler and the direct adapter, so
/// validation can't fork between the two paths.
pub async fn add_cron_job(
    jobs: &dyn CronJobRepository,
    spec: CronJobSpec,
    now: i64,
) -> anyhow::Result<CronJob> {
    let name = spec.name.trim();
    if name.is_empty() {
        anyhow::bail!("a cron job needs a name");
    }
    // A name is a key (every `komo cron` subcommand, and an agent job's session
    // id) — keep it key-shaped. Matters most on the agent's `cron` tool path,
    // where the name is model-authored.
    if !valid_cron_job_name(name) {
        anyhow::bail!(
            "invalid job name `{name}`: no whitespace or `:` `/` `\\`, at most \
             {MAX_CRON_JOB_NAME_LEN} characters"
        );
    }
    // Normalize + validate the action per kind.
    let action = match spec.action {
        CronAction::Command {
            command,
            args,
            workdir,
            timeout_secs,
        } => {
            if command.trim().is_empty() {
                anyhow::bail!("a command cron job needs a command");
            }
            CronAction::Command {
                command: command.trim().to_string(),
                args,
                workdir: workdir.filter(|w| !w.trim().is_empty()),
                timeout_secs: if timeout_secs > 0 {
                    timeout_secs
                } else {
                    DEFAULT_CRON_JOB_TIMEOUT_SECS
                },
            }
        }
        CronAction::Agent { prompt, skills } => {
            if prompt.trim().is_empty() {
                anyhow::bail!("an agent cron job needs a prompt");
            }
            CronAction::Agent {
                prompt: prompt.trim().to_string(),
                skills: skills
                    .into_iter()
                    .filter(|s| !s.trim().is_empty())
                    .collect(),
            }
        }
    };
    if jobs.find_by_name(name).await?.is_some() {
        anyhow::bail!("a cron job named `{name}` already exists");
    }
    // Also proves the expression parses — next_occurrence_local rejects
    // anything croner can't schedule.
    let next_run_at = next_occurrence_local(&spec.schedule, now)?;
    let job = CronJob::new(name, &spec.schedule, action, next_run_at)
        .with_grants(normalize_grants(spec.grants)?);
    jobs.save(&job).await?;
    Ok(job)
}

/// Validate and normalize the grants a job is created with.
///
/// The caller supplies only *what* to allow — category, matcher, value. The
/// rest of the rule's shape is fixed here rather than accepted:
///
/// - `effect = allow` — a job grant is a whitelist; a denial belongs in config,
///   where it applies to everything rather than to one job;
/// - `unattended = true` — granting a job an action it can only take with a
///   human present would grant nothing at all;
/// - `include_dangerous = false` — that stays a config-only opt-in, the same
///   floor a saved grant has;
/// - `channels = None` — a job's turn has no channel to scope to, so a channel
///   scope here could only ever make the grant silently dead.
///
/// A malformed entry is a **hard error**, never dropped: a silently-discarded
/// grant produces a job that is refused at 3am for a reason nobody can see, and
/// the operator already approved a list they believed was complete.
pub fn normalize_grants(grants: Vec<RuleSpec>) -> anyhow::Result<Vec<RuleSpec>> {
    grants
        .into_iter()
        .map(|mut spec| {
            spec.effect = "allow".to_string();
            spec.unattended = true;
            spec.include_dangerous = false;
            spec.channels = None;
            // An entry with neither `match` nor `value` is the whole-category
            // wildcard, spelled explicitly so `describe()` reads as one. A
            // matcher with an empty value stays invalid — the same rule config
            // parsing uses, so a typo can never widen into "everything".
            if spec.matcher.trim().is_empty() && spec.value.trim().is_empty() {
                spec.matcher = "any".to_string();
            }
            let matcher_is_any = Matcher::parse(&spec.matcher) == Some(Matcher::Any);
            if !matcher_is_any && spec.value.trim().is_empty() {
                anyhow::bail!(
                    "grant `{}` has an empty value: give a target, or use match = \"any\" \
                     to mean the whole category",
                    spec.category
                );
            }
            spec.value = spec.value.trim().to_string();
            if spec.to_rule().is_none() {
                anyhow::bail!(
                    "invalid grant `{} {} {}`: category must be one of \
                     shell/file/network/homeassistant/mcp/wiki, match one of \
                     prefix/suffix/exact/contains/any, access one of read/write",
                    spec.category,
                    spec.matcher,
                    spec.value
                );
            }
            Ok(spec)
        })
        .collect()
}

/// Pause or resume a job; `None` = no such job. Resuming recomputes
/// `next_run_at` from now — a stale past slot must not fire the moment the job
/// comes back (a broken-schedule job that the sweep paused keeps its stored
/// expression, so this also surfaces the parse error to the operator; a
/// one-shot whose moment has passed is refused the same way). A completed
/// one-shot is terminal: its row is the record of what ran, never a schedule.
pub async fn set_cron_enabled(
    jobs: &dyn CronJobRepository,
    name: &str,
    enabled: bool,
    now: i64,
) -> anyhow::Result<Option<CronJob>> {
    let Some(mut job) = jobs.find_by_name(name).await? else {
        return Ok(None);
    };
    if job.status == CronJobStatus::Done {
        anyhow::bail!(
            "cron job `{name}` already completed (one-shot) — create a new job to run it again"
        );
    }
    if enabled && job.status == CronJobStatus::Paused {
        job.next_run_at = next_occurrence_local(&job.schedule, now)?;
    }
    job.status = if enabled {
        CronJobStatus::Active
    } else {
        CronJobStatus::Paused
    };
    jobs.update(&job).await?;
    Ok(Some(job))
}

/// Make a job due immediately (the sweep picks it up on its next tick);
/// `None` = no such job. The job must be active — triggering a paused job
/// would silently do nothing until someone resumed it, and a completed
/// one-shot is terminal.
pub async fn trigger_cron_job(
    jobs: &dyn CronJobRepository,
    name: &str,
    now: i64,
) -> anyhow::Result<Option<CronJob>> {
    let Some(mut job) = jobs.find_by_name(name).await? else {
        return Ok(None);
    };
    match job.status {
        CronJobStatus::Done => anyhow::bail!(
            "cron job `{name}` already completed (one-shot) — create a new job to run it again"
        ),
        CronJobStatus::Paused => {
            anyhow::bail!("cron job `{name}` is paused — enable it first (`komo cron enable`)")
        }
        CronJobStatus::Active => {}
    }
    job.next_run_at = now;
    jobs.update(&job).await?;
    Ok(Some(job))
}

/// One uniform unknown-job message (the gateway's 404 body and the direct
/// path's error must read identically).
pub fn no_cron_job_message(name: &str) -> String {
    format!("no cron job named `{name}`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeJobs {
        jobs: Mutex<Vec<CronJob>>,
    }

    #[async_trait::async_trait]
    impl CronJobRepository for FakeJobs {
        async fn save(&self, job: &CronJob) -> anyhow::Result<()> {
            self.jobs.lock().unwrap().push(job.clone());
            Ok(())
        }
        async fn list(&self) -> anyhow::Result<Vec<CronJob>> {
            Ok(self.jobs.lock().unwrap().clone())
        }
        async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<CronJob>> {
            Ok(self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .find(|j| j.name == name)
                .cloned())
        }
        async fn update(&self, job: &CronJob) -> anyhow::Result<()> {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(slot) = jobs.iter_mut().find(|j| j.id == job.id) {
                *slot = job.clone();
            }
            Ok(())
        }
        async fn delete(&self, name: &str) -> anyhow::Result<bool> {
            let mut jobs = self.jobs.lock().unwrap();
            let before = jobs.len();
            jobs.retain(|j| j.name != name);
            Ok(jobs.len() != before)
        }
    }

    fn done_job(name: &str) -> CronJob {
        let mut job = CronJob::new_command(name, "@at 2020-01-01 08:00", "/bin/true", 0);
        job.status = CronJobStatus::Done;
        job.last_run_at = Some(100);
        job
    }

    /// A completed one-shot is terminal — its row is the record of what ran.
    /// Both resume and trigger refuse rather than resurrecting a past moment.
    #[tokio::test]
    async fn a_completed_one_shot_refuses_enable_and_trigger() {
        let jobs = FakeJobs::default();
        jobs.save(&done_job("once")).await.unwrap();

        let err = set_cron_enabled(&jobs, "once", true, 1000)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already completed"), "{err}");
        let err = trigger_cron_job(&jobs, "once", 1000).await.unwrap_err();
        assert!(err.to_string().contains("already completed"), "{err}");
        assert_eq!(
            jobs.find_by_name("once").await.unwrap().unwrap().status,
            CronJobStatus::Done,
            "refusal must not mutate the record"
        );
    }

    #[tokio::test]
    async fn add_accepts_a_future_one_shot_and_rejects_a_past_one() {
        let jobs = FakeJobs::default();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let job = add_cron_job(
            &jobs,
            CronJobSpec {
                name: "reboot-nas".into(),
                schedule: "@at 2030-01-02 08:30".into(),
                action: CronAction::Command {
                    command: "/bin/true".into(),
                    args: vec![],
                    workdir: None,
                    timeout_secs: 0,
                },
                grants: vec![],
            },
            now,
        )
        .await
        .unwrap();
        assert!(job.is_once());
        assert!(job.next_run_at > now);
        assert_eq!(job.status, CronJobStatus::Active);

        let err = add_cron_job(
            &jobs,
            CronJobSpec {
                name: "too-late".into(),
                schedule: "@at 2020-01-01 08:00".into(),
                action: CronAction::Command {
                    command: "/bin/true".into(),
                    args: vec![],
                    workdir: None,
                    timeout_secs: 0,
                },
                grants: vec![],
            },
            now,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already past"), "{err}");
    }

    fn spec(category: &str, matcher: &str, value: &str) -> RuleSpec {
        RuleSpec {
            category: category.to_string(),
            matcher: matcher.to_string(),
            value: value.to_string(),
            access: None,
            channels: None,
            effect: String::new(),
            include_dangerous: false,
            unattended: false,
        }
    }

    /// The caller says *what* to allow; the rule's shape is not up to them.
    #[test]
    fn normalize_fixes_the_rule_shape() {
        let mut asked = spec("homeassistant", "exact", "climate.set_temperature");
        asked.effect = "deny".into();
        asked.include_dangerous = true;
        asked.channels = Some(vec!["feishu".into()]);

        let out = normalize_grants(vec![asked]).unwrap();
        assert_eq!(out[0].effect, "allow");
        assert!(
            out[0].unattended,
            "a grant that needs a human grants nothing"
        );
        assert!(!out[0].include_dangerous, "dangerous stays config-only");
        assert_eq!(
            out[0].channels, None,
            "a job turn has no channel to scope to"
        );
    }

    /// Omitting both `match` and `value` is the explicit whole-category
    /// wildcard, so `describe()` reads as one rather than as an empty pattern.
    #[test]
    fn an_empty_entry_becomes_the_explicit_wildcard() {
        let out = normalize_grants(vec![spec("shell", "", "")]).unwrap();
        assert_eq!(out[0].matcher, "any");
    }

    /// …but a real matcher with an empty value stays an error: a typo must
    /// never widen a grant into "everything".
    #[test]
    fn a_matcher_with_an_empty_value_is_rejected() {
        assert!(normalize_grants(vec![spec("shell", "prefix", "  ")]).is_err());
    }

    /// A bad entry fails the whole call rather than being dropped — a silently
    /// discarded grant becomes a job refused at 3am for no visible reason.
    #[test]
    fn an_unparseable_grant_is_an_error_not_a_drop() {
        let err = normalize_grants(vec![
            spec("homeassistant", "exact", "climate.set_temperature"),
            spec("teleport", "exact", "somewhere"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("teleport"), "{err}");
    }
}
