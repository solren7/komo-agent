//! A Python plugin host: run `~/.komo/plugins/*.py` in a child process and
//! call the tools they register.
//!
//! Its own crate for the reason `komo-provider` and `komo-mcp` are: it
//! references nothing else in komo, so it compiles in parallel and an edit here
//! never rebuilds the agent. Nothing here knows about `Tool`, `ToolContext`, or
//! the catalog — the adapter that turns a [`PluginToolDef`] into a komo tool
//! lives in `komo-tools`, and deciding when to mount it lives in wiring.
//!
//! **Out of process, on purpose.** Embedding a Python interpreter would put a
//! GIL next to the async runtime, weld komo to one Python version, and give a
//! plugin the address space of the agent that runs it. A child process costs a
//! pipe and buys isolation: a plugin that segfaults, hangs, or leaks costs the
//! plugins and nothing else, and the supervisor starts a new one.
//!
//! The protocol is newline-delimited JSON over stdin/stdout — one object per
//! line. Requests travel both ways: komo asks for the manifest, a plugin call,
//! or a program run ([`PyHost::run_code`]); the host asks komo to run one of
//! *its* tools on behalf of a running program. The two id spaces cannot collide
//! because komo's requests carry positive ids and the host's carry negative
//! ones. The host also pushes `manifest/changed` unasked when a plugin file is
//! edited, which is what makes "write a `.py` file and the tool appears" work
//! without a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, warn};

/// The SDK and host, embedded so a `cargo install`ed komo carries its own
/// Python side — there is no second package to install and no version of it
/// that can drift from the binary talking to it. Written out on spawn.
const SDK_SOURCE: &str = include_str!("../python/komo_plugin.py");
const HOST_SOURCE: &str = include_str!("../python/host.py");

/// Bumped when the wire contract changes. The host reports the version it
/// speaks in its manifest; a mismatch is refused rather than half-understood.
pub const PROTOCOL_VERSION: u32 = 3;

/// Ceiling on one request to the host. A plugin doing real work can be slow, so
/// this is generous — it exists to catch a host that stopped answering, not to
/// bound legitimate work. The tool layer applies its own per-call timeout too.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum PyHostError {
    /// The host could not be started or has gone away. Retryable in the sense
    /// that a restarted host may answer — but the call never ran, so no side
    /// effect can have landed.
    #[error("plugin host unavailable: {0}")]
    Unavailable(String),
    /// The host answered, and the answer was an error — a plugin raising, an
    /// unknown tool name. Retrying re-sends the same request to code that
    /// already rejected it.
    #[error("{0}")]
    Plugin(String),
}

impl PyHostError {
    /// Whether a retry could plausibly succeed *and* is safe. Only for a host
    /// that never received the request.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// One tool a plugin registered, as the host describes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, derived from the Python signature.
    pub parameters: serde_json::Value,
}

/// What the host reports when asked for its manifest.
#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    #[serde(default)]
    protocol: u32,
    #[serde(default)]
    tools: Vec<PluginToolDef>,
    #[serde(default)]
    hooks: Vec<String>,
}

/// What a host has loaded: its tools, and which hook points anything registered
/// for.
///
/// The hook list is what lets komo skip the host entirely on a round where no
/// plugin asked to be consulted — a hook point nobody uses must cost a running
/// turn nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostManifest {
    pub tools: Vec<PluginToolDef>,
    pub hooks: Vec<String>,
}

impl HostManifest {
    /// Whether any plugin registered for `point`.
    pub fn has_hook(&self, point: &str) -> bool {
        self.hooks.iter().any(|hook| hook == point)
    }
}

/// What a hook point decided. Absent fields mean "nothing to say".
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct HookOutcome {
    /// Text to put in front of the model for this round.
    #[serde(default)]
    pub inject: Option<String>,
    /// End the turn with this answer instead of driving another round.
    #[serde(default)]
    pub stop: Option<String>,
}

/// What one `run_code` program produced.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodeResult {
    /// Everything the program printed, in order. Its own output channel: a
    /// program reports intermediate work by printing rather than by stuffing it
    /// all into the return value.
    #[serde(default)]
    pub logs: String,
    /// What the program returned, or `None` when it returned nothing.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
}

/// What one of a program's tool calls answers with.
///
/// Two channels, because a tool's text is laid out for a reader and a program is
/// not one: `content` is the string the model would have been shown, and
/// `structured` is the same result as data — `Null` for a tool that reports no
/// structured view. A program that has to *compute* on a result reads the second
/// rather than re-parsing the first.
pub struct ToolAnswer {
    pub content: String,
    pub structured: serde_json::Value,
}

