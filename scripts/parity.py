#!/usr/bin/env python3
"""Hold two builds of the plugin side by side and show where they differ.

    python3 scripts/parity.py --rust target/release/herdr-bar    # Python vs Rust, live
    python3 scripts/parity.py --rust ... --only bar-rename         # one scenario
    python3 scripts/parity.py --save tests/parity/golden.json     # record a reference build
    python3 scripts/parity.py --check tests/parity/golden.json --rust target/release/herdr-bar

The Rust port has to be a drop-in replacement: same screens, same Herdr
calls, same files on disk, same output. This harness drives each build as a
black box, the way Herdr does -- a real process on a pseudo-terminal, a fake
Herdr on a Unix socket, fake `ps` and `fc-match` so every run sees the same
machine -- and records a transcript per scenario:

* the screen after every step, cell by cell with its rendition,
* every Herdr request that changes something, with its parameters,
* the files the plugin left in its state directory,
* exit status and whatever it printed outside the alt screen.

Two transcripts that are equal mean the user could not tell the builds apart.
POSIX only, like the process bench it borrows its fake Herdr and pty from.
"""

from __future__ import annotations

import copy
import difflib
import hashlib
import json
import os
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unicodedata
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "scripts"))

from bench_process import FakeHerdr, Session, bulk_snapshot  # noqa: E402

from tests.fixtures import PREVIEW_TEXT, snapshot  # noqa: E402

PYTHON_CMD = [sys.executable, os.path.join(ROOT, "run.py")]
SETTLE = 0.35
EXIT_TIMEOUT = 10.0

# Every pane gets a shell pid, and the fake `ps` dates each one. Times sit
# half a minute past a boundary, so no shown running time can tick over
# during a scenario.
PANE_PIDS = {
    "w1:p1": 4101, "w1:p2": 4102, "w1:p3": 4103, "w1:p4": 4104, "w1:p5": 4105,
    "w1:p6": 4106, "w1:p7": 4107, "w2:p1": 4108, "w2:p2": 4109,
}
ETIMES = {
    4101: "4-03:00:30", 4102: "12:30", 4103: "02:04:30", 4104: "03:41:30",
    4105: "1-02:00:30", 4106: "26:30", 4107: "55:30", 4108: "08:30",
}
# Requests that only read. Their timing follows the clock, not the user, so
# they are compared as a set of distinct parameters rather than a sequence.
READS = ("session.snapshot", "pane.read", "pane.process_info", "plugin.list", "events.subscribe")


# -- the machine both builds see ---------------------------------------------


def fake_tools(directory: str) -> str:
    """A PATH entry with a `ps` that dates the panes and a silent `fc-match`."""
    bin_dir = os.path.join(directory, "bin")
    os.makedirs(bin_dir, exist_ok=True)
    table = " ".join("%d) echo '%s' ;;" % (pid, etime) for pid, etime in ETIMES.items())
    scripts = {
        "ps": "#!/bin/sh\nfor last; do :; done\ncase \"$last\" in %s *) exit 1 ;; esac\n" % table,
        "fc-match": "#!/bin/sh\nexit 1\n",
    }
    for name, body in scripts.items():
        path = os.path.join(bin_dir, name)
        with open(path, "w") as handle:
            handle.write(body)
        os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return bin_dir


