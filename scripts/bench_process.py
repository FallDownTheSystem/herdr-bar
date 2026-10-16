#!/usr/bin/env python3
"""Time the bar as a user feels it: a real process, a real terminal, a fake Herdr.

    python3 scripts/bench_process.py                  # the numbers, for `python3 run.py`
    python3 scripts/bench_process.py --json           # machine-readable
    python3 scripts/bench_process.py --save           # write scripts/bench-process-baseline.json
    python3 scripts/bench_process.py --compare        # side by side with the saved baseline
    python3 scripts/bench_process.py --cmd ./herdr-bar  # bench another build of the plugin
    python3 scripts/bench_process.py --runs 30        # more samples, tighter medians

`bench.py` drives the `Bar` object in-process, which is the right tool for
paint efficiency but blind to everything outside it: interpreter startup,
imports, the socket round trips, the terminal handshake, and the background
watcher. Those are where most of the felt cost lives, so this bench treats the
plugin as a black box. It starts the plugin's own entry point on a
pseudo-terminal, serves it a synthetic Herdr session over a Unix socket, types
at it, and times what comes back.

Because nothing here imports the plugin, any implementation that speaks the
same protocol can be measured against the same baseline: `--cmd` names the
command that stands in for `python3 run.py`.

What is measured, per session size (the fixture, +200 rows, +2,000 rows):

* first frame -- spawn to the moment the first list row is on screen.
  The number a user feels every time they press the chord.
* keystroke -- typing and deleting a query, from writing the key to the
  last byte of the frame it produced.
* exit -- ctrl+c to process exit.
* cpu and memory -- user+system CPU and peak RSS of the whole session.

And for the background title watcher, over a fixed window with Herdr
announcing a status change every 250ms: CPU per second, peak RSS, and Herdr
calls per second.

POSIX only: pseudo-terminals and Unix sockets are what is being timed.
"""

from __future__ import annotations

import errno
import fcntl
import json
import os
import pty
import resource
import select
import shlex
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from typing import Any, Dict, List, Optional, Sequence, Tuple

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

from tests.fixtures import PREVIEW_TEXT, snapshot  # noqa: E402

BASELINE_PATH = os.path.join(ROOT, "scripts", "bench-process-baseline.json")
DEFAULT_CMD = [sys.executable, os.path.join(ROOT, "run.py")]

SIZE = (104, 22)
SIZES = (("fixture", 0), ("rows_200", 200), ("rows_2000", 2000))
QUERY = "session"
# A frame is over once the plugin has been quiet this long. Well under the
# 70ms preview debounce, so a keystroke and the preview it schedules later
# are timed as the two separate frames they are.
QUIET = 0.02
TIMEOUT = 10.0
# Any of these on screen means the list has been drawn.
FIRST_ROW_MARKERS = ("Redesign week loading screen", "cards meaningless")
APPEARANCE_QUERY = b"\x1b]11;?"
APPEARANCE_REPLY = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"


# -- a fake Herdr ------------------------------------------------------------


def bulk_snapshot(extra: int) -> Dict[str, Any]:
    """The fixture session with `extra` more agent tabs (same shape as bench.py)."""
    data = snapshot()
    for index in range(extra):
        tab_id = "w1:t%d" % (index + 100)
        pane_id = "w1:p%d" % (index + 100)
        status = "working" if index % 3 else "idle"
        title = "session %d refactor pass" % index
        data["tabs"].append({
            "tab_id": tab_id, "workspace_id": "w1", "label": title,
            "number": index + 100, "agent_status": status,
            "pane_count": 1, "focused": False,
        })
        agent = {
            "pane_id": pane_id, "tab_id": tab_id, "workspace_id": "w1",
            "agent": "claude", "agent_status": status,
            "cwd": "/Users/dev/workspace/pacebeats/module%d" % index,
            "terminal_title_stripped": title, "focused": False,
        }
        data["agents"].append(agent)
        data["panes"].append(dict(agent))
    return data