impl ToolAnswer {
    /// An answer carrying text alone — what a tool with no structured view
    /// gives.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            structured: serde_json::Value::Null,
        }
    }
}

/// One komo tool call a running program made.
struct ToolRequest {
    /// The host's own (negative) request id, echoed back with the answer.
    id: i64,
    name: String,
    args: serde_json::Value,
}

/// A message from the host that komo did not ask for.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// A plugin file changed and the host reloaded: this is the new set.
    ManifestChanged(HostManifest),
    /// The host process exited. Whoever owns it decides whether to restart —
    /// this crate reports, it does not resurrect.
    Exited { status: String },
}

/// A running plugin host. Cheap to clone; every clone talks to the same child.
#[derive(Clone)]
pub struct PyHost {
    inner: Arc<Inner>,
}

struct Inner {
    /// Request id → where its response goes. The reader task drains this.
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<serde_json::Value, PyHostError>>>>,
    /// `run_code` request id → where that program's tool calls go. A program
    /// tags every call with the run it belongs to, which is what lets two turns
    /// run code over one host without their calls crossing.
    code_runs: Mutex<HashMap<i64, mpsc::UnboundedSender<ToolRequest>>>,
    next_id: AtomicI64,
    outbound: mpsc::UnboundedSender<String>,
    /// The child, kept so dropping the host kills it rather than orphaning a
    /// Python process for the rest of the session.
    child: Mutex<Option<Child>>,
}

/// The current plugin host, shared with whoever needs to reach it later.
///
/// A [`PyHost`] handle is one child process: when the supervisor restarts a
/// dead host, the old handle is not the new one. Anything long-lived that calls
/// into the host — a tool registered at wiring, for instance — therefore holds
/// this slot rather than a handle, and reads whichever host is current at call
/// time. Empty means no host is running, which is a real answer: the caller
/// says so rather than failing obscurely.
#[derive(Clone, Default)]
pub struct SharedHost(Arc<std::sync::RwLock<Option<PyHost>>>);

impl SharedHost {
    /// The host that is running right now, if any.
    pub fn get(&self) -> Option<PyHost> {
        match self.0.read() {
            Ok(host) => host.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Publish a newly started host, or `None` when one has gone away.
    pub fn set(&self, host: Option<PyHost>) {
        match self.0.write() {
            Ok(mut slot) => *slot = host,
            Err(poisoned) => *poisoned.into_inner() = host,
        }
    }
}

/// Where the plugin host's own files live, written fresh on every spawn.
///
/// Not the plugin directory: these are komo's, and a user editing them would be
/// editing something the next launch overwrites. Keeping them apart also means
/// `~/.komo/plugins` contains only what the operator (or the agent) authored.
pub fn runtime_dir(home: &Path) -> PathBuf {
    home.join("pyhost")
}

impl PyHost {
    /// Start a host for the plugins in `plugins_dir`.
    ///
    /// `python` is the interpreter to run (`python3` unless configured
    /// otherwise) and `home` is where the embedded SDK is materialized. Returns
    /// the handle plus the stream of unsolicited host events; dropping the
    /// receiver is fine — events are then discarded.
    pub async fn spawn(
        python: &str,
        home: &Path,
        plugins_dir: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<HostEvent>), PyHostError> {
        let runtime = runtime_dir(home);
        write_runtime(&runtime)?;

        let mut child = Command::new(python)
            // Unbuffered: the protocol is a conversation, and a buffered child
            // would answer only once its pipe filled.
            .arg("-u")
            .arg(runtime.join("host.py"))
            .arg(plugins_dir)
            .env("PYTHONPATH", &runtime)
            // A plugin importing something that prompts, or a stray `input()`,
            // must fail rather than park the host forever.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is the host's log (and where plugin `print`s land); it is
            // inherited so it reaches komo's own stderr and daily log file.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                PyHostError::Unavailable(format!("could not start `{python}`: {error}"))
            })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        let (outbound, outbound_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            pending: Mutex::new(HashMap::new()),
            code_runs: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            outbound,
            child: Mutex::new(Some(child)),
        });

        tokio::spawn(write_loop(stdin, outbound_rx));
        tokio::spawn(read_loop(inner.clone(), stdout, events));

