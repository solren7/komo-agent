//! External MCP servers as a plugin. Degrades per server: an unreachable
//! server is a warning, never a failed boot.

use std::sync::Arc;

use async_trait::async_trait;

use komo_core::domain::tool::Tool;
use komo_tools::mcp::McpTool;

use super::{Plugin, Scope, ToolCx, ToolRegistry};

pub struct McpPlugin;

#[async_trait]
impl Plugin for McpPlugin {
    fn name(&self) -> &'static str {
        "mcp"
    }

    async fn setup_tools(&self, reg: &mut ToolRegistry, cx: &ToolCx<'_>) -> anyhow::Result<()> {
        for tool in build_mcp_tools(&cx.config.runtime.mcp_servers).await {
            // Not offered to the briefing runtime, same as before the plugin
            // split: every MCP call is approval-gated and a briefing has no
            // approver to ask.
            reg.tool(Scope::AGENTIC, tool);
        }
        Ok(())
    }
}

/// Connect the configured MCP servers and turn their allowlisted tools into
/// komo tools, built **once** and shared (`Arc`) by every executor — each
/// [`McpTool`] leaks its name and description to satisfy `Tool`'s `&'static
/// str`, so constructing them per executor would leak the same strings again.
pub async fn build_mcp_tools(servers: &[komo_config::McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    if servers.is_empty() {
        return Vec::new();
    }
    let allowlists: std::collections::BTreeMap<String, Vec<String>> = servers
        .iter()
        .map(|s| (s.name.clone(), s.tools.clone()))
        .collect();
    let clients = komo_mcp::connect_all(
        servers
            .iter()
            .map(|s| (s.name.clone(), s.url.clone(), s.token.clone()))
            .collect(),
    )
    .await;

    let mut mounted: Vec<Arc<dyn Tool>> = Vec::new();
    for client in clients {
        let server = client.server().to_string();
        let offered = match client.list_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                tracing::warn!(server = %server, %error, "mcp tools/list failed — no tools mounted");
                continue;
            }
        };
        // Empty allowlist = `all_tools = true`; config resolution rejects the
        // empty-and-not-all case, so this is never an accidental wildcard.
        let allow = allowlists.get(&server).cloned().unwrap_or_default();
        let wanted = |name: &str| allow.is_empty() || allow.iter().any(|t| t == name);

        let offered_names: Vec<String> = offered.iter().map(|t| t.name.clone()).collect();
        // A listed tool the server doesn't have is almost always a typo, and it
        // would otherwise be invisible — the model just never sees the tool.
        for missing in allow.iter().filter(|t| !offered_names.contains(t)) {
            tracing::warn!(
                server = %server,
                tool = %missing,
                available = %offered_names.join(", "),
                "mcp tool listed in config is not offered by the server"
            );
        }

        let mut names = Vec::new();
        for def in offered.into_iter().filter(|d| wanted(&d.name)) {
            let tool = Arc::new(McpTool::new(client.clone(), def));
            names.push(tool.name().to_string());
            mounted.push(tool);
        }
        tracing::info!(
            server = %server,
            mounted = names.len(),
            offered = offered_names.len(),
            tools = %names.join(", "),
            "mcp tools mounted"
        );
    }
    mounted
}
