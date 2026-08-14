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
use std::time::Duration;

use async_trait::async_trait;
use komo_core::domain::catalog::{Registration, ToolCatalog};
use komo_core::domain::policy::{Category, Policy};
use komo_core::domain::tool::Tool;
use komo_pyhost::{HostEvent, PluginToolDef, PyHost};
use komo_tools::plugin::PyTool;

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

    async fn setup_tools(&self, _reg: &mut ToolRegistry, cx: &ToolCx<'_>) -> anyhow::Result<()> {
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

        let catalogs = cx.catalogs.covered_by(PLUGIN_SCOPE);
        let supervisor = Supervisor {
            home: cx.config.runtime.home.clone(),
            plugins_dir,
            catalogs,
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
        let tools = host.manifest().await?;

        // Held across the loop: dropping these unmounts, which is exactly what
        // should happen when this function returns for any reason.
        let mut mounted = self.mount(&host, tools);

        while let Some(event) = events.recv().await {
            match event {
                HostEvent::ManifestChanged(tools) => {
                    // Replace wholesale rather than diffing: the host reports
                    // the complete set, and a batch mount is one change to the
                    // model's view — so one prompt-cache invalidation, whether
                    // one tool changed or ten.
                    drop(std::mem::take(&mut mounted));
                    mounted = self.mount(&host, tools);
                }
                HostEvent::Exited { status } => return Ok(status),
            }
        }
        Ok("event stream closed".to_string())
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
