//! `komo doctor` — config & gateway health aggregation (roadmap §9).
//!
//! A single read-only snapshot of "what is komo configured to do, and what is
//! missing": the active model/provider and whether its API key is present, the
//! sweep schedules, each ingress channel's enabled+credentials state, the
//! resolved home channel, and the run ledger's recent failures.
//!
//! Everything config-derived renders from the one [`ConfigSnapshot`] the whole
//! process shares — the same resolved truth the gateway boots from — so doctor
//! can never disagree with the gateway about precedence or credential
//! semantics. Resolution never aborts: problems arrive as `ConfigIssue`s and
//! are all shown, not just the first.
//!
//! The two db-backed sections (home override, run ledger) follow the standard
//! CLI read path: a reachable gateway → `GET /api/*` (it holds the exclusive
//! db lock); none → open the db directly.

use crate::infra::rendezvous;
use crate::services::operator_control::{OperatorControl, OperatorQuery, OperatorQueryResult};
use komo_config::{ChannelState, ConfigSnapshot, IssueSeverity, wechat_cred_path};

/// Status glyph for a channel/credential line.
const OK: &str = "✓";
const OFF: &str = "·";
const BAD: &str = "✗";

fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

pub async fn doctor(config: &ConfigSnapshot, control: &OperatorControl) -> anyhow::Result<()> {
    println!("home: {}", config.runtime.home.display());

    // The operator backend was resolved once by the caller; the db-backed
    // sections below reuse it, and the gateway line reports which side it hit.
    gateway_health(control.via_gateway());

    issue_health(config);
    model_health(config);
    schedule_health(config);
    policy_health(config, control).await;
    println!("\nchannels:");
    channel_health(config);
    home_channel_health(control, config).await;
    cron_health(control).await;
    run_health(control).await;
    Ok(())
}

/// Scheduled cron jobs (cron.db): count, disabled ones, and any whose last run
/// failed — the operator's "is my weekly job actually running" glance.
async fn cron_health(control: &OperatorControl) {
    use crate::domain::cron::{CronJobStatus, CronRunStatus};
    println!("\ncron jobs:");
    let fetched = control
        .query(OperatorQuery::CronJobs)
        .await
        .map(|r| match r {
            OperatorQueryResult::CronJobs(jobs) => jobs,
            _ => unreachable!("CronJobs query answers with CronJobs"),
        });
    let jobs = match fetched {
        Ok(jobs) => jobs,
        Err(e) => {
            println!("  {BAD} could not read cron store: {e:#}");
            return;
        }
    };
    if jobs.is_empty() {
        println!("  (none — `komo cron add <name> <schedule> <command>`)");
        return;
    }
    for job in &jobs {
        let mark = if job.status != CronJobStatus::Active {
            OFF
        } else if job.last_status == Some(CronRunStatus::Failed) {
            BAD
        } else {
            OK
        };
        let state = match job.status {
            CronJobStatus::Active => format!("next {}", local_time(job.next_run_at)),
            CronJobStatus::Paused => "paused".to_string(),
            CronJobStatus::Done => "done".to_string(),
        };
        let last = match (&job.last_status, job.last_run_at) {
            (Some(status), Some(at)) => {
                format!(", last {} {}", status.as_str(), local_time(at))
            }
            _ => String::new(),
        };
        println!("  {mark} {}  [{}]  {state}{last}", job.name, job.schedule);
    }
}

/// Is a gateway process actually running and answering? (The channel lines
/// below describe *configuration*; this is the live process.)
fn gateway_health(reachable: bool) {
    match (rendezvous::read(), reachable) {
        (Some(info), true) => println!(
            "\ngateway: {OK} running (pid {}, api {}:{})",
            info.pid, info.bind, info.port
        ),
        (Some(info), false) => println!(
            "\ngateway: {BAD} advertised (pid {}) but not answering — stale {} or mid-restart?",
            info.pid,
            rendezvous::path().display()
        ),
        (None, _) => println!("\ngateway: {OFF} not running (db opened directly)"),
    }
}

/// Every problem resolution recorded, fatal first in resolution order. The
/// gateway refuses to start on a fatal issue; warnings are safe to run with.
fn issue_health(config: &ConfigSnapshot) {
    let issues = &config.report.issues;
    if issues.is_empty() {
        return;
    }
    println!("\nconfig issues:");
    for issue in issues {
        let mark = match issue.severity {
            IssueSeverity::Fatal => BAD,
            IssueSeverity::Warning => "!",
        };
        println!("  {mark} {}: {}", issue.path, issue.message);
    }
}