        Ok((Self { inner }, events_rx))
    }

    /// Ask the host what it has loaded. Also the liveness check — a host that
    /// cannot answer this is not usable.
    pub async fn manifest(&self) -> Result<HostManifest, PyHostError> {
        let value = self.request("manifest", serde_json::json!({})).await?;
        let manifest: Manifest = serde_json::from_value(value)
            .map_err(|error| PyHostError::Plugin(format!("malformed manifest: {error}")))?;
        if manifest.protocol != PROTOCOL_VERSION {
            return Err(PyHostError::Unavailable(format!(
                "plugin host speaks protocol {}, this komo speaks {PROTOCOL_VERSION}",
                manifest.protocol
            )));
        }
        Ok(HostManifest {
            tools: manifest.tools,
            hooks: manifest.hooks,
        })
    }

    /// Run one hook point's registered functions.
    ///
    /// `payload` is the point's arguments, passed to each function by keyword.
    /// Callers are expected to skip this entirely when the manifest lists no
    /// hook for the point — it sits on a turn's critical path.
    pub async fn hook(
        &self,
        point: &str,
        payload: serde_json::Value,
    ) -> Result<HookOutcome, PyHostError> {
        let value = self
            .request(
                "hook",
                serde_json::json!({ "point": point, "payload": payload }),
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|error| PyHostError::Plugin(format!("malformed hook outcome: {error}")))
    }

    /// Run one plugin tool. The returned string is what the model sees.
    pub async fn call(&self, name: &str, args: serde_json::Value) -> Result<String, PyHostError> {
        let value = self
            .request("call", serde_json::json!({ "name": name, "args": args }))
            .await?;
        Ok(value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// Run one program on the host, servicing the komo tool calls it makes.
    ///
    /// `dispatch` is how a call gets back into komo's own executor — the
    /// program's `tools.read(path=...)` becomes one invocation of it, so a
    /// sub-call pays the same approval, ledger and cap the model's own call
    /// would. Sub-calls are serviced while the program runs; the future
    /// resolves when the program returns.
    pub async fn run_code<F, Fut>(
        &self,
        source: &str,
        dispatch: F,
    ) -> Result<CodeResult, PyHostError>
    where
        F: Fn(String, serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<ToolAnswer, String>>,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
        self.inner.pending.lock().await.insert(id, response_tx);
        self.inner.code_runs.lock().await.insert(id, calls_tx);

        let cleanup = || async {
            self.inner.pending.lock().await.remove(&id);
            self.inner.code_runs.lock().await.remove(&id);
        };

        if let Err(error) = self.send(serde_json::json!({
            "id": id,
            "method": "run_code",
            "params": { "source": source },
        })) {
            cleanup().await;
            return Err(error);
        }

        // Service the program's calls until it answers. One at a time, which is
        // what the host does anyway — a synchronous program blocks on each call
        // — and what keeps a program's side effects in the order it wrote them.
        tokio::pin!(response_rx);
        let outcome = loop {
            tokio::select! {
                answer = &mut response_rx => break match answer {
                    Ok(result) => result,
                    Err(_) => Err(PyHostError::Unavailable(
                        "the plugin host exited before the program finished".into(),
                    )),
                },
                Some(request) = calls_rx.recv() => {
                    let (answer, is_error) = match dispatch(request.name, request.args).await {
                        Ok(answer) => (answer, false),
                        Err(message) => (
                            ToolAnswer { content: message, structured: serde_json::Value::Null },
                            true,
                        ),
                    };
                    // Answer even if the send fails: a host that went away is
                    // about to fail the program's response too.
                    let _ = self.send(serde_json::json!({
                        "id": request.id,
                        "result": {
                            "content": answer.content,
                            "structured": answer.structured,
                            "is_error": is_error,
                        },
                    }));
                }
            }
        };
        cleanup().await;

        let value = outcome?;
        serde_json::from_value(value)
            .map_err(|error| PyHostError::Plugin(format!("malformed program result: {error}")))
    }

    /// Ask the host to exit, then wait briefly for it. Dropping the handle also
    /// kills the child (`kill_on_drop`); this is the polite path, which lets a
    /// plugin's own cleanup run.
    pub async fn shutdown(&self) {
        let _ = self.send(serde_json::json!({ "method": "shutdown" }));
        let mut child = self.inner.child.lock().await;
        if let Some(mut child) = child.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
            let _ = child.start_kill();
        }
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, PyHostError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        if let Err(error) = self.send(serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        })) {
            self.inner.pending.lock().await.remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            // The reader task dropped the sender: the host died mid-request.
            Ok(Err(_)) => Err(PyHostError::Unavailable(
                "the plugin host exited before answering".into(),
            )),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(PyHostError::Unavailable(format!(
                    "the plugin host did not answer `{method}` within {}s",
                    REQUEST_TIMEOUT.as_secs()
                )))
            }
        }
    }

    fn send(&self, message: serde_json::Value) -> Result<(), PyHostError> {
        self.inner
            .outbound
            .send(message.to_string())
            .map_err(|_| PyHostError::Unavailable("the plugin host is not running".into()))
    }
}