class RecordingHerdr(FakeHerdr):
    """The bench's fake Herdr, plus a log of requests and a mutable session."""

    def __init__(self, path: str, data: Dict[str, Any], plugin_root: str,
                 plugins: Optional[List[Dict[str, Any]]] = None) -> None:
        super().__init__(path, data, plugin_root)
        self.log: List[Tuple[str, Any]] = []
        self.plugins = plugins

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
            params = request.get("params") or {}
            with self._lock:
                self.calls[method] = self.calls.get(method, 0) + 1
                self.log.append((method, params))
            if method == "events.subscribe":
                conn.sendall(self._encode({}))
                self._subscribers.append(conn)
                return
            with self._lock:
                answer = self._answer(method, params)
            conn.sendall(answer)
        except (OSError, ValueError):
            pass
        try:
            conn.close()
        except OSError:
            pass

    def _answer(self, method: str, params: Dict[str, Any]) -> bytes:
        data = self.data
        if method == "session.snapshot":
            return self._encode({"snapshot": data})
        if method == "pane.read":
            return self._encode({"read": {"text": PREVIEW_TEXT}})
        if method == "pane.process_info":
            pid = PANE_PIDS.get(params.get("pane_id"))
            return self._encode({"process_info": {"shell_pid": pid} if pid else {}})
        if method == "plugin.list":
            plugins = self.plugins if self.plugins is not None else [{
                "plugin_id": "herdr-bar", "enabled": True, "plugin_root": self.plugin_root,
            }]
            return self._encode({"plugins": plugins})
        if method == "tab.rename":
            for tab in data.get("tabs", []):
                if tab.get("tab_id") == params.get("tab_id"):
                    tab["label"] = params.get("label")
            return self._encode({})
        if method == "tab.close":
            tab_id = params.get("tab_id")
            for key in ("tabs", "agents", "panes"):
                data[key] = [entry for entry in data.get(key, []) if entry.get("tab_id") != tab_id]
            return self._encode({})
        return self._encode({})

    def changes(self) -> List[Any]:
        with self._lock:
            return [[method, params] for method, params in self.log if method not in READS]

    def reads(self) -> List[Any]:
        with self._lock:
            seen = sorted({json.dumps([m, p], sort_keys=True) for m, p in self.log
                           if m in ("pane.read", "pane.process_info")})
        return [json.loads(entry) for entry in seen]


class Machine(object):
    """Fresh Herdr directories, fake tools, and a recording server."""

    def __init__(self, data: Dict[str, Any], config: Optional[Dict[str, Any]] = None,
                 plugins: Optional[List[Dict[str, Any]]] = None, socket_up: bool = True,
                 files: Optional[Dict[str, Any]] = None, watcher_stopped: bool = True) -> None:
        self.tmp = tempfile.mkdtemp(prefix="hbp-")
        base = self.tmp
        self.socket_path = os.path.join(base, "herdr.sock")
        self.config_dir = os.path.join(base, "config")
        self.state_dir = os.path.join(base, "state")
        self.home = os.path.join(base, "home")
        for path in (self.config_dir, self.state_dir, self.home):
            os.makedirs(path)
        self.bin_dir = fake_tools(base)
        if config is not None:
            with open(os.path.join(self.config_dir, "config.json"), "w") as handle:
                handle.write(config if isinstance(config, str) else json.dumps(config))
        self.server: Optional[RecordingHerdr] = None
        if socket_up:
            self.server = RecordingHerdr(self.socket_path, data, ROOT, plugins).start()
        if watcher_stopped:
            self.watcher_dir().mkdir(parents=True, exist_ok=True)
            (self.watcher_dir() / "title-watcher.stopped").touch()
        for relative, content in (files or {}).items():
            relative = relative.replace("{watcher}", str(self.watcher_dir()))
            path = os.path.join(self.state_dir, relative)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as handle:
                handle.write(content if isinstance(content, str) else json.dumps(content))

    def watcher_dir(self):
        from pathlib import Path
        key = hashlib.sha256(os.path.abspath(self.socket_path).encode()).hexdigest()[:24]
        return Path(self.state_dir) / "title-servers" / key

    def env(self) -> Dict[str, str]:
        return {
            "PATH": os.pathsep.join([self.bin_dir, os.environ.get("PATH", "/usr/bin:/bin")]),
            "HOME": self.home,
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "HERDR_SOCKET_PATH": self.socket_path,
            "HERDR_PLUGIN_CONFIG_DIR": self.config_dir,
            "HERDR_PLUGIN_STATE_DIR": self.state_dir,
            "HERDR_BIN_PATH": "/nonexistent/herdr",
            # Keep `run.py` on the Python build even when bin/herdr-bar exists.
            "HERDR_BAR_PYTHON": "1",
        }

    def state_files(self) -> Dict[str, Any]:
        """Every file under the state directory, with volatile parts masked."""
        out: Dict[str, Any] = {}
        for directory, _, names in os.walk(self.state_dir):
            for name in sorted(names):
                path = os.path.join(directory, name)
                relative = os.path.relpath(path, self.state_dir)
                watcher = os.path.relpath(str(self.watcher_dir()), self.state_dir)
                relative = relative.replace(watcher, "{watcher}")
                with open(path, "rb") as handle:
                    raw = handle.read()
                text = raw.decode("utf-8", "replace")
                if name == "recent.json":
                    try:
                        parsed = json.loads(text)
                        for entry in parsed.get("entries", []):
                            entry["ts"] = "<ts>"
                        text = json.dumps(parsed)
                    except ValueError:
                        pass
                out[relative] = text
        return dict(sorted(out.items()))

    def close(self) -> None:
        if self.server is not None:
            self.server.close()
        shutil.rmtree(self.tmp, ignore_errors=True)


