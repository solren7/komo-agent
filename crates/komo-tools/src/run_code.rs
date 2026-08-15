//! `run_code`: let the model write a Python program that calls komo's tools.
//!
//! ## Why a program beats another tool
//!
//! The model's usual way to combine tools is to call one, read the result, and
//! call the next — a model round-trip per step, with every intermediate result
//! spent as context. A program does the same work in one call: the loop, the
//! filtering and the glue run in the host, and only what the program returns
//! becomes context.
//!
//! It is also the answer to "how does the agent extend itself without paying
//! for it". Mounting a new tool changes the schema block, which invalidates the
//! provider's cached prompt prefix once (see `domain::catalog`). Writing a
//! function inside a program changes nothing at all: the catalog is untouched,
//! the prefix survives, and the composition costs a single call. Durable
//! composition still belongs in a plugin file; a one-off belongs here.
//!
//! ## The sub-call is a real call
//!
//! `tools.shell(command="…")` inside a program goes back through the *same*
//! [`ToolExecutor`] the model's own calls do — approval, policy, redaction,
//! the run ledger, the result cap. A program is a way to sequence komo's tools,
//! never a way around what gating them means. That is also why the executor
//! handle here is weak: `run_code` sits in the catalog it dispatches through.

use std::sync::Arc;

use async_trait::async_trait;
use komo_core::domain::catalog::CatalogSnapshot;
use komo_core::domain::context::ToolContext;
use komo_core::domain::llm::ToolCallReq;
use komo_core::domain::tool::{APPROVAL_BOUND, Tool, ToolError, ToolOutput, parse_args};
use komo_pyhost::{PyHostError, SharedHost, ToolAnswer};
use komo_services::tool_execution::{
    SpinDetector, ToolTurnContext, TurnResultBudget, WeakToolExecutor,
};
use serde::Deserialize;
use serde_json::Value;

/// Tools a program may not call, whatever the catalog says.
///
/// `run_code` itself, because a program spawning a program buys nothing and
/// costs an unbounded recursion; `ask_user`, because it suspends the *turn* on
/// a human answer and a program is not a turn — the sentinel would resolve into
/// a mid-program value nobody is waiting for.
const NOT_CALLABLE: &[&str] = &["run_code", "ask_user"];

#[derive(Deserialize)]
struct Args {
    /// The program body. Runs as a function, so a top-level `return` answers.
    source: String,
}

pub struct RunCodeTool {
    /// The slot, not a handle: a restarted host is a different handle, and a
    /// tool registered at wiring outlives any one of them.
    host: SharedHost,
    /// Weak, because this tool is registered in the catalog the executor
    /// dispatches against — see the module docs.
    executor: WeakToolExecutor,
}

impl RunCodeTool {
    pub fn new(host: SharedHost, executor: WeakToolExecutor) -> Self {
        Self { host, executor }
    }
}

