//! The Python plugin host: `~/.komo/plugins/*.py` become callable tools.
//!
//! This is the first thing in komo that mounts tools into a *running* process
//! rather than at wiring, so it is where the catalog's runtime half earns its
//! keep. Writing a plugin file makes a tool appear on the next turn; deleting
//! it makes the tool go away; a host that crashes takes its tools with it and
//! brings them back when it restarts.
//!
//! The protocol and the child process live in `komo-pyhost`; the adapter that
//! makes a plugin's function look like a [`Tool`] lives in `komo-tools`. What
//! lives here is the lifecycle: when to spawn, what to mount it into, and what
//! to do when it dies.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use komo_core::domain::catalog::{Registration, ToolCatalog};
use komo_core::domain::hooks::{StepDecision, StepHook};
use komo_core::domain::policy::{Category, Policy};
use komo_core::domain::tool::Tool;
use komo_pyhost::{HostEvent, HostManifest, PluginToolDef, PyHost, SharedHost};
use komo_tools::plugin::PyTool;
use komo_tools::run_code::RunCodeTool;

use super::{Plugin, Scope, ToolCx, ToolRegistry};

/// The interpreter the host runs on.
///
/// Not configurable yet: every platform komo targets ships one under this name,
/// and a plugin needing a specific environment is better served by installing
/// its dependencies into that interpreter than by komo learning to pick one.
const PYTHON: &str = "python3";

/// How long to wait before restarting a host that exited, and the ceiling that
/// backoff climbs to.
///
/// A host that dies on startup (a syntax error in the embedded runtime, an
/// interpreter that vanished) would otherwise respawn in a tight loop; a host
/// that died once should come back promptly, because the tools are gone until
/// it does.
const RESTART_DELAY: Duration = Duration::from_secs(2);
const RESTART_DELAY_MAX: Duration = Duration::from_secs(60);

/// Which runtimes plugin tools are offered to.
///
/// Not the briefing runtime: every plugin call is approval-gated and briefing
/// turns have no one to ask, so mounting there would cost a schema per turn to
/// produce a refusal.
const PLUGIN_SCOPE: Scope = Scope::AGENTIC;

pub struct PyHostPlugin;

#[async_trait]
impl Plugin for PyHostPlugin {
    fn name(&self) -> &'static str {
        "pyhost"
    }

    async fn setup_tools(&self, reg: &mut ToolRegistry, cx: &ToolCx<'_>) -> anyhow::Result<()> {
        let plugins_dir = cx.config.runtime.home.join("plugins");
        if !plugins_dir.is_dir() {
            // No directory means no plugins can exist. Creating one the user
            // did not ask for, and paying an interpreter to watch it, is worse
            // than waiting for the deliberate act of making it.
            tracing::debug!(
                dir = %plugins_dir.display(),
                "no plugin directory; the python plugin host is not started"
            );
            return Ok(());
        }

        // A policy that denies plugins outright is answered by not starting the
        // host at all: the tools would be dropped from every catalog anyway,
        // and an interpreter running plugin code nobody may call is worse than
        // pointless.
        let policy = &cx.config.runtime.policy.policy;
        if wholly_denied(policy) {
            tracing::info!(
                "[policy] denies the `plugin` category; the python plugin host is not started"
            );
            return Ok(());
        }

        // The slot the supervisor keeps current across restarts. `run_code`
        // holds it rather than a host handle, so a restarted host is picked up
        // without re-registering the tool.
        let host = SharedHost::default();
        // Whether any loaded plugin registered for `pre_step`, kept current by
        // the supervisor across reloads and restarts.
        let registered = Arc::new(AtomicBool::new(false));

        // Code mode: one tool that runs a program, in place of the model
        // calling three tools in three rounds. Built per executor because a
        // program's calls go back through that executor — see `run_code`.
        let shared = host.clone();
        reg.tool_from_executor(
            PLUGIN_SCOPE,
            Box::new(move |executor| {
                Arc::new(RunCodeTool::new(shared.clone(), executor.downgrade())) as Arc<dyn Tool>
            }),
        );

        // Between-round hooks, for a plugin that wants to see the turn in
        // flight. Registered unconditionally but *inert* unless a plugin asked
        // for the point: the hook checks a flag before it costs an RPC, so a
        // deployment with no `@hook("pre_step")` pays nothing per round.
        reg.step_hook(
            PLUGIN_SCOPE,
            Arc::new(PluginStepHook {
                host: host.clone(),
                registered: registered.clone(),
            }),
        );

        let catalogs = cx.catalogs.covered_by(PLUGIN_SCOPE);
        let supervisor = Supervisor {
            home: cx.config.runtime.home.clone(),
            plugins_dir,
            catalogs,
            host,
            pre_step_registered: registered,
        };
        // Supervised in the background: a plugin host that will not start must
        // cost the plugins, never the boot. Its first attempt is made here
        // rather than deferred, so the usual case (a working host) has its
        // tools mounted before the first turn.
        tokio::spawn(supervisor.run());
        Ok(())
    }
}