# -- a screen ----------------------------------------------------------------


def char_width(char: str) -> int:
    if unicodedata.combining(char):
        return 0
    code = ord(char)
    if code < 32 or code == 127:
        return 0
    return 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1


DEFAULT_STYLE = (None, None, False, False, False)


def sgr(params: str, style: Tuple) -> Tuple:
    fg, bg, bold, underline, reverse = style
    parts = params.split(";") if params else ["0"]
    index = 0
    while index < len(parts):
        part = parts[index] or "0"
        if part in ("38", "48"):
            mode = parts[index + 1] if index + 1 < len(parts) else ""
            length = 3 if mode == "2" else 1
            value = ";".join(parts[index: index + 2 + length])
            if part == "38":
                fg = value
            else:
                bg = value
            index += 2 + length
            continue
        if part == "0":
            fg = bg = None
            bold = underline = reverse = False
        elif part == "1":
            bold = True
        elif part == "4":
            underline = True
        elif part == "7":
            reverse = True
        elif part == "22":
            bold = False
        elif part == "24":
            underline = False
        elif part == "27":
            reverse = False
        elif part == "39":
            fg = None
        elif part == "49":
            bg = None
        index += 1
    return (fg, bg, bold, underline, reverse)


class Screen(object):
    """The alt screen, and what was printed on the main one."""

    def __init__(self, width: int, height: int) -> None:
        self.width = width
        self.height = height
        self.cells = [[(" ", DEFAULT_STYLE)] * width for _ in range(height)]
        self.x = self.y = 0
        self.style = DEFAULT_STYLE
        self.alt = False
        self.main_text = ""
        self.modes: List[str] = []

    def feed(self, data: str) -> None:
        index, length = 0, len(data)
        while index < length:
            char = data[index]
            if char != "\x1b":
                if not self.alt:
                    self.main_text += char
                elif char == "\n":
                    self.y += 1
                    self.x = 0
                elif char == "\r":
                    self.x = 0
                else:
                    self._put(char)
                index += 1
                continue
            if index + 1 >= length:
                break
            introducer = data[index + 1]
            if introducer == "[":
                end = index + 2
                while end < length and not ("\x40" <= data[end] <= "\x7e"):
                    end += 1
                if end >= length:
                    break
                self._csi(data[index + 2: end], data[end])
                index = end + 1
                continue
            if introducer == "]":
                bel = data.find("\x07", index)
                st = data.find("\x1b\\", index)
                ends = [e for e in ((bel, 1), (st, 2)) if e[0] >= 0]
                if not ends:
                    break
                end, width = min(ends)
                self.modes.append("osc:" + data[index + 2: end])
                index = end + width
                continue
            index += 2

    def _put(self, char: str) -> None:
        width = char_width(char)
        if width == 0 or self.y >= self.height or self.x >= self.width:
            return
        self.cells[self.y][self.x] = (char, self.style)
        if width == 2 and self.x + 1 < self.width:
            self.cells[self.y][self.x + 1] = ("", self.style)
        self.x += width

    def _clear(self, y: int, start: int, stop: int) -> None:
        if 0 <= y < self.height:
            row = self.cells[y]
            for x in range(max(0, start), min(self.width, stop)):
                row[x] = (" ", DEFAULT_STYLE)

    def _csi(self, params: str, final: str) -> None:
        if params.startswith("?") and final in ("h", "l"):
            self.modes.append(params + final)
            if params == "?1049":
                self.alt = final == "h"
                if self.alt:
                    for y in range(self.height):
                        self._clear(y, 0, self.width)
            return
        numbers = [int(part) if part.isdigit() else 0 for part in params.split(";")]
        if final == "m":
            self.style = sgr(params, self.style)
        elif final in ("H", "f"):
            self.y = max(0, (numbers[0] if numbers and numbers[0] else 1) - 1)
            self.x = max(0, (numbers[1] if len(numbers) > 1 and numbers[1] else 1) - 1)
        elif final == "K":
            mode = numbers[0] if numbers else 0
            if mode == 0:
                self._clear(self.y, self.x, self.width)
            elif mode == 1:
                self._clear(self.y, 0, self.x + 1)
            else:
                self._clear(self.y, 0, self.width)
        elif final == "J":
            mode = numbers[0] if numbers else 0
            if mode == 0:
                self._clear(self.y, self.x, self.width)
                for y in range(self.y + 1, self.height):
                    self._clear(y, 0, self.width)
            else:
                for y in range(self.height):
                    self._clear(y, 0, self.width)
        else:
            self.modes.append("csi:" + params + final)

    def dump(self) -> List[str]:
        """Each row as its text, then its style runs."""
        out = []
        for row in self.cells:
            text = "".join(char for char, _ in row)
            runs = []
            start = 0
            for x in range(1, len(row) + 1):
                if x == len(row) or row[x][1] != row[start][1]:
                    style = row[start][1]
                    if style != DEFAULT_STYLE:
                        runs.append("%d-%d:%s" % (start, x, json.dumps(style)))
                    start = x
            out.append(text.rstrip() + ("  ‖ " + " ".join(runs) if runs else ""))
        return out