class FakeHerdr(object):
    """Serves the slice of the Herdr socket protocol the plugin uses.

    Answers are instant, so what is timed is the plugin and not a server;
    the real one adds a roughly constant round trip on top.
    """

    def __init__(self, path: str, data: Dict[str, Any], plugin_root: str) -> None:
        self.path = path
        self.data = data
        self.plugin_root = plugin_root
        self.calls: Dict[str, int] = {}
        self.event_interval: Optional[float] = None
        self._lock = threading.Lock()
        self._closing = False
        self._subscribers: List[socket.socket] = []
        self._server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._server.bind(path)
        self._server.listen(64)
        self._encoded_snapshot = self._encode({"snapshot": data})

    @staticmethod
    def _encode(result: Dict[str, Any]) -> bytes:
        return json.dumps({"id": "bar", "result": result}).encode() + b"\n"

    def start(self) -> "FakeHerdr":
        threading.Thread(target=self._accept, daemon=True).start()
        threading.Thread(target=self._announce, daemon=True).start()
        return self

    def reset_calls(self) -> None:
        with self._lock:
            self.calls = {}

    def total_calls(self) -> int:
        with self._lock:
            return sum(self.calls.values())

    def close(self) -> None:
        self._closing = True
        try:
            self._server.close()
        except OSError:
            pass
        for conn in list(self._subscribers):
            try:
                conn.close()
            except OSError:
                pass

    def _accept(self) -> None:
        while not self._closing:
            try:
                conn, _ = self._server.accept()
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn: socket.socket) -> None:
        buffered = b""
        try:
            while b"\n" not in buffered:
                chunk = conn.recv(65536)
                if not chunk:
                    conn.close()
                    return
                buffered += chunk
            request = json.loads(buffered.split(b"\n", 1)[0])
            method = request.get("method", "")
            with self._lock:
                self.calls[method] = self.calls.get(method, 0) + 1
            if method == "events.subscribe":
                conn.sendall(self._encode({}))
                self._subscribers.append(conn)
                return  # stays open; _announce writes to it
            conn.sendall(self._answer(method, request.get("params") or {}))
        except (OSError, ValueError):
            pass
        try:
            conn.close()
        except OSError:
            pass

    def _answer(self, method: str, params: Dict[str, Any]) -> bytes:
        if method == "session.snapshot":
            return self._encoded_snapshot
        if method == "pane.read":
            return self._encode({"read": {"text": PREVIEW_TEXT}})
        if method == "pane.process_info":
            # Dated by the OS like any real pane. pid 1 has been up for as long
            # as the machine, so the running times hold still during a run
            # instead of ticking a repaint into the keystroke timings.
            return self._encode({"process_info": {"shell_pid": 1}})
        if method == "plugin.list":
            return self._encode({"plugins": [{
                "plugin_id": "herdr-bar", "enabled": True,
                "plugin_root": self.plugin_root,
            }]})
        return self._encode({})

    def _announce(self) -> None:
        sequence = 0
        while not self._closing:
            interval = self.event_interval
            time.sleep(interval or 0.05)
            if not interval:
                continue
            sequence += 1
            agents = self.data.get("agents") or [{}]
            pane = agents[sequence % len(agents)].get("pane_id", "w1:p2")
            line = json.dumps({"event": "pane.agent_status_changed",
                               "data": {"pane_id": pane}}).encode() + b"\n"
            for conn in list(self._subscribers):
                try:
                    conn.sendall(line)
                except OSError:
                    self._subscribers.remove(conn)


# -- a pseudo-terminal session -----------------------------------------------


