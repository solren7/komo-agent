"""The plugin host komo spawns: load `~/.komo/plugins/*.py`, answer calls, and
run the programs komo's `run_code` tool sends.

Speaks newline-delimited JSON on stdin/stdout — one object per line. Requests
travel both ways: komo asks for the manifest, a plugin call, or a program run;
the host asks komo to run one of *its* tools on behalf of a running program.
The two id spaces cannot collide because komo's requests carry positive ids and
the host's carry negative ones.

stdout is the protocol and nothing else: a plugin's own `print` would corrupt
the stream, so stdout is swapped for stderr around every import and every call.

The main loop only reads and routes — every request is handled on its own
thread. That is what makes a program calling back into komo possible at all: the
program blocks waiting for its answer while the loop stays free to deliver it.
"""

import contextlib
import importlib.util
import io
import json
import queue
import sys
import threading
import time
import traceback
from pathlib import Path

import komo_plugin

PROTOCOL_VERSION = 3

# How often to re-stat the plugin directory. A plugin appearing is the agent
# having just written a file, so this wants to be quick; stat-ing a handful of
# files is nothing.
WATCH_INTERVAL_SECONDS = 1.0

# The real stdout, captured before anything can replace it.
#
# `send` must never go through `sys.stdout`: a running program has that swapped
# for a buffer so its `print`s can be captured, and a protocol message written
# into that buffer is a message that never leaves the process — both sides then
# wait forever. This is the one handle the protocol writes to.
_PROTOCOL_OUT = sys.stdout

_write_lock = threading.Lock()

# Host-originated requests: id → where its answer goes. Negative ids, allocated
# under the same lock that hands them out.
_pending_lock = threading.Lock()
_pending = {}
_next_id = 0


def send(message):
    """Write one protocol message. Locked: many threads write."""
    line = json.dumps(message, ensure_ascii=False)
    with _write_lock:
        _PROTOCOL_OUT.write(line + "\n")
        _PROTOCOL_OUT.flush()


def log(level, message):
    send({"method": "log", "params": {"level": level, "message": message}})


def ask_komo(method, params):
    """Send a request *to* komo and block this thread until it answers.

    Safe to call from a program thread precisely because the main loop is not
    the one blocking — it stays free to read the response and hand it over.
    """
    global _next_id
    answer = queue.Queue(maxsize=1)
    with _pending_lock:
        _next_id -= 1
        request_id = _next_id
        _pending[request_id] = answer
    send({"id": request_id, "method": method, "params": params})
    message = answer.get()
    if "error" in message:
        raise RuntimeError(message["error"].get("message", "komo rejected the request"))
    return message.get("result") or {}


@contextlib.contextmanager
def stdout_to_stderr():
    """Keep plugin output off the protocol stream.

    A `print` in plugin code is a debugging habit, not a bug — but on stdout it
    would be read as a protocol message and desynchronize the connection. It
    goes to stderr instead, where komo's log picks it up.
    """
    real = sys.stdout
    sys.stdout = sys.stderr
    try:
        yield
    finally:
        sys.stdout = real


class ToolError(Exception):
    """A komo tool call that failed, raised inside a running program.

    Carries the tool's own message, which is what the model needs to see: a
    program that catches this can recover, and one that does not gets the text
    in its failure.
    """

    def __init__(self, tool, message):
        super().__init__(f"{tool}: {message}")
        self.tool = tool
        self.message = message


class Text(str):
    """What a tool call returns: the text, with the same result as data beside it.

    A tool's text is laid out to be *read* — `read` returns a header line and
    `N│` gutters, `grep` returns indented `Line N:` entries — so a program that
    computes on it ends up re-parsing a display page, and gets it wrong. This is
    that text (it is a `str`, so every existing use keeps working) with the
    tool's structured view on `.structured`, or `None` when the tool reports
    none.
    """

    __slots__ = ("structured",)

    def __new__(cls, content, structured=None):
        text = super().__new__(cls, content)
        text.structured = structured
        return text