# -- running a scenario ------------------------------------------------------


Step = Tuple[str, Any]  # ("keys", b"..."), ("wait", seconds)


def reap(session: Session) -> bool:
    """Whether the plugin has exited, collecting its status once it has."""
    if session.exited_at is not None:
        return True
    pid, status, usage = os.wait4(session.proc.pid, os.WNOHANG)
    if not pid:
        return False
    session.exited_at = time.perf_counter()
    session.proc.returncode = status
    session.rusage = usage
    return True


def wait_reaped(session: Session, timeout: float) -> bool:
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        if reap(session):
            return True
        session.pump(0.01)
    return reap(session)


def drain(session: Session, quiet: float = SETTLE) -> None:
    while True:
        before = len(session.output)
        if not session.pump(quiet) or len(session.output) == before:
            return


def run_bar(cmd: Sequence[str], scenario: Dict[str, Any]) -> Dict[str, Any]:
    data = copy.deepcopy(scenario.get("data") or snapshot())
    size = scenario.get("size", (104, 22))
    machine = Machine(data, scenario.get("config"), files=scenario.get("files"),
                      socket_up=scenario.get("socket_up", True))
    env = machine.env()
    env.update(scenario.get("env", {}))
    session = Session(list(cmd) + list(scenario.get("args", [])), env, size)
    frames: List[Any] = []
    screen = Screen(*size)
    fed = 0
    try:
        drain(session, 0.6)
        for kind, value in [("look", None)] + list(scenario.get("steps", [])):
            if kind == "keys":
                session.send(value)
                drain(session)
            elif kind == "wait":
                time.sleep(value)
                drain(session)
            text = session.output[fed:].decode("utf-8", "replace")
            fed = len(session.output)
            screen.feed(text)
            frames.append({"after": repr(value) if kind != "look" else "open",
                           "screen": screen.dump()})
            if reap(session):
                break
        if wait_reaped(session, EXIT_TIMEOUT):
            status = session.proc.returncode
        else:
            session.send(b"\x03")
            if wait_reaped(session, EXIT_TIMEOUT):
                status = "did not exit; ctrl+c -> %s" % session.proc.returncode
            else:
                status = "hung"
        drain(session, 0.1)
        screen.feed(session.output[fed:].decode("utf-8", "replace"))
        exit_code = os.WEXITSTATUS(status) if isinstance(status, int) else status
        return {
            "frames": frames,
            "exit": exit_code,
            "printed": screen.main_text,
            "modes": screen.modes,
            "changes": machine.server.changes() if machine.server else [],
            "reads": machine.server.reads() if machine.server else [],
            "files": machine.state_files(),
        }
    finally:
        session.close()
        machine.close()


