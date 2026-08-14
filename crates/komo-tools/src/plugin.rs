//! Adapter turning one tool a Python plugin registered into a komo [`Tool`].
//!
//! Everything protocol-shaped lives in `komo-pyhost`; this file is only the
//! seam. One instance per mounted tool, all sharing the host handle — the host
//! is one child process, and a second one per tool would multiply interpreters
//! for nothing.
//!
//! Unlike [`McpTool`](crate::mcp::McpTool), these mount and unmount while komo
//! runs: the point of the plugin host is that writing a `.py` file makes a tool
//! appear. The catalog handles that (see `domain::catalog`); what this file
//! owns is the call.

use async_trait::async_trait;
use komo_core::domain::approval::{ActionRef, ApprovalRequest};
use komo_core::domain::context::ToolContext;
use komo_core::domain::tool::{
    APPROVAL_BOUND, RetryHint, Tool, ToolError, ToolOutput, TransientError,
};
use komo_pyhost::{PluginToolDef, PyHost, PyHostError};
use serde_json::Value;

/// Prefix that namespaces every plugin-registered tool.
///
/// Without it a plugin's `read` would collide with komo's own in the catalog's
/// name-keyed map — last registration wins, and the shadowing would be silent.
/// It also gives `[policy]` and the executor's category mapping one prefix to
/// key on, the same trick `mcp__` plays.
const NAMESPACE: &str = "py";

/// The catalog name for a plugin tool: `py__<name>`.
pub fn qualified_name(tool: &str) -> String {
    format!("{NAMESPACE}__{tool}")
}

/// One tool registered by a plugin file.
pub struct PyTool {
    host: PyHost,
    /// The namespaced catalog name. Leaked because [`Tool::name`] is
    /// `&'static str` while a plugin's names are only known once the host has
    /// imported it.
    ///
    /// A plugin edited in a loop therefore leaks a little per reload — bounded
    /// by how many distinct tool names the session ever sees, which is small,
    /// and the alternative is threading a lifetime through every tool in komo.
    name: &'static str,
    /// Plugin-authored, likewise leaked. This text goes into the system prompt.
    description: &'static str,
    /// The name to send to the host — the plugin's own, un-namespaced.
    plugin_name: String,
    schema: Value,
}

impl PyTool {
    pub fn new(host: PyHost, def: PluginToolDef) -> Self {
        let name: &'static str = String::leak(qualified_name(&def.name));
        let description: &'static str = String::leak(def.description);
        Self {
            host,
            name,
            description,
            plugin_name: def.name,
            schema: normalize_schema(def.parameters),
        }
    }
}

#[async_trait]
impl Tool for PyTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    /// Plugin code runs unsandboxed in the interpreter komo spawned, so every
    /// call is approval-gated — a plugin can do anything the `shell` tool can,
    /// and it arrived without anyone reading it.
    ///
    /// An operator who wants a specific plugin tool to run unprompted says so:
    ///
    /// ```toml
    /// [[policy.rule]]
    /// category = "plugin"
    /// match    = "exact"
    /// value    = "strlen"
    /// effect   = "allow"
    /// ```
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let request = ApprovalRequest::normal(format!("run plugin tool `{}`", self.plugin_name))
            .with_scope_key(format!("plugin:{}", self.plugin_name))
            .with_action(ActionRef::Plugin {
                tool: self.plugin_name.clone(),
            });
        let decision = ctx.decide(&request).await;
        if !decision.is_allowed() {
            return Err(ToolError::Denied(
                decision
                    .feedback()
                    .unwrap_or("Plugin tool call was not approved.")
                    .to_string(),
            ));
        }

        let text = self
            .host
            .call(&self.plugin_name, input)
            .await
            .map_err(|error| map_error(error, &self.plugin_name))?;
        Ok(ToolOutput::text(text))
    }

    /// Long enough to outlast a human reading the approval prompt — the same
    /// bound every approval-gated tool carries.
    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(APPROVAL_BOUND)
    }

    /// Never. A plugin is arbitrary code the model cannot inspect: whether a
    /// second run is safe is exactly what komo does not know.
    fn idempotent(&self) -> bool {
        false
    }
}

/// Map a host failure onto the executor's retry classification.
///
/// Only "the host was not there" retries, and even then the call provably never
/// ran — the request could not be written. A plugin that raised will raise
/// again, and re-running it could double-apply whatever it did before failing.
fn map_error(error: PyHostError, tool: &str) -> ToolError {
    match error {
        PyHostError::Unavailable(message) => ToolError::Failed(
            TransientError::new(
                RetryHint::Connection,
                format!("plugin host unavailable while calling `{tool}`: {message}"),
            )
            .into(),
        ),
        PyHostError::Plugin(message) => {
            ToolError::Failed(anyhow::anyhow!("plugin tool `{tool}` failed: {message}"))
        }
    }
}

/// Force the schema into the object shape providers accept.
///
/// A plugin's schema is derived from a Python signature, so it is already an
/// object — but a hand-written `parameters` could be anything, and a provider
/// rejects the whole request (every tool, not just this one) over one malformed
/// schema.
fn normalize_schema(schema: Value) -> Value {
    match schema {
        Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("object") => {
            Value::Object(map)
        }
        _ => serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, parameters: Value) -> PluginToolDef {
        PluginToolDef {
            name: name.to_string(),
            description: "does a thing".to_string(),
            parameters,
        }
    }

    /// The namespace is what keeps a plugin from silently shadowing a built-in
    /// tool of the same name.
    #[test]
    fn a_plugin_tool_is_namespaced() {
        assert_eq!(qualified_name("read"), "py__read");
    }

    #[test]
    fn a_signature_derived_schema_is_kept_as_is() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        });
        assert_eq!(normalize_schema(schema.clone()), schema);
    }

    /// One malformed schema would make the provider reject the whole request,
    /// taking every other tool down with it — so it is replaced, not passed on.
    #[test]
    fn a_malformed_schema_degrades_to_no_arguments() {
        let empty = serde_json::json!({ "type": "object", "properties": {}, "required": [] });
        assert_eq!(normalize_schema(serde_json::json!("nonsense")), empty);
        assert_eq!(normalize_schema(serde_json::json!(null)), empty);
        assert_eq!(
            normalize_schema(serde_json::json!({ "type": "array" })),
            empty
        );
    }

    /// A host that was not there never ran the call, so retrying is safe; a
    /// plugin that raised will raise again, and might have half-applied
    /// something first.
    #[test]
    fn only_an_unreachable_host_is_retryable() {
        let unavailable = map_error(PyHostError::Unavailable("exited".into()), "t");
        let ToolError::Failed(error) = unavailable else {
            panic!("expected a failure");
        };
        let transient = error
            .downcast_ref::<TransientError>()
            .expect("an unavailable host classifies itself");
        assert_eq!(transient.hint, RetryHint::Connection);

        let raised = map_error(PyHostError::Plugin("kaboom".into()), "t");
        let ToolError::Failed(error) = raised else {
            panic!("expected a failure");
        };
        assert!(
            error.downcast_ref::<TransientError>().is_none(),
            "a plugin that raised must not be retried"
        );
        assert!(format!("{error:#}").contains("kaboom"));
    }

    /// The def's own name is what goes over the wire; only the catalog sees the
    /// prefix. A rule written `value = "strlen"` must match.
    #[test]
    fn the_wire_name_stays_un_namespaced() {
        let host_def = def("strlen", serde_json::json!({ "type": "object" }));
        assert_eq!(host_def.name, "strlen");
        assert_eq!(qualified_name(&host_def.name), "py__strlen");
    }
}