class Tools:
    """The `tools` object a program calls komo's own tools through.

    Every attribute is a callable that dispatches back to komo, so `tools`
    mirrors whatever the calling turn was offered without this file knowing a
    single tool name.
    """

    def __init__(self, run_id):
        self._run_id = run_id

    def __getattr__(self, name):
        def call(*args, **kwargs):
            if args:
                # komo dispatches by argument *name*; there is no authored
                # parameter order to bind a positional to, and guessing one
                # would bind the wrong value silently. Say so instead — the
                # signature the caller was shown already names every argument.
                raise TypeError(
                    f"tools.{name}() takes keyword arguments only — name each "
                    f"argument, as in the signature you were given"
                )
            result = ask_komo(
                "tool/call",
                {"run": self._run_id, "name": name, "args": kwargs},
            )
            if result.get("is_error"):
                raise ToolError(name, result.get("content", ""))
            return Text(result.get("content", ""), result.get("structured"))

        call.__name__ = name
        return call

    # Subscript access for a name that is not a legal attribute (a mounted
    # plugin tool is `py__x`, which is fine, but an MCP tool is
    # `mcp__server__tool` — legal too, and this covers whatever is not).
    def __getitem__(self, name):
        return getattr(self, name)


class Plugins:
    """The loaded plugin set, and the mtimes that decide when to reload."""

    def __init__(self, directory: Path):
        self.directory = directory
        self.signature = None

    def scan(self):
        """(path, mtime, size) for every plugin file, sorted.

        Size rides along with mtime because an editor that writes twice within
        one filesystem timestamp tick would otherwise look unchanged.
        """
        if not self.directory.is_dir():
            return []
        found = []
        for path in sorted(self.directory.glob("*.py")):
            # `_`-prefixed files are shared helpers a plugin imports, not
            # plugins themselves — the same convention komo's skills use for
            # non-skill files in a skills directory.
            if path.name.startswith("_"):
                continue
            try:
                stat = path.stat()
            except OSError:
                continue
            found.append((str(path), stat.st_mtime, stat.st_size))
        return found

    def load(self):
        """Import every plugin file, replacing whatever was registered before.

        A file that raises is reported and skipped: one broken plugin must not
        cost the others, and the traceback is what the agent needs to fix it.
        """
        signature = self.scan()
        self.signature = signature
        komo_plugin.clear()
        # The plugin directory goes on the path so a plugin can import its own
        # `_helpers.py` next to it.
        directory = str(self.directory)
        if directory not in sys.path:
            sys.path.insert(0, directory)

        for path, _mtime, _size in signature:
            name = Path(path).stem
            try:
                spec = importlib.util.spec_from_file_location(f"komo_plugin_{name}", path)
                if spec is None or spec.loader is None:
                    raise ImportError(f"cannot load {path}")
                module = importlib.util.module_from_spec(spec)
                with stdout_to_stderr():
                    spec.loader.exec_module(module)
            except Exception:
                log("warn", f"plugin `{name}` failed to load:\n{traceback.format_exc()}")

        return komo_plugin.registered_tools()

    def changed(self):
        return self.scan() != self.signature


def watch(plugins: Plugins):
    """Reload on change and push the new manifest. Runs until the process exits."""
    while True:
        time.sleep(WATCH_INTERVAL_SECONDS)
        try:
            if not plugins.changed():
                continue
            tools = plugins.load()
            send({
                "method": "manifest/changed",
                "params": {"tools": tools, "hooks": komo_plugin.registered_hooks()},
            })
        except Exception:
            log("warn", f"plugin reload failed:\n{traceback.format_exc()}")


# Filename a `run_code` program compiles under. Used to tell the program's own
# frames apart from this file's when a traceback is built.
PROGRAM_FILENAME = "<run_code>"