fn wholly_denied(policy: &Policy) -> bool {
    policy.wholly_denied(Category::Plugin, None)
}

/// Owns one plugin host across restarts, and the registrations that keep its
/// tools mounted.
struct Supervisor {
    home: PathBuf,
    plugins_dir: PathBuf,
    catalogs: Vec<Arc<ToolCatalog>>,
    /// Published so `run_code` can reach whichever host is current.
    host: SharedHost,
    /// Set while a loaded plugin has a `pre_step` hook. The between-round hook
    /// reads it to decide whether a round is worth an RPC at all.
    pre_step_registered: Arc<AtomicBool>,
}

impl Supervisor {
    /// Run until the process ends: spawn, mount, follow the host's events, and
    /// respawn when it dies.
    async fn run(self) {
        let mut delay = RESTART_DELAY;
        loop {
            match self.serve_one_host().await {
                // The host exited. Its registrations dropped with the loop
                // body, so its tools are already out of every catalog — the
                // model will not be offered a tool nothing can answer.
                Ok(status) => {
                    tracing::warn!(
                        status = %status,
                        delay_secs = delay.as_secs(),
                        "python plugin host exited; restarting"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        delay_secs = delay.as_secs(),
                        "python plugin host unavailable; retrying"
                    );
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RESTART_DELAY_MAX);
        }
    }

    /// One host's whole life: spawn it, mount what it reports, re-mount on
    /// every change, and return when it goes away.
    async fn serve_one_host(&self) -> anyhow::Result<String> {
        let (host, mut events) = PyHost::spawn(PYTHON, &self.home, &self.plugins_dir).await?;
        let manifest = host.manifest().await?;
        self.publish_hooks(&manifest);
        // Published only once the host answered: a handle to a process that
        // cannot speak the protocol is worse than none, because `run_code`
        // would hand programs to it and wait.
        self.host.set(Some(host.clone()));

        // Held across the loop: dropping these unmounts, which is exactly what
        // should happen when this function returns for any reason.
        let mut mounted = self.mount(&host, manifest.tools);

        while let Some(event) = events.recv().await {
            match event {
                HostEvent::ManifestChanged(manifest) => {
                    self.publish_hooks(&manifest);
                    // Replace wholesale rather than diffing: the host reports
                    // the complete set, and a batch mount is one change to the
                    // model's view — so one prompt-cache invalidation, whether
                    // one tool changed or ten.
                    drop(std::mem::take(&mut mounted));
                    mounted = self.mount(&host, manifest.tools);
                }
                HostEvent::Exited { status } => return Ok(self.retire(status)),
            }
        }
        Ok(self.retire("event stream closed".to_string()))
    }

    /// Stop advertising a host that is gone, so `run_code` says "not running"
    /// instead of handing a program to a dead process.
    fn retire(&self, status: String) -> String {
        self.host.set(None);
        // A host that is gone has no hooks either — leaving the flag set would
        // cost an RPC per round to reach nothing.
        self.pre_step_registered.store(false, Ordering::Relaxed);
        status
    }

    /// Record whether the loaded plugins registered a `pre_step` hook.
    fn publish_hooks(&self, manifest: &HostManifest) {
        let registered = manifest.has_hook(PRE_STEP);
        if registered != self.pre_step_registered.swap(registered, Ordering::Relaxed) {
            tracing::info!(
                registered,
                "python plugin `pre_step` hook availability changed"
            );
        }
    }

    /// Mount `tools` into every catalog this supervisor covers.
    fn mount(&self, host: &PyHost, tools: Vec<PluginToolDef>) -> Vec<Registration> {
        if tools.is_empty() {
            tracing::info!("python plugin host ready; no plugins registered a tool");
            return Vec::new();
        }
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        tracing::info!(
            count = tools.len(),
            tools = %names.join(", "),
            "mounted python plugin tools"
        );
        self.catalogs
            .iter()
            .map(|catalog| {
                // Built per catalog: each `PyTool` leaks its name, but they are
                // the same handful of strings and the alternative is sharing
                // one `Arc<dyn Tool>` across catalogs whose lifetimes differ.
                let adapted: Vec<Arc<dyn Tool>> = tools
                    .iter()
                    .map(|def| Arc::new(PyTool::new(host.clone(), def.clone())) as Arc<dyn Tool>)
                    .collect();
                catalog.mount_all(adapted)
            })
            .collect()
    }
}

/// The hook point name, as the SDK spells it.
const PRE_STEP: &str = "pre_step";

/// Bridges komo's between-round hook to whatever the plugins registered.
///
/// Inert unless a plugin asked for the point: a hook runs on every round of
/// every turn, so "nobody registered one" has to cost nothing rather than an
/// RPC that returns nothing.
struct PluginStepHook {
    host: SharedHost,
    registered: Arc<AtomicBool>,
}

#[async_trait]
impl StepHook for PluginStepHook {
    fn name(&self) -> &'static str {
        "pyhost"
    }