#[async_trait]
impl Tool for RunCodeTool {
    fn name(&self) -> &'static str {
        "run_code"
    }

    fn description(&self) -> &'static str {
        "Run a Python program that can call komo's other tools through a `tools` \
         object (`tools.read(path=\"a.txt\")`). Use it to do in one call what \
         would otherwise take several rounds: loop over files, filter results, \
         combine outputs. Every tool call inside the program is gated exactly as \
         a direct call would be. `print(...)` reports progress and a top-level \
         `return` answers; define helper functions freely — they live only for \
         this program."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "The program body. Written as if inside a \
                                    function: `return` answers, `print` reports. \
                                    A failed tool call raises `ToolError` \
                                    (`.tool`, `.message`), which you may catch.",
                }
            },
            "required": ["source"],
        })
    }

    /// The program is gated by what it calls, not by being a program: each
    /// `tools.x(...)` inside it goes through the executor and prompts exactly
    /// as a direct call to `x` would. Gating the program itself as well would
    /// ask the operator to approve something whose effects are not yet known.
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: Args = parse_args(&input)?;
        let Some(host) = self.host.get() else {
            return Err(ToolError::Failed(anyhow::anyhow!(
                "the python plugin host is not running, so `run_code` cannot run \
                 a program; it restarts on its own — try again, or use the tools \
                 directly"
            )));
        };
        let Some(executor) = self.executor.upgrade() else {
            return Err(ToolError::Failed(anyhow::anyhow!(
                "the tool executor is gone; `run_code` cannot dispatch"
            )));
        };
        // The program's calls join *this* turn: same session, same approver,
        // and the same ledger run — so every sub-call is audited individually
        // and the turn's per-call budget keeps counting across them, which is
        // what bounds a runaway program.
        //
        // The *output* budget is not shared, and deliberately: it exists to
        // bound what enters the model's context, and a sub-call's result enters
        // the program, not the context. Only what the program returns is paid
        // for. Each result is still capped individually by the executor, exactly
        // as a direct call would be.
        let turn = ToolTurnContext {
            session: ctx.session.clone(),
            run: ctx.run.clone(),
            budget: TurnResultBudget::new(0),
            spin: SpinDetector::default(),
        };

        let callable = executor.snapshot();
        let outcome = host
            .run_code(&args.source, |name, args| {
                let executor = executor.clone();
                let turn = &turn;
                let callable = callable.clone();
                async move { dispatch(&executor, turn, &callable, name, args).await }
            })
            .await;

        match outcome {
            Ok(result) => Ok(render(result)),
            Err(PyHostError::Unavailable(message)) => Err(ToolError::Failed(anyhow::anyhow!(
                "the plugin host is unavailable, so `run_code` did not run: {message}"
            ))),
            // A program that raised is a result the model rewrites from, not a
            // tool failure to retry — same call the executor makes for invalid
            // input.
            Err(PyHostError::Plugin(message)) => Ok(ToolOutput::text(format!(
                "The program failed:\n{}",
                message.trim_end()
            ))),
        }
    }

    /// A program can legitimately run for as long as the tools it calls do,
    /// approval prompts included.
    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(APPROVAL_BOUND)
    }

    /// A program is arbitrary composition; whether running it twice is safe is
    /// exactly what komo cannot know.
    fn idempotent(&self) -> bool {
        false
    }

    /// The program is the payload, and it can be long. The ledger keeps the
    /// first lines — enough to recognize what ran — rather than a whole file.
    fn redact_args(&self, args: &str) -> String {
        elide_source(args)
    }
}

/// Keep the head of a long program for the ledger.
///
/// The audit record wants to say what ran, not keep a copy: the first lines are
/// enough to recognize a program, and a whole file per call would dominate the
/// ledger. Cut on a char boundary — a program is often not ASCII.
fn elide_source(args: &str) -> String {
    const KEEP: usize = 2_000;
    if args.len() <= KEEP {
        return args.to_string();
    }
    let head: String = args.chars().take(KEEP).collect();
    let elided = args.len() - head.len();
    format!("{head}…[{elided} bytes elided]")
}

