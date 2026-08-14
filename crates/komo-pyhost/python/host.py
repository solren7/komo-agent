"""The plugin host komo spawns: load `~/.komo/plugins/*.py`, answer calls.

Speaks newline-delimited JSON on stdin/stdout — one object per line, requests
in, responses out, plus unsolicited `manifest/changed` when a plugin file is
edited. stdout is the protocol and nothing else: a plugin's own `print` would
corrupt the stream, so stdout is swapped for stderr for the duration of every
import and every call.

Everything is stdlib-only, and the process is deliberately dumb: it holds no
state komo cannot rebuild by asking for the manifest again. If it dies, komo
restarts it and re-mounts what it reports — which is why a crash here costs the
plugins for a moment and nothing else.
"""

import contextlib
import importlib.util
import json
import sys
import threading
import time
import traceback
from pathlib import Path

import komo_plugin

PROTOCOL_VERSION = 1

# How often to re-stat the plugin directory. A plugin appearing is the agent
# having just written a file, so this wants to be quick; stat-ing a handful of
# files is nothing.
WATCH_INTERVAL_SECONDS = 1.0

_write_lock = threading.Lock()


def send(message):
    """Write one protocol message. Locked: the watcher thread also writes."""
    line = json.dumps(message, ensure_ascii=False)
    with _write_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def log(level, message):
    send({"method": "log", "params": {"level": level, "message": message}})


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
            send({"method": "manifest/changed", "params": {"tools": tools}})
        except Exception:
            log("warn", f"plugin reload failed:\n{traceback.format_exc()}")


def handle(request, plugins: Plugins):
    """Answer one request. Returns the response body, or None for a notification."""
    method = request.get("method")
    params = request.get("params") or {}

    if method == "manifest":
        return {"protocol": PROTOCOL_VERSION, "tools": plugins.load()}

    if method == "call":
        name = params.get("name", "")
        args = params.get("args") or {}
        with stdout_to_stderr():
            return {"content": komo_plugin.call(name, args)}

    raise ValueError(f"unknown method `{method}`")


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
            request = json.loads(line)
        except ValueError:
            log("warn", f"ignoring unparseable line: {line[:200]}")
            continue

        request_id = request.get("id")
        if request.get("method") == "shutdown":
            return
        try:
            result = handle(request, plugins)
            if request_id is not None:
                send({"id": request_id, "result": result})
        except Exception as error:
            # Every failure answers the request rather than dying: komo is
            # waiting on this id, and a tool that raises is a result the model
            # can work with, not a reason to lose the host.
            if request_id is not None:
                send({"id": request_id, "error": {"message": f"{error}"}})
            else:
                log("warn", f"{error}")


if __name__ == "__main__":
    main()