def run_program(source, run_id):
    """Execute one `run_code` program and report what it printed and returned.

    The program body is wrapped in a function so that `return` works at what
    looks like top level — the whole point of the mode is that a program reads
    like a script, and a bare `return` is the natural way to answer.

    Its `print`s are captured rather than redirected to stderr: they *are* the
    output the model asked for, and returning them is how a program reports
    intermediate work without stuffing everything into one return value.
    """
    printed = io.StringIO()
    body = "\n".join("    " + line for line in source.splitlines()) or "    pass"
    program = f"def __komo_program__(tools):\n{body}\n"

    namespace = {"ToolError": ToolError}
    real_stdout = sys.stdout
    sys.stdout = printed
    try:
        exec(compile(program, PROGRAM_FILENAME, "exec"), namespace)  # noqa: S102
        result = namespace["__komo_program__"](Tools(run_id))
    finally:
        sys.stdout = real_stdout

    return {
        "logs": printed.getvalue(),
        # `None` and "returned nothing" are the same thing here; komo renders
        # the absence rather than the word "None".
        "result": None if result is None else _renderable(result),
    }


def _renderable(value):
    """Make a program's return value something komo can put in a message."""
    if isinstance(value, (str, int, float, bool)):
        return value
    try:
        json.dumps(value)
        return value
    except (TypeError, ValueError):
        return repr(value)


def handle(request, plugins: Plugins):
    """Answer one komo request. Runs on its own thread."""
    method = request.get("method")
    params = request.get("params") or {}

    if method == "manifest":
        tools = plugins.load()
        # The hook points anything registered for. komo skips calling a host
        # that lists none, so a deployment with no hooks pays nothing per round.
        return {
            "protocol": PROTOCOL_VERSION,
            "tools": tools,
            "hooks": komo_plugin.registered_hooks(),
        }

    if method == "call":
        name = params.get("name", "")
        args = params.get("args") or {}
        with stdout_to_stderr():
            return {"content": komo_plugin.call(name, args)}

    if method == "hook":
        point = params.get("point", "")
        payload = params.get("payload") or {}
        with stdout_to_stderr():
            return komo_plugin.run_hook(point, payload, lambda text: log("warn", text))

    if method == "run_code":
        return run_program(params.get("source", ""), request.get("id"))

    raise ValueError(f"unknown method `{method}`")


def program_traceback():
    """The current exception's traceback, starting at the program's own code.

    Everything above the `<run_code>` frame is this file dispatching — frames
    the program's author cannot act on, and three of the four lines in a typical
    failure. Dropping them leaves the line that raised and the error itself.
    A failure with no program frame at all (a syntax error, which never gets
    that far) formats to the exception alone, which already carries the offending
    source line.
    """
    kind, error, tb = sys.exc_info()
    while tb is not None and tb.tb_frame.f_code.co_filename != PROGRAM_FILENAME:
        tb = tb.tb_next
    return "".join(traceback.format_exception(kind, error, tb))


def serve(request, plugins: Plugins):
    """Handle one request and answer it. Every failure answers rather than dies.

    komo is waiting on this id: a program that raised is a result the model can
    work with, not a reason to lose the host.
    """
    request_id = request.get("id")
    try:
        result = handle(request, plugins)
        if request_id is not None:
            send({"id": request_id, "result": result})
    except Exception as error:
        detail = f"{error}"
        if request.get("method") == "run_code":
            # A program's traceback is the thing the model rewrites from, so it
            # travels rather than just the exception's own line.
            detail = program_traceback()
        if request_id is not None:
            send({"id": request_id, "error": {"message": detail}})
        else:
            log("warn", detail)


def main():
    directory = Path(sys.argv[1]) if len(sys.argv) > 1 else Path.cwd()
    plugins = Plugins(directory)
    plugins.load()

    threading.Thread(target=watch, args=(plugins,), daemon=True).start()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except ValueError:
            log("warn", f"ignoring unparseable line: {line[:200]}")
            continue

        request_id = message.get("id")
        # A negative id is komo answering something this host asked — hand it to
        # the thread that is blocked waiting for it.
        if request_id is not None and request_id < 0:
            with _pending_lock:
                waiter = _pending.pop(request_id, None)
            if waiter is not None:
                waiter.put(message)
            continue

        if message.get("method") == "shutdown":
            return
        # Each request on its own thread: a plugin call or a program can take as
        # long as it takes without stopping this loop from delivering the
        # answers that program is waiting on.
        threading.Thread(target=serve, args=(message, plugins), daemon=True).start()


if __name__ == "__main__":
    main()