def run_cli(cmd: Sequence[str], scenario: Dict[str, Any]) -> Dict[str, Any]:
    data = copy.deepcopy(scenario.get("data") or snapshot())
    machine = Machine(data, scenario.get("config"), files=scenario.get("files"),
                      socket_up=scenario.get("socket_up", True))
    env = machine.env()
    env.update(scenario.get("env", {}))
    try:
        proc = subprocess.run(list(cmd) + list(scenario["args"]), env=env, cwd=ROOT,
                              stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, timeout=30)
        stdout = proc.stdout.decode("utf-8", "replace")
        stderr = proc.stderr.decode("utf-8", "replace")
        for mask in scenario.get("mask", []):
            stdout = "\n".join("<masked>" if line.startswith(mask) else line
                               for line in stdout.split("\n"))
        stdout = stdout.replace(machine.tmp, "<tmp>")
        stderr = stderr.replace(machine.tmp, "<tmp>")
        return {
            "exit": proc.returncode,
            "stdout": stdout,
            "stderr": stderr,
            "changes": machine.server.changes() if machine.server else [],
            "files": machine.state_files(),
        }
    finally:
        machine.close()


def run_watcher(cmd: Sequence[str], scenario: Dict[str, Any]) -> Dict[str, Any]:
    """Run the watcher until Herdr hears nothing new, then stop it."""
    data = copy.deepcopy(scenario.get("data") or snapshot())
    machine = Machine(data, scenario.get("config"), plugins=scenario.get("plugins"),
                      files=scenario.get("files"), watcher_stopped=False)
    env = machine.env()
    directory = machine.watcher_dir()
    directory.mkdir(parents=True, exist_ok=True)
    log_path = directory / "title-watcher.log"
    with open(log_path, "a") as log:
        proc = subprocess.Popen(list(cmd) + ["--watch-titles", "-"], cwd=str(directory), env=env,
                                stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                start_new_session=True)
    try:
        events = scenario.get("events", [])
        quiet_for = scenario.get("quiet", 3.0)
        deadline = time.monotonic() + 30
        last_count, last_change = -1, time.monotonic()
        while time.monotonic() < deadline and proc.poll() is None:
            if events and time.monotonic() - last_change > 1.0:
                mutate = events.pop(0)
                with machine.server._lock:
                    mutate(machine.server.data)
                machine.server.event_interval = 0.2
                last_change = time.monotonic()
            count = len(machine.server.changes())
            if count != last_count:
                last_count, last_change = count, time.monotonic()
            elif not events and time.monotonic() - last_change > quiet_for:
                break
            time.sleep(0.05)
        machine.server.event_interval = None
        exited_alone = proc.poll() is not None
        (directory / "title-watcher.stopped").touch()
        try:
            proc.wait(timeout=EXIT_TIMEOUT)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        labels = [tab.get("label") for tab in machine.server.data.get("tabs", [])]
        return {
            "exited_alone": exited_alone,
            "exit": proc.returncode,
            "changes": machine.server.changes(),
            "labels": labels,
            "files": machine.state_files(),
        }
    finally:
        if proc.poll() is None:
            proc.kill()
        machine.close()


# -- scenarios ---------------------------------------------------------------


def keys(*parts: Any) -> List[Step]:
    out: List[Step] = []
    for part in parts:
        if isinstance(part, (int, float)):
            out.append(("wait", part))
        elif isinstance(part, bytes):
            out.append(("keys", part))
        else:
            out.extend(("keys", char.encode()) for char in part)
    return out