class Session(object):
    """One plugin process on a pty, with every output byte timestamped."""

    def __init__(self, argv: Sequence[str], env: Dict[str, str], size: Tuple[int, int]) -> None:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", size[1], size[0], 0, 0))
        self.master = master
        self.output = bytearray()
        self.chunks: List[Tuple[float, int]] = []  # (time, output length after it)
        self.started = time.perf_counter()
        self.proc = subprocess.Popen(
            list(argv), stdin=slave, stdout=slave, stderr=slave, env=env,
            cwd=ROOT, start_new_session=True, close_fds=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
        )
        os.close(slave)
        self.rusage: Optional[resource.struct_rusage] = None
        self.exited_at: Optional[float] = None
        self._answered = 0

    def pump(self, timeout: float) -> bool:
        """Read whatever arrives within `timeout`; False once the pty is closed."""
        try:
            readable, _, _ = select.select([self.master], [], [], max(0.0, timeout))
        except InterruptedError:
            return True
        if not readable:
            return True
        try:
            data = os.read(self.master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                return False
            raise
        if not data:
            return False
        now = time.perf_counter()
        self.output += data
        self.chunks.append((now, len(self.output)))
        # Be the terminal: answer the background color query the way most do.
        while True:
            found = self.output.find(APPEARANCE_QUERY, self._answered)
            if found < 0:
                break
            self._answered = found + len(APPEARANCE_QUERY)
            os.write(self.master, APPEARANCE_REPLY)
        return True

    def wait_for(self, markers: Sequence[bytes], since: int = 0) -> float:
        """Time at which any marker first appeared after byte `since`."""
        deadline = time.perf_counter() + TIMEOUT
        while time.perf_counter() < deadline:
            for marker in markers:
                found = self.output.find(marker, since)
                if found >= 0:
                    end = found + len(marker)
                    return next(t for t, length in self.chunks if length >= end)
            if not self.pump(0.05):
                break
        raise RuntimeError("timed out waiting for %r; got:\n%r"
                           % (markers, bytes(self.output[-400:])))

    def respond(self, timeout: float = 2.0) -> Optional[float]:
        """After input: wait for the first byte back, then for the frame to end."""
        before = len(self.output)
        deadline = time.perf_counter() + timeout
        while len(self.output) == before and time.perf_counter() < deadline:
            if not self.pump(deadline - time.perf_counter()):
                return None
        if len(self.output) == before:
            return None
        return self.settle() or self.chunks[-1][0]

    def settle(self, quiet: float = QUIET) -> Optional[float]:
        """Read until quiet; the time of the last byte, or None if nothing came."""
        last: Optional[float] = None
        while True:
            before = len(self.output)
            if not self.pump(quiet) or len(self.output) == before:
                return last
            last = self.chunks[-1][0]

    def send(self, data: bytes) -> float:
        sent = time.perf_counter()
        os.write(self.master, data)
        return sent

    def wait_exit(self) -> float:
        deadline = time.perf_counter() + TIMEOUT
        while time.perf_counter() < deadline:
            pid, status, usage = os.wait4(self.proc.pid, os.WNOHANG)
            if pid:
                self.exited_at = time.perf_counter()
                self.proc.returncode = status
                self.rusage = usage
                return self.exited_at
            self.pump(0.002)
        raise RuntimeError("plugin did not exit")

    def close(self) -> None:
        if self.proc.returncode is None:
            try:
                os.killpg(self.proc.pid, signal.SIGKILL)
            except OSError:
                pass
            try:
                os.wait4(self.proc.pid, 0)
            except ChildProcessError:
                pass
        try:
            os.close(self.master)
        except OSError:
            pass


# -- scenarios ---------------------------------------------------------------


class Workspace(object):
    """Fresh Herdr directories and a fake server for one measurement."""

    def __init__(self, data: Dict[str, Any], config: Optional[Dict[str, Any]] = None,
                 stop_watcher: bool = True) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="hbb-")
        base = self.tmp.name
        self.socket_path = os.path.join(base, "herdr.sock")
        self.config_dir = os.path.join(base, "config")
        self.state_dir = os.path.join(base, "state")
        self.home = os.path.join(base, "home")
        for path in (self.config_dir, self.state_dir, self.home):
            os.makedirs(path)
        if config is not None:
            with open(os.path.join(self.config_dir, "config.json"), "w") as handle:
                json.dump(config, handle)
        self.server = FakeHerdr(self.socket_path, data, ROOT).start()
        if stop_watcher:
            # The popup starts the title watcher on open. The bar scenarios
            # time the popup alone; the watcher has a scenario of its own.
            self.watcher_dir().mkdir(parents=True, exist_ok=True)
            (self.watcher_dir() / "title-watcher.stopped").touch()

    def watcher_dir(self):
        import hashlib
        from pathlib import Path
        key = hashlib.sha256(os.path.abspath(self.socket_path).encode()).hexdigest()[:24]
        return Path(self.state_dir) / "title-servers" / key

    def env(self) -> Dict[str, str]:
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": self.home,
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "HERDR_SOCKET_PATH": self.socket_path,
            "HERDR_PLUGIN_CONFIG_DIR": self.config_dir,
            "HERDR_PLUGIN_STATE_DIR": self.state_dir,
            # Never reach a real Herdr through the CLI fallback.
            "HERDR_BIN_PATH": "/nonexistent/herdr",
            # Keep `run.py` on the Python build even when bin/herdr-bar exists.
            "HERDR_BAR_PYTHON": "1",
        }
        return env

    def close(self) -> None:
        self.server.close()
        self.tmp.cleanup()