/// Materialize the embedded SDK and host next to komo's home.
///
/// Rewritten on every spawn rather than only when missing: these files belong
/// to the binary, and a stale copy from an older komo talking a newer protocol
/// is exactly the failure this avoids.
fn write_runtime(runtime: &Path) -> Result<(), PyHostError> {
    let write = |name: &str, source: &str| -> Result<(), PyHostError> {
        std::fs::write(runtime.join(name), source).map_err(|error| {
            PyHostError::Unavailable(format!(
                "could not write the plugin host to {}: {error}",
                runtime.display()
            ))
        })
    };
    std::fs::create_dir_all(runtime).map_err(|error| {
        PyHostError::Unavailable(format!("could not create {}: {error}", runtime.display()))
    })?;
    write("komo_plugin.py", SDK_SOURCE)?;
    write("host.py", HOST_SOURCE)
}

/// Feed the child's stdin. Ends when the host handle is dropped.
async fn write_loop(mut stdin: ChildStdin, mut outbound: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = outbound.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err()
            || stdin.write_all(b"\n").await.is_err()
            || stdin.flush().await.is_err()
        {
            break;
        }
    }
}

/// Demultiplex the child's stdout: responses to their waiters, everything else
/// to the event stream. Ends when the child's stdout closes, which is also how
/// "the host died" is detected — every pending request is failed rather than
/// left to time out one by one.
async fn read_loop(
    inner: Arc<Inner>,
    stdout: tokio::process::ChildStdout,
    events: mpsc::UnboundedSender<HostEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                warn!(%error, "plugin host stdout failed");
                break;
            }
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            warn!(line = %truncate(&line), "unparseable line from the plugin host");
            continue;
        };

        // A *negative* id is the host asking komo for something — the only
        // request it originates is a running program's tool call.
        if let Some(id) = message
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id < 0)
        {
            route_tool_call(&inner, id, &message).await;
            continue;
        }

        // A positive id is the response to something komo sent; anything else
        // is a notification.
        if let Some(id) = message.get("id").and_then(serde_json::Value::as_i64) {
            let Some(waiter) = inner.pending.lock().await.remove(&id) else {
                debug!(id, "response for an unknown request id");
                continue;
            };
            let answer = match message.get("error") {
                Some(error) => Err(PyHostError::Plugin(
                    error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the plugin failed")
                        .to_string(),
                )),
                None => Ok(message.get("result").cloned().unwrap_or_default()),
            };
            let _ = waiter.send(answer);
            continue;
        }

        match message.get("method").and_then(serde_json::Value::as_str) {
            Some("manifest/changed") => {
                let params = message.get("params");
                let tools = params
                    .and_then(|p| p.get("tools"))
                    .cloned()
                    .unwrap_or_default();
                let hooks = params
                    .and_then(|p| p.get("hooks"))
                    .cloned()
                    .unwrap_or_default();
                match (
                    serde_json::from_value::<Vec<PluginToolDef>>(tools),
                    serde_json::from_value::<Vec<String>>(hooks),
                ) {
                    (Ok(tools), Ok(hooks)) => {
                        let _ =
                            events.send(HostEvent::ManifestChanged(HostManifest { tools, hooks }));
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        warn!(%error, "malformed manifest from the plugin host")
                    }
                }
            }
            Some("log") => {
                let params = message.get("params");
                let text = params
                    .and_then(|p| p.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let level = params
                    .and_then(|p| p.get("level"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("info");
                match level {
                    "warn" | "error" => warn!(target: "pyhost", "{text}"),
                    _ => debug!(target: "pyhost", "{text}"),
                }
            }
            other => debug!(?other, "ignoring an unknown notification"),
        }
    }

    // The host is gone. Fail everyone still waiting — a request that will never
    // be answered should say so now, not in two minutes.
    inner.code_runs.lock().await.clear();
    let waiting: Vec<_> = inner.pending.lock().await.drain().collect();
    for (_, waiter) in waiting {
        let _ = waiter.send(Err(PyHostError::Unavailable(
            "the plugin host exited".into(),
        )));
    }
    let status = match inner.child.lock().await.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(status)) => status.to_string(),
            _ => "stdout closed".to_string(),
        },
        None => "shut down".to_string(),
    };
    let _ = events.send(HostEvent::Exited { status });
}