UP, DOWN, LEFT, RIGHT = b"\x1b[A", b"\x1b[B", b"\x1b[D", b"\x1b[C"
ENTER, ESC, TAB, SHIFT_TAB, BS, DEL = b"\r", b"\x1b", b"\t", b"\x1b[Z", b"\x7f", b"\x1b[3~"
PGUP, PGDN, HOME, END = b"\x1b[5~", b"\x1b[6~", b"\x1b[H", b"\x1b[F"


def ctrl(letter: str) -> bytes:
    return bytes([ord(letter) - 96])


def click(x: int, y: int) -> bytes:
    return b"\x1b[<0;%d;%dM\x1b[<0;%d;%dm" % (x, y, x, y)


def wheel(down: bool, x: int = 10, y: int = 5) -> bytes:
    return b"\x1b[<%d;%d;%dM" % (65 if down else 64, x, y)


def unicode_snapshot() -> Dict[str, Any]:
    data = snapshot()
    data["tabs"][0]["label"] = "日本語のタブ 🚀 build"
    data["tabs"][5]["label"] = "école combining and a very long label that has to be cut somewhere"
    data["agents"][0]["terminal_title_stripped"] = "宽字符 terminal title"
    data["workspaces"][1]["label"] = "ériç ✓ wörk"
    return data


def empty_snapshot() -> Dict[str, Any]:
    data = snapshot()
    data["tabs"], data["agents"], data["panes"] = [], [], []
    return data


def renamable_snapshot() -> Dict[str, Any]:
    """Tabs Herdr numbered by position, each running one agent with a task title."""
    data = snapshot()
    for index, tab in enumerate(data["tabs"]):
        if tab["workspace_id"] == "w1":
            tab["label"] = str(index + 1)
    for pane in data["panes"]:
        pane["terminal_id"] = "term-" + pane["pane_id"]
    data["agents"][0]["agent_session"] = {"agent": "claude", "kind": "id",
                                         "value": "0123abcd-0123-4567-89ab-0123456789ab"}
    for agent in data["agents"]:
        for pane in data["panes"]:
            if pane["pane_id"] == agent["pane_id"]:
                pane["agent_session"] = agent.get("agent_session")
    return data


def set_status(index: int, status: str) -> Callable[[Dict[str, Any]], None]:
    def mutate(data: Dict[str, Any]) -> None:
        data["agents"][index]["agent_status"] = status
    return mutate


def set_label(tab_index: int, label: str) -> Callable[[Dict[str, Any]], None]:
    def mutate(data: Dict[str, Any]) -> None:
        data["tabs"][tab_index]["label"] = label
    return mutate