def bar_session(cmd: Sequence[str], data: Dict[str, Any]) -> Dict[str, Any]:
    """Open, type a query and delete it, then cancel."""
    workspace = Workspace(data)
    session = Session(cmd, workspace.env(), SIZE)
    try:
        markers = [marker.encode() for marker in FIRST_ROW_MARKERS]
        first_frame = session.wait_for(markers) - session.started
        session.settle(0.25)  # previews and running times fill in; let them
        keystrokes: List[float] = []
        for key in [c.encode() for c in QUERY] + [b"\x7f"] * len(QUERY):
            sent = session.send(key)
            last = session.respond()
            if last is not None:
                keystrokes.append(last - sent)
            session.settle(0.1)  # let the preview debounce fire before the next key
        calls = workspace.server.total_calls()
        sent = session.send(b"\x03")
        exited = session.wait_exit()
        usage = session.rusage
        return {
            "first_frame_ms": first_frame * 1000.0,
            "keystroke_ms": [value * 1000.0 for value in keystrokes],
            "exit_ms": (exited - sent) * 1000.0,
            "cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000.0 if usage else 0.0,
            "max_rss_kb": _rss_kb(usage.ru_maxrss) if usage else 0,
            "calls": calls,
            "bytes": len(session.output),
        }
    finally:
        session.close()
        workspace.close()


def watcher_session(cmd: Sequence[str], seconds: float) -> Dict[str, Any]:
    """The title watcher, alone, while Herdr reports a busy session."""
    data = snapshot()
    workspace = Workspace(data, config={"auto_titles": True, "tab_status": True},
                          stop_watcher=False)
    env = workspace.env()
    workspace.watcher_dir().mkdir(parents=True, exist_ok=True)
    proc = subprocess.Popen(
        list(cmd) + ["--watch-titles", "-"], cwd=str(workspace.watcher_dir()), env=env,
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        time.sleep(1.0)  # startup is not what this scenario is about
        workspace.server.reset_calls()
        workspace.server.event_interval = 0.25
        before = _cpu_seconds(proc.pid)
        time.sleep(seconds)
        after = _cpu_seconds(proc.pid)
        calls = workspace.server.total_calls()
        workspace.server.event_interval = None
        (workspace.watcher_dir() / "title-watcher.stopped").touch()
        _, _, usage = os.wait4(proc.pid, 0)
        proc.returncode = 0
        cpu = (after - before) if before is not None and after is not None else None
        return {
            "cpu_ms_per_s": cpu * 1000.0 / seconds if cpu is not None else None,
            "max_rss_kb": _rss_kb(usage.ru_maxrss),
            "calls_per_s": calls / seconds,
        }
    finally:
        if proc.returncode is None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
                os.wait4(proc.pid, 0)
            except (OSError, ChildProcessError):
                pass
        workspace.close()


def _rss_kb(value: int) -> int:
    return int(value / 1024) if sys.platform == "darwin" else int(value)  # bytes on macOS


def _cpu_seconds(pid: int) -> Optional[float]:
    """CPU time of a live process, from /proc or ps."""
    try:
        with open("/proc/%d/stat" % pid) as handle:
            fields = handle.read().rsplit(")", 1)[1].split()
        ticks = os.sysconf("SC_CLK_TCK")
        return (int(fields[11]) + int(fields[12])) / float(ticks)
    except (OSError, IndexError, ValueError):
        pass
    try:
        out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)],
                             stdout=subprocess.PIPE, check=False).stdout.decode().strip()
    except OSError:
        return None
    if not out:
        return None
    seconds = 0.0
    for part in out.replace("-", ":").split(":"):
        seconds = seconds * 60 + float(part)
    return seconds


# -- statistics and reporting ------------------------------------------------