/// The resolved provider/model and whether its credential is present.
fn model_health(config: &ConfigSnapshot) {
    // An unparsable provider is already listed under config issues; the model
    // line then shows the fallback resolution actually in effect.
    let model = &config.runtime.model;
    let provider = model.provider;
    println!("\nmodel: {} / {}", provider.name(), model.model);
    if provider.uses_api_key() {
        let has_key = config.report.key_present(provider);
        let mark = if has_key { OK } else { BAD };
        println!(
            "  {mark} {} {}",
            provider.api_key_var(),
            if has_key { "set" } else { "MISSING" }
        );
    } else {
        // Codex authenticates from ~/.codex/auth.json — validate that
        // login rather than looking for an env key.
        match komo_infra::codex::CodexAuth::load() {
            Ok(_) => println!("  {OK} Codex OAuth (~/.codex/auth.json)"),
            Err(e) => println!("  {BAD} Codex auth: {e}"),
        }
    }
}

/// Maintenance cron, daily briefing (opt-in), dreaming, and the workday gate.
fn schedule_health(config: &ConfigSnapshot) {
    let rt = &config.runtime;
    println!("\nsweeps:");
    println!("  maintenance  {}", rt.maintenance_schedule);
    match &rt.briefing_schedule {
        Some(s) => {
            let gate = if rt.briefing_workdays_only {
                " (Chinese workdays only)"
            } else {
                ""
            };
            println!("  briefing     {s}{gate}");
        }
        None => println!("  briefing     {OFF} disabled (set briefing_schedule to enable)"),
    }
    match &rt.dream_schedule {
        Some(s) => println!("  dreaming     {s}"),
        None => println!("  dreaming     {OFF} disabled"),
    }
    println!("  reminders    every minute");
    println!("  tasks        every minute");
    println!("  cron jobs    every minute (see `komo cron list`)");
}

/// The permission policy: configured?, rule count, load errors, and the two
/// runtime grant sources (saved prompts, scheduled jobs).
async fn policy_health(config: &ConfigSnapshot, control: &OperatorControl) {
    use crate::domain::policy::Verdict;
    let report = &config.runtime.policy;
    println!("\npolicy:");
    // Saved grants are reported whether or not a [policy] table exists — they are
    // accumulated at runtime, so an operator with no config can still have them.
    let saved = komo_infra::permissions_store::PermissionsStore::load(&config.runtime.home);
    if !saved.is_empty() {
        println!(
            "  {OK} {} saved grant(s) from approval prompts  (see `komo policy saved list`)",
            saved.len()
        );
    }
    // Job grants likewise: a job created purely in conversation carries
    // unattended permissions with no [policy] table anywhere.
    if let Ok(OperatorQueryResult::CronJobs(jobs)) = control.query(OperatorQuery::CronJobs).await {
        let granting = jobs.iter().filter(|j| !j.grants.is_empty()).count();
        if granting > 0 {
            println!(
                "  {OK} {granting} job(s) with their own unattended grants  \
                 (see `komo policy list`)"
            );
        }
    }
    if !report.configured {
        println!("  {OFF} no [policy] table — Normal/Dangerous actions ask interactively");
        return;
    }
    let d = match report.policy.default_normal() {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::Ask => "ask",
    };
    println!(
        "  {OK} {} rule(s), default_normal = {d}  (see `komo policy list`)",
        report.policy.rules().len()
    );
    if !report.invalid.is_empty() {
        println!(
            "  {BAD} {} invalid rule(s) ignored — fix [[policy.rule]] in config.toml",
            report.invalid.len()
        );
    }
}

