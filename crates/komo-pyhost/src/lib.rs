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
//! line. komo sends requests carrying an `id` and gets a response with the same
//! `id`; the host also pushes `manifest/changed` unasked when a plugin file is
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
pub const PROTOCOL_VERSION: u32 = 1;

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
}

/// A message from the host that komo did not ask for.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// A plugin file changed and the host reloaded: this is the new set.
    ManifestChanged(Vec<PluginToolDef>),
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
    next_id: AtomicI64,
    outbound: mpsc::UnboundedSender<String>,
    /// The child, kept so dropping the host kills it rather than orphaning a
    /// Python process for the rest of the session.
    child: Mutex<Option<Child>>,
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
    pub async fn manifest(&self) -> Result<Vec<PluginToolDef>, PyHostError> {
        let value = self.request("manifest", serde_json::json!({})).await?;
        let manifest: Manifest = serde_json::from_value(value)
            .map_err(|error| PyHostError::Plugin(format!("malformed manifest: {error}")))?;
        if manifest.protocol != PROTOCOL_VERSION {
            return Err(PyHostError::Unavailable(format!(
                "plugin host speaks protocol {}, this komo speaks {PROTOCOL_VERSION}",
                manifest.protocol
            )));
        }
        Ok(manifest.tools)
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

        // A response carries the id komo sent; anything else is a notification.
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
                let tools = message
                    .get("params")
                    .and_then(|p| p.get("tools"))
                    .cloned()
                    .unwrap_or_default();
                match serde_json::from_value::<Vec<PluginToolDef>>(tools) {
                    Ok(tools) => {
                        let _ = events.send(HostEvent::ManifestChanged(tools));
                    }
                    Err(error) => warn!(%error, "malformed manifest from the plugin host"),
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

        let tools = host.manifest().await.unwrap();
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
        let tools = host.manifest().await.unwrap();
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
        assert!(host.manifest().await.unwrap().is_empty());

        scratch.write("late.py", GREETER);

        let event = tokio::time::timeout(std::time::Duration::from_secs(15), events.recv())
            .await
            .expect("the host should notice a new plugin file")
            .expect("the event stream should stay open");
        match event {
            HostEvent::ManifestChanged(tools) => {
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].name, "greet");
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
        assert_eq!(host.manifest().await.unwrap().len(), 1);
        assert_eq!(
            host.call("chatty", serde_json::json!({})).await.unwrap(),
            "answered"
        );
        // Still healthy after all that noise.
        assert_eq!(host.manifest().await.unwrap().len(), 1);
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