/// Hand a program's tool call to the `run_code` invocation that owns it.
///
/// A call whose run is unknown is answered rather than dropped: the program is
/// blocked on it, and a program hung forever is worse than one told its call
/// went nowhere.
async fn route_tool_call(inner: &Arc<Inner>, id: i64, message: &serde_json::Value) {
    let params = message.get("params");
    let run = params
        .and_then(|p| p.get("run"))
        .and_then(serde_json::Value::as_i64);
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = params
        .and_then(|p| p.get("args"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let sender = match run {
        Some(run) => inner.code_runs.lock().await.get(&run).cloned(),
        None => None,
    };
    match sender {
        Some(sender) => {
            let _ = sender.send(ToolRequest { id, name, args });
        }
        None => {
            warn!(id, ?run, tool = %name, "tool call from an unknown program run");
            let _ = inner.outbound.send(
                serde_json::json!({
                    "id": id,
                    "error": { "message": "this program run is no longer active" },
                })
                .to_string(),
            );
        }
    }
}

fn truncate(line: &str) -> String {
    line.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    //! These run a real Python interpreter against the real embedded host —
    //! the protocol has two sides and testing one against a mock of the other
    //! proves only that the mock agrees with itself.

    use super::*;

    fn python() -> Option<String> {
        for candidate in ["python3", "python"] {
            if std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok()
            {
                return Some(candidate.to_string());
            }
        }
        None
    }

    /// A temp dir holding a `plugins/` directory, cleaned up on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("komo-pyhost-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(path.join("plugins")).unwrap();
            Self(path)
        }

        fn home(&self) -> &Path {
            &self.0
        }

        fn plugins(&self) -> PathBuf {
            self.0.join("plugins")
        }

        fn write(&self, name: &str, source: &str) {
            std::fs::write(self.plugins().join(name), source).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const GREETER: &str = r#"
from komo_plugin import tool

@tool("Greet someone by name.")
def greet(name: str, excited: bool = False) -> str:
    return f"hello {name}" + ("!" if excited else "")
"#;

    #[tokio::test]
    async fn a_plugin_is_loaded_described_and_callable() {
        let Some(python) = python() else {
            eprintln!("no python interpreter; skipping");
            return;
        };
        let scratch = Scratch::new("callable");
        scratch.write("greeter.py", GREETER);

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let tools = host.manifest().await.unwrap().tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "greet");
        assert_eq!(tools[0].description, "Greet someone by name.");
        // The signature is the contract: annotated types and which arguments
        // are required both come from it.
        assert_eq!(tools[0].parameters["properties"]["name"]["type"], "string");
        assert_eq!(
            tools[0].parameters["properties"]["excited"]["type"],
            "boolean"
        );
        assert_eq!(tools[0].parameters["required"], serde_json::json!(["name"]));

        let out = host
            .call("greet", serde_json::json!({ "name": "komo" }))
            .await
            .unwrap();
        assert_eq!(out, "hello komo");
        let out = host
            .call(
                "greet",
                serde_json::json!({ "name": "komo", "excited": true }),
            )
            .await
            .unwrap();
        assert_eq!(out, "hello komo!");

        host.shutdown().await;
    }

    /// A plugin raising is a result the model can work with, not a dead host:
    /// the error comes back as `Plugin` (never retried, since the code already
    /// rejected the call) and the next call still works.
    #[tokio::test]
    async fn a_raising_plugin_answers_with_an_error_and_the_host_survives() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("raising");
        scratch.write(
            "boom.py",
            r#"
from komo_plugin import tool

@tool("Always fails.")
def boom() -> str:
    raise ValueError("kaboom")

@tool("Always works.")
def fine() -> str:
    return "ok"
"#,
        );

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        host.manifest().await.unwrap();

        let error = host
            .call("boom", serde_json::json!({}))
            .await
            .expect_err("a raising tool is an error");
        assert!(format!("{error}").contains("kaboom"), "{error}");
        assert!(!error.retryable(), "the plugin already rejected this call");

        assert_eq!(
            host.call("fine", serde_json::json!({})).await.unwrap(),
            "ok"
        );
        host.shutdown().await;
    }

    /// One broken file must not cost the working ones — the agent writing a
    /// plugin with a syntax error should lose that plugin and nothing else.
    #[tokio::test]
    async fn a_plugin_that_fails_to_import_does_not_take_the_others_down() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("broken");
        scratch.write("good.py", GREETER);
        scratch.write("bad.py", "this is not python(");

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        let tools = host.manifest().await.unwrap().tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "greet");
        host.shutdown().await;
    }

    /// The point of the whole thing: write a file, and the tool shows up
    /// without a restart. The host pushes the new manifest unasked.
    #[tokio::test]
    async fn writing_a_plugin_file_pushes_a_new_manifest() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("hotreload");

        let (host, mut events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        assert!(host.manifest().await.unwrap().tools.is_empty());

        scratch.write("late.py", GREETER);

        let event = tokio::time::timeout(std::time::Duration::from_secs(15), events.recv())
            .await
            .expect("the host should notice a new plugin file")
            .expect("the event stream should stay open");
        match event {
            HostEvent::ManifestChanged(manifest) => {
                assert_eq!(manifest.tools.len(), 1);
                assert_eq!(manifest.tools[0].name, "greet");
            }
            other => panic!("expected a manifest change, got {other:?}"),
        }

        // And it is callable straight away, without asking for the manifest.
        assert_eq!(
            host.call("greet", serde_json::json!({ "name": "you" }))
                .await
                .unwrap(),
            "hello you"
        );
        host.shutdown().await;
    }

    /// A plugin's own `print` must not corrupt the protocol stream — it is a
    /// debugging habit, not a bug, and the connection has to survive it.
    #[tokio::test]
    async fn plugin_stdout_does_not_corrupt_the_protocol() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("printing");
        scratch.write(
            "chatty.py",
            r#"
import sys
from komo_plugin import tool

print("noise at import time")

@tool("Prints, then answers.")
def chatty() -> str:
    print("noise during the call")
    print({"id": 1, "result": "a forged response"})
    return "answered"
"#,
        );

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        assert_eq!(host.manifest().await.unwrap().tools.len(), 1);
        assert_eq!(
            host.call("chatty", serde_json::json!({})).await.unwrap(),
            "answered"
        );
        // Still healthy after all that noise.
        assert_eq!(host.manifest().await.unwrap().tools.len(), 1);
        host.shutdown().await;
    }

    // ── Hooks ────────────────────────────────────────────────────────────────

    /// A plugin's `pre_step` hook injects text, and the manifest advertises the
    /// point so komo knows a round is worth asking about at all.
    #[tokio::test]
    async fn a_pre_step_hook_is_advertised_and_injects() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("hook-inject");
        scratch.write(
            "guard.py",
            r#"
from komo_plugin import hook

@hook("pre_step")
def remind(session_id, round):
    if round >= 2:
        return f"round {round}: wrap up"
"#,
        );

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        let manifest = host.manifest().await.unwrap();
        assert!(manifest.has_hook("pre_step"));
        assert!(manifest.tools.is_empty(), "a hook is not a tool");

        // Round 1: the hook returned nothing, so nothing is injected.
        let quiet = host
            .hook(
                "pre_step",
                serde_json::json!({ "session_id": "s", "round": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(quiet, HookOutcome::default());

        let loud = host
            .hook(
                "pre_step",
                serde_json::json!({ "session_id": "s", "round": 2 }),
            )
            .await
            .unwrap();
        assert_eq!(loud.inject.as_deref(), Some("round 2: wrap up"));
        assert!(loud.stop.is_none());

        host.shutdown().await;
    }

    /// `stop(...)` ends the turn, and the hooks after it do not run.
    #[tokio::test]
    async fn a_hook_can_stop_the_turn_and_short_circuits_the_rest() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("hook-stop");
        scratch.write(
            "guard.py",
            r#"
from komo_plugin import hook, stop

@hook("pre_step")
def halt(session_id, round):
    return stop("that is enough")

@hook("pre_step")
def never(session_id, round):
    return "this must not be delivered"
"#,
        );

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        host.manifest().await.unwrap();

        let outcome = host
            .hook(
                "pre_step",
                serde_json::json!({ "session_id": "s", "round": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(outcome.stop.as_deref(), Some("that is enough"));
        assert!(outcome.inject.is_none());

        host.shutdown().await;
    }

    /// A hook runs on every round of every turn, so a broken one must cost its
    /// own effect and nothing else — not the round, and not the hooks beside it.
    #[tokio::test]
    async fn a_raising_hook_is_skipped_and_the_others_still_run() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("hook-raise");
        scratch.write(
            "guard.py",
            r#"
from komo_plugin import hook

@hook("pre_step")
def broken(session_id, round):
    raise ValueError("kaboom")

@hook("pre_step")
def wrong_type(session_id, round):
    return 42

@hook("pre_step")
def fine(session_id, round):
    return "still here"
"#,
        );

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        host.manifest().await.unwrap();

        let outcome = host
            .hook(
                "pre_step",
                serde_json::json!({ "session_id": "s", "round": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(outcome.inject.as_deref(), Some("still here"));

        host.shutdown().await;
    }

    /// A host with no hooks says so, which is what lets komo skip the RPC.
    #[tokio::test]
    async fn a_host_without_hooks_advertises_none() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("hook-none");
        scratch.write("greeter.py", GREETER);

        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        let manifest = host.manifest().await.unwrap();
        assert!(!manifest.has_hook("pre_step"));
        assert_eq!(manifest.tools.len(), 1);

        host.shutdown().await;
    }

    // ── Code mode ────────────────────────────────────────────────────────────

    /// Dispatch that answers every call by echoing what it was asked for, and
    /// records the calls in order.
    fn recording_dispatch() -> (
        impl Fn(String, serde_json::Value) -> std::future::Ready<Result<ToolAnswer, String>>,
        Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let dispatch = move |name: String, args: serde_json::Value| {
            recorder.lock().unwrap().push((name.clone(), args.clone()));
            std::future::ready(Ok(ToolAnswer::text(format!("{name} says hi"))))
        };
        (dispatch, seen)
    }

    /// A program runs, prints, calls back into komo, and returns — the whole
    /// point of code mode in one exchange.
    #[tokio::test]
    async fn a_program_calls_komo_tools_and_returns_a_value() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let (dispatch, seen) = recording_dispatch();
        let result = host
            .run_code(
                r#"
print("starting")
first = tools.read(path="a.txt")
second = tools.shell(command="ls")
return {"first": first, "second": second}
"#,
                dispatch,
            )
            .await
            .unwrap();

        assert_eq!(result.logs.trim(), "starting");
        let value = result.result.expect("the program returned a value");
        assert_eq!(value["first"], "read says hi");
        assert_eq!(value["second"], "shell says hi");

        // The calls reached komo as the program wrote them, in order, with
        // their arguments intact.
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read");
        assert_eq!(calls[0].1["path"], "a.txt");
        assert_eq!(calls[1].0, "shell");

        host.shutdown().await;
    }

    /// A result reaches the program on two channels: the text it would show a
    /// reader, and the same result as data. The program computes on the second
    /// — parsing the first is what got the early programs' answers wrong.
    #[tokio::test]
    async fn a_tool_result_carries_its_structured_view_into_the_program() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode-structured");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let result = host
            .run_code(
                r#"
out = tools.read(path="a.txt")
return {"text": str(out), "lines": out.structured["total_lines"]}
"#,
                |_name, _args| {
                    std::future::ready(Ok(ToolAnswer {
                        // Laid out for a reader, exactly as `read` renders it.
                        content: "a.txt (lines 1-2 of 2)\n1│one\n2│two".to_string(),
                        structured: serde_json::json!({ "total_lines": 2 }),
                    }))
                },
            )
            .await
            .unwrap();

        let value = result.result.expect("the program returned a value");
        // Still a `str`: a program that only wants the text is untouched by the
        // second channel existing.
        assert!(value["text"].as_str().unwrap().contains("1│one"));
        assert_eq!(value["lines"], 2);

        host.shutdown().await;
    }

    /// A tool with no structured view hands the program `None` rather than
    /// something it has to tell apart from real data.
    #[tokio::test]
    async fn a_tool_without_a_structured_view_answers_none() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode-nostructure");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let result = host
            .run_code("return tools.time().structured is None", |_n, _a| {
                std::future::ready(Ok(ToolAnswer::text("9am")))
            })
            .await
            .unwrap();
        assert_eq!(result.result.unwrap(), true);

        host.shutdown().await;
    }

    /// A tool that failed raises inside the program, so it can catch and
    /// recover — the same choice komo makes for the model, one level down.
    #[tokio::test]
    async fn a_failing_tool_raises_inside_the_program_and_can_be_caught() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode-error");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let result = host
            .run_code(
                r#"
try:
    tools.read(path="missing")
    return "no error"
except ToolError as error:
    return f"caught: {error.message}"
"#,
                |_name, _args| std::future::ready(Err("no such file".to_string())),
            )
            .await
            .unwrap();
        assert_eq!(result.result.unwrap(), "caught: no such file");

        host.shutdown().await;
    }

    /// A positional call says what to do about it. komo dispatches by argument
    /// name, so there is no order to bind a positional to — and the first
    /// programs written against `tools` called them positionally, spending a
    /// whole round on a host traceback to learn it.
    #[tokio::test]
    async fn a_positional_tool_call_is_refused_by_name() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode-positional");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let error = host
            .run_code("return tools.read(\"/etc/hosts\")", |_n, _a| {
                std::future::ready(Ok(ToolAnswer::text("")))
            })
            .await
            .expect_err("a positional call cannot be dispatched");
        let rendered = format!("{error}");
        assert!(rendered.contains("keyword arguments only"), "{rendered}");
        assert!(rendered.contains("tools.read()"), "{rendered}");

        host.shutdown().await;
    }

    /// A program that raises answers with its traceback rather than killing the
    /// host: the traceback is what the model rewrites from.
    #[tokio::test]
    async fn a_raising_program_returns_its_traceback_and_the_host_survives() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode-raise");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let error = host
            .run_code("return 1 / 0", |_n, _a| {
                std::future::ready(Ok(ToolAnswer::text("")))
            })
            .await
            .expect_err("a raising program is an error");
        let rendered = format!("{error}");
        assert!(rendered.contains("ZeroDivisionError"), "{rendered}");
        assert!(!error.retryable(), "the program already failed");
        // The host's own dispatch frames are three of the four lines in a
        // typical failure and name a file the program's author never wrote.
        assert!(
            !rendered.contains("host.py"),
            "the traceback should start at the program's own frame: {rendered}"
        );

        // Still usable afterwards.
        let ok = host
            .run_code("return 2 + 2", |_n, _a| {
                std::future::ready(Ok(ToolAnswer::text("")))
            })
            .await
            .unwrap();
        assert_eq!(ok.result.unwrap(), 4);

        host.shutdown().await;
    }

    /// Two programs over one host must not have their tool calls crossed — the
    /// run id each call carries is what keeps them apart.
    #[tokio::test]
    async fn concurrent_programs_keep_their_tool_calls_apart() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode-concurrent");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        // Each program's dispatch answers with its own marker, so a crossed
        // call would come back with the other program's answer.
        let one = host.run_code("return tools.whoami()", |_n, _a| {
            std::future::ready(Ok(ToolAnswer::text("first")))
        });
        let two = host.run_code("return tools.whoami()", |_n, _a| {
            std::future::ready(Ok(ToolAnswer::text("second")))
        });
        let (one, two) = tokio::join!(one, two);
        assert_eq!(one.unwrap().result.unwrap(), "first");
        assert_eq!(two.unwrap().result.unwrap(), "second");

        host.shutdown().await;
    }

    /// A program that returns nothing says so, rather than the word `None` —
    /// which the model would read as a value.
    #[tokio::test]
    async fn a_program_that_returns_nothing_reports_no_value() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("codemode-void");
        let (host, _events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();

        let result = host
            .run_code("print('side effect only')", |_n, _a| {
                std::future::ready(Ok(ToolAnswer::text("")))
            })
            .await
            .unwrap();
        assert!(result.result.is_none());
        assert_eq!(result.logs.trim(), "side effect only");

        host.shutdown().await;
    }

    /// A host that dies fails everyone waiting on it immediately and reports
    /// the exit, so the supervisor can restart rather than time out per call.
    #[tokio::test]
    async fn a_dead_host_fails_pending_calls_and_reports_the_exit() {
        let Some(python) = python() else { return };
        let scratch = Scratch::new("dying");
        scratch.write(
            "suicide.py",
            r#"
import os
from komo_plugin import tool

@tool("Kills the host process.")
def die() -> str:
    os._exit(1)
"#,
        );

        let (host, mut events) = PyHost::spawn(&python, scratch.home(), &scratch.plugins())
            .await
            .unwrap();
        host.manifest().await.unwrap();

        let error = host
            .call("die", serde_json::json!({}))
            .await
            .expect_err("the host died mid-call");
        assert!(error.retryable(), "the call never completed: {error}");

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the exit should be reported")
            .unwrap();
        assert!(matches!(event, HostEvent::Exited { .. }), "{event:?}");
    }
}