BAR_SCENARIOS: Dict[str, Dict[str, Any]] = {
    "bar-open-cancel": {"steps": keys(ESC)},
    "bar-type-and-erase": {"steps": keys("red", BS, BS, "x", LEFT, "q", HOME, "z", END, BS,
                                         ctrl("w"), "two words", ctrl("w"), ctrl("u"), ESC)},
    "bar-scopes": {"steps": keys(TAB, TAB, TAB, TAB, TAB, SHIFT_TAB, "@", BS, "%", "srv", BS, BS,
                                 BS, BS, "$", "!", ctrl("u"), ctrl("c"))},
    "bar-move": {"steps": keys(DOWN, DOWN, UP, UP, UP, ctrl("n"), ctrl("p"), PGDN, PGUP,
                               ctrl("v"), b"\x1bv", ESC)},
    "bar-jump-agent": {"steps": keys(DOWN, ENTER)},
    "bar-jump-query": {"steps": keys("migr", ENTER)},
    "bar-jump-pane": {"steps": keys("%", DOWN, ENTER)},
    "bar-jump-workspace": {"config": {"workspaces": True}, "steps": keys("erestor", ENTER)},
    "bar-jump-nothing": {"steps": keys("zzzzqqq", ENTER, ESC)},
    "bar-rename": {"steps": keys(ctrl("r"), "fresh name", LEFT, BS, RIGHT, ENTER, 3.0, ESC)},
    "bar-rename-cancel": {"steps": keys(ctrl("r"), "abc", ctrl("w"), ESC, ctrl("r"), ENTER, 3.0,
                                        ESC)},
    "bar-rename-pane": {"steps": keys("%", ctrl("r"), 3.0, ESC)},
    "bar-rename-editing": {"steps": keys(ctrl("r"), "one two", ctrl("a"), ctrl("d"), ctrl("e"),
                                         b"\x1b[3~", ctrl("h"), ctrl("u"), "x", ctrl("q"))},
    "bar-close": {"steps": keys(DOWN, DEL, "y", 3.0, ESC)},
    "bar-close-backspace": {"steps": keys(BS, "n", BS, ENTER, 3.0, ESC)},
    "bar-close-panic": {"steps": keys(DEL, ctrl("c"))},
    "bar-preview-toggle": {"steps": keys(ctrl("o"), DOWN, 0.4, ctrl("o"), ctrl("o"), ESC)},
    "bar-preview-on": {"config": {"preview": True}, "size": (90, 20),
                       "steps": keys(DOWN, 0.4, DOWN, 0.4, ESC)},
    "bar-preview-off": {"config": {"preview": False}, "steps": keys(DOWN, ctrl("o"), 0.4, ESC)},
    "bar-mouse": {"steps": keys(wheel(True), wheel(True), wheel(False), click(10, 6),
                                click(10, 6))},
    "bar-mouse-off": {"config": {"mouse": False}, "steps": keys(ESC)},
    "bar-paste": {"steps": keys(b"\x1b[200~multi\nline   paste\x1b[201~", ctrl("u"),
                                b"\x1b[200~@\x1b[201~", ESC)},
    "bar-odd-keys": {"steps": keys(b"\x1bx", b"\x1b[1;5C", b"\x1b[15~", b"\x1bOA", b"\x1b]11;x\x07",
                                   ctrl("t"), b"\x00", "é", ESC)},
    "bar-narrow": {"size": (50, 12), "steps": keys("a", DOWN, ESC)},
    "bar-tiny": {"size": (24, 5), "steps": keys(DOWN, ESC)},
    "bar-tall": {"size": (140, 40), "steps": keys("e", ESC)},
    "bar-bulk": {"data": bulk_snapshot(200), "steps": keys("session 1", PGDN, PGDN, PGUP, ctrl("u"),
                                                        PGDN, ESC)},
    "bar-empty": {"data": empty_snapshot(), "steps": keys("x", BS, ENTER, ESC)},
    "bar-unicode": {"data": unicode_snapshot(), "steps": keys("日本", BS, BS, "ecole", ESC)},
    "bar-icons": {"config": {"agent_icons": "font"}, "steps": keys("claude", ESC)},
    "bar-colors": {"config": {"colors": {"accent": "#ff8800", "muted": "245", "text": "nope",
                                         "working": "1"},
                              "selection_background": "#334455"},
                   "steps": keys(DOWN, ESC)},
    "bar-no-selection": {"config": {"selection_background": None}, "steps": keys(DOWN, ESC)},
    "bar-no-running-time": {"config": {"running_time": False}, "steps": keys(DOWN, ESC)},
    "bar-broken-config": {"config": "{not json", "steps": keys(ESC)},
    "bar-recents": {"files": {"recent.json": {"version": 1, "entries": [
        {"key": "tab:w2:t2", "title": "migrations", "ts": 5},
        {"key": "agent:w1:p4", "title": "cards meaningless", "ts": 4},
        {"key": "nope", "title": "", "ts": "x"},
    ]}}, "steps": keys(ENTER)},
    "bar-unreachable": {"socket_up": False, "steps": []},
}

CLI_SCENARIOS: Dict[str, Dict[str, Any]] = {
    "cli-version": {"args": ["--version"]},
    "cli-help": {"args": ["--help"]},
    "cli-h": {"args": ["-h", "--list"]},
    "cli-list": {"args": ["--list"]},
    "cli-list-bulk": {"args": ["--list"], "data": bulk_snapshot(50)},
    "cli-list-unicode": {"args": ["--list"], "data": unicode_snapshot()},
    "cli-list-unreachable": {"args": ["--list"], "socket_up": False},
    "cli-doctor": {"args": ["--doctor"], "mask": ["python ", "binary "]},
    "cli-doctor-unreachable": {"args": ["--doctor"], "socket_up": False,
                               "mask": ["python ", "binary "]},
    "cli-no-tty": {"args": []},
    "cli-stop-titles": {"args": ["--stop-titles"]},
}