def percentile(values: Sequence[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(round(fraction * (len(ordered) - 1))))
    return ordered[index]


def run_all(cmd: Sequence[str], runs: int, watcher_seconds: float) -> Dict[str, Any]:
    results: Dict[str, Any] = {}
    for name, extra in SIZES:
        data = bulk_snapshot(extra)
        bar_session(cmd, data)  # warm the page cache and any bytecode caches
        sessions = [bar_session(cmd, data) for _ in range(runs)]
        keys = [ms for s in sessions for ms in s["keystroke_ms"]]
        first = [s["first_frame_ms"] for s in sessions]
        results[name] = {
            "rows": len(data["tabs"]) + len(data["agents"]),
            "first_frame_p50_ms": round(percentile(first, 0.5), 2),
            "first_frame_p90_ms": round(percentile(first, 0.9), 2),
            "keystroke_p50_ms": round(percentile(keys, 0.5), 3),
            "keystroke_p99_ms": round(percentile(keys, 0.99), 3),
            "exit_p50_ms": round(percentile([s["exit_ms"] for s in sessions], 0.5), 2),
            "cpu_p50_ms": round(percentile([s["cpu_ms"] for s in sessions], 0.5), 1),
            "max_rss_p50_kb": int(percentile([s["max_rss_kb"] for s in sessions], 0.5)),
            "herdr_calls_p50": int(percentile([s["calls"] for s in sessions], 0.5)),
        }
    watcher = watcher_session(cmd, watcher_seconds)
    results["watcher"] = {
        key: (round(value, 2) if isinstance(value, float) else value)
        for key, value in watcher.items()
    }
    return results


def machine() -> Dict[str, Any]:
    uname = os.uname()
    return {
        "platform": sys.platform,
        "machine": uname.machine,
        "cpus": os.cpu_count(),
        "bench_python": sys.version.split()[0],
    }


ROWS = (
    ("first_frame_p50_ms", "first frame p50", "ms"),
    ("first_frame_p90_ms", "first frame p90", "ms"),
    ("keystroke_p50_ms", "keystroke p50", "ms"),
    ("keystroke_p99_ms", "keystroke p99", "ms"),
    ("exit_p50_ms", "exit p50", "ms"),
    ("cpu_p50_ms", "cpu per session", "ms"),
    ("max_rss_p50_kb", "peak memory", "KB"),
    ("herdr_calls_p50", "herdr calls", ""),
)
WATCHER_ROWS = (
    ("cpu_ms_per_s", "watcher cpu", "ms/s"),
    ("max_rss_kb", "watcher peak memory", "KB"),
    ("calls_per_s", "watcher herdr calls", "/s"),
)


def _fmt(value: Any, unit: str) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        text = "%.2f" % value if value < 100 else "%.0f" % value
    else:
        text = str(value)
    return (text + " " + unit).strip()


def report(data: Dict[str, Any], baseline: Optional[Dict[str, Any]] = None) -> str:
    results = data["results"]
    names = [name for name, _ in SIZES]
    lines = ["", "  herdr-bar process bench   %s" % " ".join(data["cmd"]), ""]
    header = "  %-22s" % "" + "".join("%16s" % "%s (%d)" % (n, results[n]["rows"]) for n in names)
    lines.append(header)
    lines.append("  " + "-" * (len(header) - 2))
    for key, label, unit in ROWS:
        cells = []
        for name in names:
            cell = _fmt(results[name][key], unit)
            if baseline:
                was = baseline["results"].get(name, {}).get(key)
                if was:
                    cell += " %s" % _delta(results[name][key], was)
            cells.append("%16s" % cell if not baseline else "%24s" % cell)
        lines.append("  %-22s%s" % (label, "".join(cells)))
    lines.append("")
    for key, label, unit in WATCHER_ROWS:
        value = results["watcher"].get(key)
        cell = _fmt(value, unit)
        if baseline:
            was = baseline["results"].get("watcher", {}).get(key)
            if was and value is not None:
                cell += " %s" % _delta(value, was)
        lines.append("  %-22s%16s" % (label, cell))
    lines.append("")
    return "\n".join(lines) + "\n"


def _delta(now: float, was: float) -> str:
    if not was:
        return ""
    if now <= was:
        return "(%.1fx)" % (was / now) if now else "(inf)"
    return "(+%.0f%%)" % (100.0 * (now - was) / was)


def main(argv: Sequence[str]) -> int:
    args = list(argv)
    cmd = DEFAULT_CMD
    runs = 15
    watcher_seconds = 10.0
    if "--cmd" in args:
        cmd = shlex.split(args[args.index("--cmd") + 1])
    if "--runs" in args:
        runs = int(args[args.index("--runs") + 1])
    if "--watcher-seconds" in args:
        watcher_seconds = float(args[args.index("--watcher-seconds") + 1])

    data = {
        "cmd": [os.path.relpath(part, ROOT) if os.path.isabs(part) and part.startswith(ROOT)
                else part for part in cmd],
        "runs": runs,
        "machine": machine(),
        "results": run_all(cmd, runs, watcher_seconds),
    }
    if data["cmd"] and data["cmd"][0] == sys.executable:
        data["cmd"][0] = "python3"
        data["machine"]["plugin_python"] = sys.version.split()[0]

    if "--json" in args:
        sys.stdout.write(json.dumps(data, indent=2, sort_keys=True) + "\n")
        return 0
    baseline = None
    if "--compare" in args and os.path.exists(BASELINE_PATH):
        with open(BASELINE_PATH, encoding="utf-8") as handle:
            baseline = json.load(handle)
    sys.stdout.write(report(data, baseline))
    if "--save" in args:
        with open(BASELINE_PATH, "w", encoding="utf-8") as handle:
            json.dump(data, handle, indent=2, sort_keys=True)
            handle.write("\n")
        sys.stdout.write("  saved %s\n\n" % os.path.relpath(BASELINE_PATH, ROOT))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