/// One line per ingress channel: enabled?, credentials present?
fn channel_health(config: &ConfigSnapshot) {
    let rt = &config.runtime;
    fn line<T>(label: &str, state: &ChannelState<T>) {
        match state {
            ChannelState::Ready(_) => println!("  {OK} {label:<14} enabled"),
            ChannelState::Disabled => println!("  {OFF} {label:<14} disabled"),
            ChannelState::Misconfigured(e) => println!("  {BAD} {label:<14} {e}"),
        }
    }
    line("feishu", &rt.feishu);
    line("telegram", &rt.telegram);
    // The api channel is always on (it's how the CLI reaches a running gateway);
    // `enabled` only widens it from loopback-only to externally reachable.
    match &rt.api {
        ChannelState::Ready(cfg) if cfg.port != 0 => {
            println!(
                "  {OK} {:<14} enabled (external {}:{})",
                "api", cfg.bind, cfg.port
            )
        }
        ChannelState::Ready(_) => println!("  {OK} {:<14} on (loopback-only, CLI)", "api"),
        ChannelState::Misconfigured(e) => println!("  {BAD} {:<14} {e}", "api"),
        ChannelState::Disabled => unreachable!("the api channel is always on"),
    }

    // WeChat resolves with no credential check (login is QR-based, creds in a
    // separate file), so verify the file ourselves.
    match &rt.wechat {
        ChannelState::Ready(_) => {
            if wechat_cred_path().exists() {
                println!("  {OK} {:<14} enabled", "wechat");
            } else {
                println!(
                    "  {BAD} {:<14} enabled but not logged in (run `komo channel wechat login`)",
                    "wechat"
                );
            }
        }
        ChannelState::Disabled => println!("  {OFF} {:<14} disabled", "wechat"),
        ChannelState::Misconfigured(e) => println!("  {BAD} {:<14} {e}", "wechat"),
    }

    // The homeassistant tool (agent queries/controls HA on demand).
    let ha_tool = if rt.homeassistant_tool.is_some() {
        format!("{OK} HASS_TOKEN set")
    } else {
        format!("{OFF} HASS_TOKEN unset (homeassistant tool not registered)")
    };
    println!("  {ha_tool}");
}

/// Resolved proactive-output home: the `/sethome` runtime override (db) wins
/// over the config `home_chat` fallback (feishu first).
async fn home_channel_health(control: &OperatorControl, config: &ConfigSnapshot) {
    println!("\nhome channel (proactive output):");
    let over = control
        .query(OperatorQuery::HomeOverride)
        .await
        .map(|r| match r {
            OperatorQueryResult::HomeOverride(over) => over,
            _ => unreachable!("HomeOverride query answers with HomeOverride"),
        });
    match over {
        Ok(Some(session)) => println!("  {OK} /sethome override → {session}"),
        Ok(None) => match config_home_chat(config) {
            Some((platform, chat)) => {
                println!("  {OK} config home_chat → {platform}:{chat}")
            }
            None => {
                println!("  {OFF} none set — proactive output falls back to the macOS notifier")
            }
        },
        Err(e) => println!("  {BAD} could not read home setting: {e:#}"),
    }
}

/// The config `home_chat` fallback, feishu-first (matches `HomeNotifier`).
fn config_home_chat(config: &ConfigSnapshot) -> Option<(&'static str, String)> {
    let rt = &config.runtime;
    if let Some(chat) = rt.feishu.ready().and_then(|c| c.home_chat.clone()) {
        return Some(("feishu", chat));
    }
    if let Some(chat) = rt.telegram.ready().and_then(|c| c.home_chat.clone()) {
        return Some(("telegram", chat));
    }
    if let Some(chat) = rt.wechat.ready().and_then(|c| c.home_chat.clone()) {
        return Some(("wechat", chat));
    }
    None
}

/// Recent run-ledger health: how many of the last 50 turns failed, with the
/// most recent few. The roadmap §9 "last error" view.
async fn run_health(control: &OperatorControl) {
    println!("\nrecent runs:");
    let fetched = control
        .query(OperatorQuery::Runs { limit: 50 })
        .await
        .map(|r| match r {
            OperatorQueryResult::Runs(runs) => runs,
            _ => unreachable!("Runs query answers with Runs"),
        });
    let runs = match fetched {
        Ok(r) => r,
        Err(e) => {
            println!("  {BAD} could not read run ledger: {e:#}");
            return;
        }
    };
    if runs.is_empty() {
        println!("  (no runs recorded)");
        return;
    }
    let failed: Vec<_> = runs
        .iter()
        .filter(|r| r.status == crate::domain::run::RunStatus::Failed)
        .collect();
    println!("  last {} turns, {} failed", runs.len(), failed.len());
    for r in failed.iter().take(3) {
        let msg = if r.error.is_empty() { "—" } else { &r.error };
        println!("  {BAD} {} {} {}", r.id, local_time(r.started_at), msg);
    }
}