    async fn pre_step(&self, session_id: &str, round: usize) -> StepDecision {
        if !self.registered.load(Ordering::Relaxed) {
            return StepDecision::Continue;
        }
        let Some(host) = self.host.get() else {
            return StepDecision::Continue;
        };
        let payload = serde_json::json!({ "session_id": session_id, "round": round });
        let outcome = match host.hook(PRE_STEP, payload).await {
            Ok(outcome) => outcome,
            Err(error) => {
                // A hook that could not run leaves the turn alone. Failing the
                // turn over an advisory extension would be the wrong trade: the
                // conversation is the thing that matters.
                tracing::warn!(%error, round, "python `pre_step` hook failed; continuing");
                return StepDecision::Continue;
            }
        };
        if let Some(reply) = outcome.stop {
            return StepDecision::Stop(reply);
        }
        match outcome.inject {
            Some(text) => StepDecision::Inject(text),
            None => StepDecision::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::policy::{Effect, Matcher, Rule, Verdict};

    fn deny_plugins() -> Policy {
        Policy::new(
            vec![Rule {
                channels: None,
                category: Category::Plugin,
                matcher: Matcher::Any,
                value: String::new(),
                access: None,
                effect: Effect::Deny,
                include_dangerous: false,
                unattended: false,
            }],
            Verdict::Ask,
        )
    }

    /// An operator who denied the category gets no interpreter at all — the
    /// tools would be dropped from every catalog anyway, so running plugin code
    /// nobody may call is strictly worse than not running it.
    #[test]
    fn a_wholly_denied_policy_stops_the_host_from_starting() {
        assert!(wholly_denied(&deny_plugins()));
        assert!(!wholly_denied(&Policy::default()));
    }

    /// Plugin tools reach the three tool-wielding runtimes and not the
    /// unattended briefing one, which has no one to approve a call.
    #[test]
    fn plugin_tools_are_offered_to_the_agentic_runtimes_only() {
        assert!(PLUGIN_SCOPE.contains(Scope::MAIN));
        assert!(PLUGIN_SCOPE.contains(Scope::SUBAGENT));
        assert!(PLUGIN_SCOPE.contains(Scope::CRON));
        assert!(!PLUGIN_SCOPE.contains(Scope::BRIEFING));
    }
}