WATCHER_SCENARIOS: Dict[str, Dict[str, Any]] = {
    "watch-titles": {"data": renamable_snapshot(), "config": {"auto_titles": True}},
    "watch-titles-lowercase": {"data": renamable_snapshot(),
                               "config": {"auto_titles": True, "lowercase_titles": True}},
    "watch-status": {"data": renamable_snapshot(),
                     "config": {"auto_titles": False, "tab_status": True},
                     "events": [set_status(0, "blocked"), set_status(2, "done")]},
    "watch-both": {"data": renamable_snapshot(),
                   "config": {"auto_titles": True, "tab_status": True},
                   "events": [set_status(1, "idle"), set_label(3, "my own name")]},
    "watch-icons": {"data": renamable_snapshot(),
                    "config": {"tab_status": True, "agent_icons": "font"}},
    "watch-renamer": {"data": renamable_snapshot(), "config": {"auto_titles": True},
                      "plugins": [
                          {"plugin_id": "herdr-bar", "enabled": True, "plugin_root": ROOT},
                          {"plugin_id": "someone.tab-namer", "enabled": True,
                           "description": "Automatic tab renaming for agents"},
                      ]},
    "watch-unregistered": {"data": renamable_snapshot(), "config": {"auto_titles": True},
                           "plugins": []},
    "watch-disabled": {"data": renamable_snapshot(), "config": {"auto_titles": False}},
}

KINDS = (("bar", BAR_SCENARIOS, run_bar), ("cli", CLI_SCENARIOS, run_cli),
         ("watch", WATCHER_SCENARIOS, run_watcher))


def run_scenarios(cmd: Sequence[str], only: Optional[str]) -> Dict[str, Any]:
    results: Dict[str, Any] = {}
    for _, scenarios, runner in KINDS:
        for name, scenario in scenarios.items():
            if only and only not in name:
                continue
            results[name] = runner(cmd, copy.deepcopy(scenario) if "events" not in scenario
                                   else dict(scenario, events=list(scenario["events"])))
    return results


def diff(name: str, left: Any, right: Any) -> List[str]:
    a = json.dumps(left, indent=1, ensure_ascii=False, sort_keys=True).splitlines()
    b = json.dumps(right, indent=1, ensure_ascii=False, sort_keys=True).splitlines()
    return list(difflib.unified_diff(a, b, "python:" + name, "rust:" + name, lineterm="", n=2))


def compare(reference: Dict[str, Any], candidate: Dict[str, Any]) -> int:
    failures = 0
    for name in reference:
        if name not in candidate:
            continue
        lines = diff(name, reference[name], candidate[name])
        if lines:
            failures += 1
            print("✗ %s" % name)
            print("\n".join(lines[:120]))
            if len(lines) > 120:
                print("  … %d more diff lines" % (len(lines) - 120))
        else:
            print("✓ %s" % name)
    print("\n%d/%d scenarios differ" % (failures, len(candidate)))
    return 1 if failures else 0


def main(argv: Sequence[str]) -> int:
    args = list(argv)

    def option(flag: str) -> Optional[str]:
        return args[args.index(flag) + 1] if flag in args else None

    only = option("--only")
    rust = option("--rust")
    rust_cmd = [os.path.abspath(rust)] if rust else None
    if "--save" in args:
        results = run_scenarios(PYTHON_CMD, only)
        with open(option("--save"), "w") as handle:
            json.dump(results, handle, indent=1, ensure_ascii=False, sort_keys=True)
            handle.write("\n")
        return 0
    if "--check" in args:
        with open(option("--check")) as handle:
            reference = json.load(handle)
        return compare(reference, run_scenarios(rust_cmd or PYTHON_CMD, only))
    if rust_cmd is None:
        print(__doc__)
        return 2
    reference = run_scenarios(PYTHON_CMD, only)
    return compare(reference, run_scenarios(rust_cmd, only))


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