/// Run one tool call a program made, through the executor.
///
/// Returns `Err(text)` for a call the program should see as a failure — an
/// unknown or forbidden name, or a tool that errored. The host turns that into
/// a `ToolError` the program may catch.
async fn dispatch(
    executor: &komo_services::tool_execution::ToolExecutor,
    turn: &ToolTurnContext,
    callable: &Arc<CatalogSnapshot>,
    name: String,
    args: Value,
) -> Result<ToolAnswer, String> {
    if NOT_CALLABLE.contains(&name.as_str()) {
        return Err(format!(
            "`{name}` cannot be called from a program; call it directly instead"
        ));
    }
    if callable.get(&name).is_none() {
        // Naming what *is* available beats a bare "unknown": the program was
        // written against a guess, and the fix is one edit away.
        return Err(format!(
            "no tool named `{name}`. Available: {}",
            callable
                .names()
                .filter(|n| !NOT_CALLABLE.contains(n))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let call = ToolCallReq {
        id: format!("code-{name}"),
        call_id: None,
        name,
        args: args.to_string(),
    };
    let mut outcomes = executor
        .execute_round(std::slice::from_ref(&call), turn)
        .await;
    let outcome = outcomes.pop();
    let content = outcome
        .as_ref()
        .map(|o| o.content.clone())
        .unwrap_or_default();
    // The executor answers a failure as content (the model is meant to recover
    // from it), so "did this work" has to be read off the text — the same
    // convention every other reader of an outcome uses.
    if content.starts_with("error:") || content.starts_with("tool `") {
        return Err(content);
    }
    Ok(ToolAnswer {
        content,
        structured: outcome.map(|o| o.structured).unwrap_or(Value::Null),
    })
}

/// Turn a finished program into the model's answer.
fn render(result: komo_pyhost::CodeResult) -> ToolOutput {
    let logs = result.logs.trim_end();
    let value = match &result.result {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => Some(other.to_string()),
    };
    let text = match (logs.is_empty(), value) {
        (true, None) => "(the program produced no output and returned nothing)".to_string(),
        (true, Some(value)) => value,
        (false, None) => logs.to_string(),
        (false, Some(value)) => format!("{logs}\n\n{value}"),
    };
    ToolOutput::text(text).with_structured(serde_json::json!({
        "returned": result.result,
        "printed_bytes": result.logs.len(),
    }))
}

/// The `tools` API text for the system prompt: one line per callable tool.
///
/// Rendered from the catalog, so it lists exactly what a program may call and
/// nothing else. Name-sorted like everything else the model is shown — an
/// unchanged tool set renders byte-identically, which is what keeps this text
/// from costing the provider's cached prefix on every turn.
pub fn sdk_note(catalog: &CatalogSnapshot) -> Option<String> {
    let mut lines: Vec<String> = catalog
        .tools()
        .filter(|tool| !NOT_CALLABLE.contains(&tool.name()))
        .map(|tool| {
            let args = argument_names(&tool.parameters_schema());
            format!("  tools.{}({})", tool.name(), args.join(", "))
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    lines.sort();
    Some(format!(
        "Inside `run_code`, these are callable as Python functions with keyword \
         arguments. Each is the same gated tool you can call directly, and each \
         returns the tool's output as a **str**; a failure raises \
         `ToolError`.\n{}\n\
         That str is the same text you would be shown — laid out for reading, \
         not for parsing. `read` returns a header line and then `N│text` \
         gutters; `grep` returns `Found N matches` and indented `Line N:` \
         entries. Do not compute on that layout. Every result also carries \
         `.structured`, the same result as data (`read` puts the page's own \
         lines in `.structured[\"text\"]`), which is `None` only for a tool that \
         reports no structured view — reach for a shell when that happens.",
        lines.join("\n")
    ))
}

/// Argument names from a tool's schema, required ones first and marked.
fn argument_names(schema: &Value) -> Vec<String> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut names: Vec<String> = properties
        .keys()
        .map(|name| {
            if required.contains(&name.as_str()) {
                name.clone()
            } else {
                // The model reads this as "optional", the same way a Python
                // signature with a default does.
                format!("{name}=None")
            }
        })
        .collect();
    // Required first, then alphabetical — a signature, not a set.
    names.sort_by_key(|name| (name.contains('='), name.clone()));
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::catalog::ToolCatalog;
    use komo_core::domain::tool::ToolOutput;

    struct Fake(&'static str, Value);

    #[async_trait]
    impl Tool for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "stand-in"
        }
        fn parameters_schema(&self) -> Value {
            self.1.clone()
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn schema(required: &[&str], optional: &[&str]) -> Value {
        let mut properties = serde_json::Map::new();
        for name in required.iter().chain(optional) {
            properties.insert(name.to_string(), serde_json::json!({ "type": "string" }));
        }
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    /// The note is a signature list, so the model can write a call without
    /// guessing: required arguments first, optional ones marked.
    #[test]
    fn the_sdk_note_renders_each_tool_as_a_python_signature() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &["limit"]))));
        catalog.register(Arc::new(Fake("time", schema(&[], &[]))));

        let note = sdk_note(&catalog.snapshot()).expect("two tools is not empty");
        assert!(note.contains("tools.read(path, limit=None)"), "{note}");
        assert!(note.contains("tools.time()"), "{note}");
    }

    /// The return type is stated because a model that has to guess gets it
    /// wrong: the first real program written against this note called `.get()`
    /// on what is a `str`.
    #[test]
    fn the_note_states_what_a_tool_call_returns() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        let note = sdk_note(&catalog.snapshot()).unwrap();
        assert!(note.contains("str"), "{note}");
        assert!(note.contains("ToolError"), "{note}");
    }

    /// Saying "str" was not enough: the first programs written against this note
    /// parsed `read`'s display page as data and computed line counts off the
    /// header and the gutter. The note has to say the text is laid out for
    /// reading, and name the channel that carries the result as data.
    #[test]
    fn the_note_warns_that_output_is_display_text() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        let note = sdk_note(&catalog.snapshot()).unwrap();
        assert!(note.contains("N│text"), "{note}");
        assert!(note.contains(".structured"), "{note}");
    }

    /// Byte stability is the whole reason this is worth generating rather than
    /// hand-writing: the same tool set must render identically every turn, or
    /// the prompt prefix changes for nothing.
    #[test]
    fn the_same_tool_set_renders_byte_identically_whatever_the_order() {
        let first = ToolCatalog::new();
        first.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        first.register(Arc::new(Fake("shell", schema(&["command"], &[]))));

        let second = ToolCatalog::new();
        second.register(Arc::new(Fake("shell", schema(&["command"], &[]))));
        second.register(Arc::new(Fake("read", schema(&["path"], &[]))));

        assert_eq!(sdk_note(&first.snapshot()), sdk_note(&second.snapshot()));
    }

    /// What a program may not call must not be advertised to it either.
    #[test]
    fn the_note_leaves_out_what_a_program_cannot_call() {
        let catalog = ToolCatalog::new();
        catalog.register(Arc::new(Fake("read", schema(&["path"], &[]))));
        catalog.register(Arc::new(Fake("run_code", schema(&["source"], &[]))));
        catalog.register(Arc::new(Fake("ask_user", schema(&["question"], &[]))));

        let note = sdk_note(&catalog.snapshot()).unwrap();
        // Only the listed signatures matter — the indented lines. The prose
        // around them names `run_code` itself (the thing being described) and
        // `tools.shell` (where to go for unformatted bytes).
        let listed: Vec<&str> = note.lines().filter(|l| l.starts_with("  tools.")).collect();
        assert_eq!(listed, vec!["  tools.read(path)"]);
    }

    /// An empty catalog has nothing to say — better no note than a heading over
    /// an empty list, which costs prompt bytes every turn to describe nothing.
    #[test]
    fn an_empty_catalog_produces_no_note() {
        let catalog = ToolCatalog::new();
        assert!(sdk_note(&catalog.snapshot()).is_none());
        catalog.register(Arc::new(Fake("run_code", schema(&["source"], &[]))));
        assert!(
            sdk_note(&catalog.snapshot()).is_none(),
            "a catalog with only uncallable tools is still nothing to say"
        );
    }

    fn code(logs: &str, result: Option<Value>) -> komo_pyhost::CodeResult {
        komo_pyhost::CodeResult {
            logs: logs.to_string(),
            result,
        }
    }

    /// Printed output and the returned value are different channels, and a
    /// program that used both must have both rendered.
    #[test]
    fn a_program_reports_what_it_printed_and_what_it_returned() {
        let out = render(code("step 1\nstep 2\n", Some(serde_json::json!("done"))));
        assert_eq!(out.text, "step 1\nstep 2\n\ndone");

        assert_eq!(render(code("", Some(serde_json::json!(42)))).text, "42");
        assert_eq!(render(code("just logs\n", None)).text, "just logs");
    }

    /// Silence is reported as silence: a program that printed nothing and
    /// returned nothing did run, and "nothing happened" is the honest answer.
    #[test]
    fn a_silent_program_says_so_rather_than_returning_an_empty_string() {
        let out = render(code("", None));
        assert!(out.text.contains("no output"), "{}", out.text);
    }

    /// A long program is recognizable in the ledger without storing the whole
    /// file — the audit record wants to say what ran, not keep a copy.
    #[test]
    fn a_long_program_is_elided_in_the_ledger() {
        let short = r#"{"source":"return 1"}"#;
        assert_eq!(elide_source(short), short);

        let long = format!(r#"{{"source":"{}"}}"#, "x".repeat(5_000));
        let redacted = elide_source(&long);
        assert!(redacted.len() < long.len());
        assert!(redacted.contains("bytes elided"), "{redacted}");
    }

    /// Cutting mid-character would make the ledger row unreadable — programs
    /// are routinely not ASCII.
    #[test]
    fn eliding_cuts_on_a_character_boundary() {
        let long = "读".repeat(3_000);
        let redacted = elide_source(&long);
        assert!(redacted.starts_with('读'));
        assert!(redacted.contains("bytes elided"));
    }
}
