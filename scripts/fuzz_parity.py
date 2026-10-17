#!/usr/bin/env python3
"""Throw random inputs at the Python functions and their Rust ports, and compare.

    cargo build --release --features probe
    python3 scripts/fuzz_parity.py --probe target/release/herdr-bar
    python3 scripts/fuzz_parity.py --probe ... --only json --count 20000 --seed 7

scripts/parity.py proves the builds behave alike end to end; this proves the
pieces do, on inputs no scenario would think of: case folding of Kelvin signs,
JSON with duplicate keys, terminal replies cut mid-sequence, SGR soup at odd
widths. The probe build of the binary answers one JSON line per call (see
src/probe.rs); every case here builds the Python answer in the same shape.

A Python exception is not compared -- a traceback is not behaviour anyone
relies on -- but it is counted, so a case that mostly raises shows up.
Mismatches are shrunk to a smaller input that still differs before printing.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import struct
import subprocess
import sys
from typing import Any, Callable, Dict, List, Optional, Tuple

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "src"))

from herdr_bar import (  # noqa: E402
    age,
    auto_title,
    fuzzy,
    items,
    keys,
    paint,
    renaming_guard,
    sequences,
    tab_status,
    textutil,
    theme,
)

# --------------------------------------------------------------------------
# Inputs

PIECES = [
    # Plain text, and the delimiters fuzzy scoring cares about.
    *"abcdefxyzABCXYZ0123456789",
    " ",
    "  ",
    "-",
    "_",
    ".",
    "/",
    "\\",
    ":",
    "#",
    "'",
    '"',
    "~",
    "@",
    "|",
    " | ",
    "(",
    ")",
    "[",
    "]",
    "<",
    ">",
    "…",
    # Controls and line breaks of every kind Python knows.
    "\t",
    "\n",
    "\r",
    "\r\n",
    "\x0b",
    "\x0c",
    "\x1c",
    "\x1d",
    "\x1e",
    "\x1f",
    "\x85",
    "\u2028",
    "\u2029",
    "\x00",
    "\x07",
    "\x7f",
    "\x1b",
    # Escape sequences.
    "\x1b[31m",
    "\x1b[0m",
    "\x1b[1;38;5;200m",
    "\x1b[38;2;1;2;3m",
    "\x1b[?25l",
    "\x1b]0;t\x07",
    "\x1b]8;;x\x1b\\",
    "\x1b(B",
    "\x1b[K",
    "\x1b[",
    "\x1b]",
    # Wide, zero-width and combining.
    "中",
    "文字",
    "😀",
    "👍🏽",
    "👨‍👩‍👧",
    "🇯🇵",
    "가",
    "ｶ",
    "Ａ",
    "\u3000",
    "e\u0301",
    "\u0300",
    "\u200b",
    "\u200d",
    "\ufe0f",
    "\u00ad",
    "\u1160",
    "\u0e33",
    "\U000f0001",
    "\ue000",
    # Case mapping oddities.
    "İ",
    "ı",
    "K",
    "ſ",
    "ß",
    "ǅ",
    "Σ",
    "ς",
    "ﬃ",
    "ŉ",
    "Ω",
    "ẞ",
    "Å",
    "ǰ",
    "ΐ",
    "ᾳ",
    # Digits and spaces that are not ASCII.
    "٣",
    "５",
    "²",
    "①",
    "߀",
    "𝟘",
    "\u00a0",
    "\u2003",
    "\u180e",
    "\u205f",
]

HOME = os.path.expanduser("~")


def code_point(rng: random.Random) -> str:
    while True:
        value = rng.choice(
            [
                rng.randrange(0x80, 0x800),
                rng.randrange(0x800, 0x10000),
                rng.randrange(0x10000, 0x110000),
                rng.randrange(0x20, 0x7F),
            ]
        )
        if not 0xD800 <= value < 0xE000:
            return chr(value)


def text(rng: random.Random, size: int = 12, pieces: Optional[List[str]] = None) -> str:
    out = []
    for _ in range(rng.randrange(size + 1)):
        if pieces and rng.random() < 0.5:
            out.append(rng.choice(pieces))
        elif rng.random() < 0.1:
            out.append(code_point(rng))
        else:
            out.append(rng.choice(PIECES))
    return "".join(out)


def pick(rng: random.Random, *options: Any) -> Any:
    return rng.choice(options)


def scalar(rng: random.Random) -> Any:
    return pick(rng, None, True, False, 0, 1, -1, 2, 3.5, "", "x", text(rng, 4), [], {})


def maybe(rng: random.Random, value: Any, odds: float = 0.15) -> Any:
    return scalar(rng) if rng.random() < odds else value


IDS = {"w": ["w1", "w2", "w3"], "t": ["t1", "t2", "t3", "t4"], "p": ["p1", "p2", "p3", "p4", "p5"]}
KINDS = ["claude", "codex", "gemini", "opencode", None, ""]
STATUSES = ["working", "blocked", "idle", "done", "unknown", "", None, "Working", " idle "]


def uuid(rng: random.Random) -> str:
    value = "%08x-%04x-%04x-%04x-%012x" % tuple(rng.getrandbits(b) for b in (32, 16, 16, 16, 48))
    if rng.random() < 0.2:
        value = value.upper()
    if rng.random() < 0.2:
        index = rng.randrange(len(value))
        value = (
            value[:index] + rng.choice(["g", "-", "", "٣", "Ａ", "0", "00"]) + value[index + 1 :]
        )
    return value


def cwd(rng: random.Random) -> Any:
    return maybe(
        rng,
        pick(
            rng,
            HOME + "/src/herdr-bar",
            HOME,
            "/tmp/x y/",
            "/srv/" + text(rng, 3),
            "C:\\Users\\me",
            "/home/x/Café-Ｐroject",
            "/a/b/",
            "",
            "/",
            "~/z",
            HOME + "x",
            "/p/123abc",
            "/p/---",
        ),
    )


def title_text(rng: random.Random, folder: str = "") -> str:
    base = pick(
        rng,
        "Fix the bug",
        "claude",
        "Claude Code",
        "codex",
        text(rng, 8),
        "C:\\Program Files\\PowerShell\\7\\pwsh.EXE",
        "Administrator: C:\\x\\cmd.exe",
        "\\\\server\\share\\run.BAT",
        "c:\\a.com\n",
        "✳ Thinking",
        folder,
    )
    if folder and rng.random() < 0.3:
        base += " | " + folder
    return base


def snapshot(rng: random.Random) -> Dict[str, Any]:
    def ident(kind: str) -> Any:
        # Mostly hashable: an unhashable id only makes Python raise.
        if rng.random() < 0.1:
            return (
                pick(rng, None, True, 0, 1, 2.5, "", "x", [], {})
                if rng.random() < 0.2
                else pick(rng, None, True, 0, 1, 2.5, "", "x")
            )
        return rng.choice(IDS[kind])

    workspaces = []
    for _ in range(rng.randrange(4)):
        workspace = {
            "workspace_id": ident("w"),
            "number": maybe(rng, rng.randrange(1, 4)),
            "label": maybe(rng, pick(rng, "", "main", text(rng, 6)), 0.3),
            "focused": maybe(rng, rng.random() < 0.3),
            "active_tab_id": ident("t"),
            "agent_status": rng.choice(STATUSES),
        }
        if rng.random() < 0.4:
            workspace["worktree"] = maybe(
                rng, {"branch": maybe(rng, pick(rng, "main", " feat/x ", ""))}
            )
        workspaces.append(workspace)
    tabs = []
    for _ in range(rng.randrange(6)):
        tab = {
            "tab_id": ident("t"),
            "workspace_id": ident("w"),
            "number": maybe(rng, rng.randrange(1, 6), 0.3),
            "label": maybe(
                rng,
                pick(
                    rng,
                    None,
                    "",
                    "1",
                    "2",
                    "build",
                    "● build",
                    "\U000f0001  ● Fix it",
                    text(rng, 6),
                ),
                0.2,
            ),
            "agent_status": rng.choice(STATUSES),
        }
        if rng.random() < 0.2:
            tab["title_conflict"] = maybe(rng, True)
        tabs.append(tab)
    panes = []
    for _ in range(rng.randrange(7)):
        folder = os.path.basename(str(cwd(rng) or "").rstrip("/"))
        pane = {
            "pane_id": ident("p"),
            "tab_id": ident("t"),
            "workspace_id": ident("w"),
            "cwd": cwd(rng),
            "foreground_cwd": maybe(rng, None, 0.3),
            "label": maybe(rng, pick(rng, None, None, "", "logs", text(rng, 5)), 0.1),
            "title": maybe(rng, pick(rng, None, "", title_text(rng, folder)), 0.1),
            "terminal_title_stripped": maybe(rng, title_text(rng, folder), 0.2),
            "terminal_id": maybe(rng, pick(rng, "term-1", "term-2", "", None), 0.1),
        }
        if rng.random() < 0.5:
            pane["agent_session"] = maybe(
                rng,
                {
                    "agent": rng.choice(KINDS),
                    "kind": pick(rng, "id", "id", "name"),
                    "value": maybe(rng, uuid(rng)),
                },
            )
        panes.append(pane)
    agents = []
    for _ in range(rng.randrange(5)):
        kind = rng.choice(KINDS)
        folder_cwd = cwd(rng)
        folder = os.path.basename(str(folder_cwd or "").rstrip("/"))
        agent = {
            "pane_id": ident("p"),
            "tab_id": ident("t"),
            "agent": kind,
            "cwd": folder_cwd,
            "agent_status": rng.choice(STATUSES),
            "name": maybe(
                rng, pick(rng, None, "", folder.lower(), folder.lower() + "-2", text(rng, 4))
            ),
            "title": maybe(rng, None, 0.3),
            "terminal_title_stripped": maybe(rng, title_text(rng, folder), 0.3),
        }
        if rng.random() < 0.3:
            agent["display_agent"] = maybe(rng, pick(rng, "Claude", " "))
        if rng.random() < 0.6:
            agent["agent_session"] = maybe(
                rng,
                {
                    "agent": pick(rng, kind, kind, "claude"),
                    "kind": pick(rng, "id", "id", "x"),
                    "value": maybe(rng, uuid(rng)),
                },
            )
        agents.append(agent)
    result: Dict[str, Any] = {
        "workspaces": workspaces,
        "tabs": tabs,
        "panes": panes,
        "agents": agents,
    }
    for key in list(result):
        if rng.random() < 0.05:
            result[key] = maybe(rng, None, 1.0)
        elif rng.random() < 0.05:
            del result[key]
    result["focused_tab_id"] = ident("t")
    result["focused_pane_id"] = ident("p")
    return result


def json_value(rng: random.Random, depth: int = 0) -> Any:
    roll = rng.random()
    if depth > 3 or roll < 0.5:
        return pick(
            rng,
            None,
            True,
            False,
            rng.randrange(-5, 1000),
            rng.getrandbits(rng.choice([32, 64, 127, 200])) * pick(rng, 1, -1),
            rng.random() * 10 ** rng.randrange(-8, 30),
            0.1,
            1e16,
            1e-7,
            -0.0,
            5e-324,
            1.7976931348623157e308,
            float("inf"),
            float("nan"),
            text(rng, 6),
        )
    if roll < 0.75:
        return [json_value(rng, depth + 1) for _ in range(rng.randrange(4))]
    return {
        pick(rng, "a", "b", "é", "", "10", "2", "B", text(rng, 3)): json_value(rng, depth + 1)
        for _ in range(rng.randrange(4))
    }


def json_text(rng: random.Random) -> str:
    raw = json.dumps(json_value(rng), ensure_ascii=rng.random() < 0.5)
    roll = rng.random()
    if roll < 0.25:
        raw = (
            rng.choice([" ", "\n", "\t", "\r", ""])
            + raw
            + rng.choice([" ", "\n", "", "\x0b", "\u00a0"])
        )
    elif roll < 0.5 and raw:
        # Damage it: most of these are errors, a few are other valid documents.
        index = rng.randrange(len(raw) + 1)
        raw = (
            raw[:index]
            + rng.choice(
                [
                    "",
                    ",",
                    "]",
                    "}",
                    '"',
                    "\\",
                    "\\u00e9",
                    "\\ud83d\\ude00",
                    "\\/",
                    "\\x",
                    "1",
                    "-",
                    "e5",
                    ".5",
                    "NaN",
                    "Infinity",
                    "-Infinity",
                    "true",
                    "nul",
                    "\x01",
                    ' "a": 1, "a": 2',
                ]
            )
            + raw[index + rng.randrange(3) :]
        )
    elif roll < 0.6:
        raw = rng.choice(
            [
                '{"a": 1, "a": 2}',
                '{"b": 1, "a": 2, "b": 3}',
                "01",
                "-0",
                "1E+05",
                "1.0e-5",
                "123456789012345678901234567890",
                "[1,]",
                "1 2",
                '"\\u0000"',
                "-",
                "[" * 30 + "]" * 30,
                '"\t"',
                "1.5e999",
                "-1e400",
                "0.1e1",
            ]
        )
    return raw


def sgr_text(rng: random.Random) -> str:
    params = [
        "",
        "0",
        "1",
        "2",
        "3",
        "4",
        "7",
        "22",
        "23",
        "24",
        "27",
        "39",
        "49",
        "31",
        "42",
        "90",
        "107",
        "38;5;%d" % rng.randrange(260),
        "48;5;12",
        "38;2;%d;2;3" % rng.randrange(300),
        "48;2;9;9;9",
        "1;31",
        "38;5",
        "38;2;1",
        "999",
        "0;1;4",
        ";",
        "1;;4",
    ]
    out = []
    for _ in range(rng.randrange(8)):
        if rng.random() < 0.4:
            out.append("\x1b[%sm" % rng.choice(params))
        else:
            out.append(text(rng, 4))
    return "".join(out)


def key_bytes(rng: random.Random) -> List[Optional[List[int]]]:
    pieces = [
        b"\x1b",
        b"[",
        b"A",
        b"O",
        b"~",
        b";",
        b"1",
        b"5",
        b"\x1b[A",
        b"\x1b[1;5C",
        b"\x1b[3~",
        b"\x1b[5;2~",
        b"\x1b[Z",
        b"\x1bOP",
        b"\x1b[200~",
        b"\x1b[201~",
        b"paste",
        b"\x1b[<0;10;5M",
        b"\x1b[<64;1;1M",
        b"\x1b[<65;2;2M",
        b"\x1b[<0;3;4m",
        b"\x1b[<32;1;1M",
        b"\x1b[<2;1;1M",
        b"\x1b[<0;x;1M",
        b"\x1b[<0;-1;+2M",
        b"\x1b]11;rgb:0/0/0\x07",
        b"\x1b\\",
        b"\r",
        b"\n",
        b"\t",
        b"\x7f",
        b"\x08",
        b"\x00",
        b"\x03",
        b"\x1ba",
        b"\x1b\x1b",
        b"q",
        "é".encode(),
        "😀".encode(),
        "中".encode()[:2],
        b"\xff",
        b"\xc3",
        b"\x80",
        b"\xed\xa0\x80",
        b"\x1b[1;3D",
        b"\x1b[1;9A",
        b"\x1b[99~",
        b"\x1b[?1;2c",
        "\x1b[٣~".encode(),
    ]
    chunks: List[Optional[List[int]]] = []
    for _ in range(rng.randrange(1, 6)):
        if rng.random() < 0.15:
            chunks.append(None)
            continue
        data = b"".join(
            rng.choice(pieces) if rng.random() < 0.85 else bytes([rng.randrange(256)])
            for _ in range(rng.randrange(1, 6))
        )
        chunks.append(list(data))
    return chunks


def reply_bytes(rng: random.Random) -> List[int]:
    def channel() -> str:
        return "".join(rng.choice("0123456789abcdefABCDEFg") for _ in range(rng.randrange(0, 6)))

    body = "\x1b]11;%s:%s/%s/%s" % (
        pick(rng, "rgb", "rgba", "RGB"),
        channel(),
        channel(),
        channel(),
    )
    if rng.random() < 0.3:
        body += "/" + channel()
    body += pick(rng, "\x07", "\x1b\\", "", "\x1b")
    data = body.encode()
    if rng.random() < 0.3:
        data = (
            bytes([rng.randrange(256)]) * rng.randrange(3)
            + data
            + bytes(rng.randrange(256) for _ in range(2))
        )
    return list(data)


def etime(rng: random.Random) -> str:
    parts = [
        str(rng.randrange(0, 100)).zfill(rng.choice([1, 2])) for _ in range(rng.randrange(1, 4))
    ]
    value = ":".join(parts)
    if rng.random() < 0.3:
        value = "%d-%s" % (rng.randrange(0, 400), value)
    if rng.random() < 0.3:
        index = rng.randrange(len(value) + 1)
        value = (
            value[:index]
            + rng.choice([" ", "-", ":", "x", "٣", "+", "", "--", "\t"])
            + value[index:]
        )
    return rng.choice(["", " ", "\n"]) + value + rng.choice(["", " ", "\n"])


def theme_spec(rng: random.Random) -> Any:
    hex_digits = "0123456789abcdefABCDEF٣５g"
    return pick(
        rng,
        None,
        True,
        7,
        255,
        256,
        -1,
        3.0,
        "",
        " "
        + pick(
            rng,
            "red",
            "Bright-Blue",
            "bright_red",
            "grey",
            "gray",
            "default",
            "NONE",
            "inherit",
            "reset",
        )
        + " ",
        "#" + "".join(rng.choice(hex_digits) for _ in range(rng.choice([3, 6, 7]))),
        pick(rng, "0", "12", "255", "256", "٣", "²", "007", "+5", "-0", " 9 "),
        text(rng, 4),
    )


# --------------------------------------------------------------------------
# Cases: a generator of arguments, and the Python function shaped like the probe


def match_py(query: str, value: str) -> Any:
    found = fuzzy.match(query, value)
    return None if found is None else [found.score, list(found.positions)]


def keys_py(chunks: List[Optional[List[int]]]) -> Any:
    decoder = keys.KeyDecoder()
    out: List[Any] = []
    for chunk in chunks:
        if chunk is None:
            out.append(decoder.pending_escape())
            events = decoder.flush()
        else:
            events = decoder.feed(bytes(chunk))
        out.extend([event.kind, event.value, event.x, event.y] for event in events)
    return out


SURROGATE = re.compile("[\ud800-\udfff]")


def json_py(raw: str) -> Any:
    try:
        value = json.loads(raw)
    except ValueError:
        return "error"
    if SURROGATE.search(json.dumps(value, ensure_ascii=False)):
        # A lone surrogate escape is a Python str but not a Rust one; the port
        # reads it as U+FFFD, which Python could not even print.
        raise ValueError("lone surrogate")
    return [
        json.dumps(value),
        json.dumps(value, sort_keys=True),
        json.dumps(value, indent=2),
        str(value),
        bool(value),
    ]


def json_eq_py(first: str, second: str) -> Any:
    try:
        return json.loads(first) == json.loads(second)
    except ValueError:
        return "error"


def item_py(item: items.Item) -> Dict[str, Any]:
    # Herdr's ids are strings. The port holds them as strings whatever the
    # snapshot says, so an id of another type comes out as its str().
    found = {
        name: getattr(item, name)
        for name in (
            "kind",
            "key",
            "title",
            "detail",
            "subtitle",
            "workspace_id",
            "workspace_label",
            "tab_id",
            "pane_id",
            "agent",
            "agent_name",
            "status",
            "tab_number",
            "focused",
            "fields",
            "detail_field",
        )
    }
    for name in ("key", "workspace_id", "tab_id", "pane_id"):
        if found[name] is not None:
            found[name] = str(found[name])
    return found


def painter_py(frames: List[Any]) -> List[str]:
    painter = paint.Painter()
    out = []
    for frame in frames:
        if frame is None:
            painter.reset()
            continue
        out.append(painter.frame(frame[0], frame[1], frame[2]))
    return out


def painter_args(rng: random.Random) -> List[Any]:
    width, height = rng.randrange(1, 30), rng.randrange(1, 5)
    rows = [sgr_text(rng) for _ in range(height)]
    frames: List[Any] = []
    for _ in range(rng.randrange(1, 7)):
        roll = rng.random()
        if roll < 0.1:
            frames.append(None)
            continue
        if roll < 0.2:
            width = rng.randrange(1, 30)
        rows = [row if rng.random() < 0.6 else sgr_text(rng) for row in rows]
        if rng.random() < 0.2:
            rows = rows[: rng.randrange(len(rows) + 1)]
        frames.append([list(rows), width, height])
    return [frames]


def logo(rng: random.Random) -> str:
    return pick(rng, "", "\U000f0001", "\ue000", "x")


def tab_label(rng: random.Random) -> str:
    glyphs = list(tab_status.GLYPHS.values())
    if rng.random() < 0.6:
        return tab_status.compose(
            text(rng, 5), rng.choice(list(tab_status.GLYPHS)), logo(rng)
        ) + pick(rng, "", " ", text(rng, 3))
    return pick(rng, "", " ") + rng.choice(glyphs) + pick(rng, " ", "  ", "") + text(rng, 5)


def naming_text(rng: random.Random) -> str:
    words = [
        "auto",
        "automatic",
        "session",
        "title",
        "titles",
        "rename",
        "renames",
        "renaming",
        "name",
        "names",
        "naming",
        "tab",
        "tabs",
        "Tab",
        "AUTO",
        "x",
        "é",
        "_",
        "1",
    ]
    return "".join(
        rng.choice(words + [" ", " ", "-", ".", "ß", "ﬃ"]) for _ in range(rng.randrange(10))
    )


def opening_args(rng: random.Random) -> List[Any]:
    tags = [
        "<command-name>",
        "</command-name>",
        "<command-args>",
        "</command-args>",
        "/",
        "<system-reminder>",
        "</system-reminder>",
        "<a-b>",
        "</a-b>",
        "<X>",
        "</x>",
        "\n",
        " ",
    ]
    content: Any = text(rng, 10, tags)
    if rng.random() < 0.3:
        content = [
            maybe(
                rng,
                {
                    "type": pick(rng, "text", "text", "image"),
                    "text": maybe(rng, text(rng, 6, tags)),
                },
            )
            for _ in range(rng.randrange(4))
        ]
    return [maybe(rng, content, 0.1)]


def agent_title_args(rng: random.Random) -> List[Any]:
    folder_cwd = cwd(rng)
    folder = os.path.basename(str(folder_cwd or "").rstrip("/"))
    agent = {"terminal_title_stripped": maybe(rng, title_text(rng, folder), 0.4), "cwd": folder_cwd}
    pane = {"terminal_title_stripped": maybe(rng, title_text(rng, folder), 0.4), "cwd": cwd(rng)}
    return [agent, pane]


def default_name_args(rng: random.Random) -> List[Any]:
    path = str(cwd(rng) or pick(rng, "/x/123", "/x/.hidden", "/x/" + text(rng, 5)))
    base = re.sub(r"[^a-z0-9_-]+", "-", os.path.basename(path.rstrip("/")).lower()).strip("-_")
    name = pick(
        rng,
        base,
        base + "-2",
        base + "-1",
        base + "-99",
        base + "-100",
        base[:30] + "-12",
        "claude-" + base,
        text(rng, 4),
        "",
        None,
        5,
        base.upper(),
    )
    return [name, rng.choice(KINDS + [3]), path]


def owned(rng: random.Random) -> Dict[str, Any]:
    out: Dict[str, Any] = {}
    for tab in IDS["t"]:
        if rng.random() < 0.4:
            out[tab] = maybe(
                rng,
                {
                    "yielded": maybe(rng, rng.random() < 0.2),
                    "title": pick(rng, "build", "1", None, "Fix the bug"),
                },
            )
    return out


def pid_info(rng: random.Random) -> Dict[str, Any]:
    def pid() -> Any:
        return pick(rng, 1, 42, 0, -3, True, False, "12", 1.5, None, 2**40)

    info = {
        "foreground_process_group_id": maybe(rng, pid()),
        "foreground_processes": maybe(
            rng, [maybe(rng, {"pid": pid()}) for _ in range(rng.randrange(4))]
        ),
    }
    return info


def candidates_py(snap: Dict[str, Any], own: Dict[str, Any]) -> Any:
    # A tab id that is not a string can only be renamed in Python, where Herdr
    # rejects it; the port leaves such tabs alone (see item_py on ids).
    return {k: v for k, v in auto_title._candidates(snap, own).items() if isinstance(k, str)}


CASES: Dict[str, Tuple[Callable[[random.Random], List[Any]], Callable[..., Any]]] = {
    "lower": (lambda r: [text(r)], lambda s: s.lower()),
    "casefold": (lambda r: [text(r)], lambda s: s.casefold()),
    "islower": (lambda r: [text(r, 4)], lambda s: s.islower()),
    "isdigit": (lambda r: [text(r, 3)], lambda s: s.isdigit()),
    "strip": (lambda r: [text(r)], lambda s: s.strip()),
    "split": (lambda r: [text(r)], lambda s: s.split()),
    "splitlines": (lambda r: [text(r)], lambda s: s.splitlines()),
    "int": (
        lambda r: [
            pick(
                r,
                etime(r),
                text(r, 4),
                " -0012 ",
                "1_000",
                "_1",
                "٣٣",
                "+٣",
                "1__0",
                "9" * 45,
                "-" + "9" * 39,
                "１２",
                "1" * 4300,
                "1" * 4301,
                "",
            )
        ],
        # The port saturates at the i128 bounds (see pystr::parse_int).
        lambda s: str(max(min(int(s), 2**127 - 1), 1 - 2**127)),
    ),
    "len": (lambda r: [text(r)], lambda s: len(s)),
    "json": (lambda r: [json_text(r)], json_py),
    "float_repr": (
        lambda r: [
            pick(
                r,
                struct.unpack("<d", r.randbytes(8))[0],
                r.randrange(1, 10**17) / pick(r, 4, 8, 10, 100, 1),
                r.random() * 10 ** r.randrange(-30, 30),
            )
        ],
        repr,
    ),
    "json_eq": (
        lambda r: [json_text(r), pick(r, json_text(r), "1", "1.0", "true", "[1]", "{}")],
        json_eq_py,
    ),
    "display_width": (lambda r: [text(r)], textutil.display_width),
    "visible_width": (lambda r: [sgr_text(r)], textutil.visible_width),
    "strip_ansi": (lambda r: [text(r)], textutil.strip_ansi),
    "sanitize": (lambda r: [text(r), pick(r, " ", "", "?", "\x00")], textutil.sanitize),
    "truncate": (
        lambda r: [text(r), r.randrange(-2, 16), pick(r, "…", "", "...", "中")],
        textutil.truncate,
    ),
    "truncate_middle": (lambda r: [text(r), r.randrange(-2, 16)], textutil.truncate_middle),
    "pad": (lambda r: [text(r), r.randrange(-2, 16)], textutil.pad),
    "window_positions": (
        lambda r: (
            lambda t: [
                t,
                r.randrange(-1, 14),
                sorted(r.sample(range(max(len(t), 1)), min(len(t), r.randrange(4)))),
            ]
        )(text(r, 20)),
        lambda t, w, p: list(textutil.window_positions(t, w, p)),
    ),
    "split_query": (lambda r: [text(r, 8)], fuzzy.split_query),
    "match": (
        lambda r: (
            lambda t: [
                pick(
                    r,
                    text(r, 3),
                    t[: r.randrange(4)].lower(),
                    "".join(r.sample(t, min(len(t), 3))) if t else "",
                ),
                t,
            ]
        )(text(r, 14)),
        match_py,
    ),
    "keys": (lambda r: [key_bytes(r)], keys_py),
    "reply_complete": (lambda r: [reply_bytes(r)], lambda b: sequences.reply_complete(bytes(b))),
    "appearance": (lambda r: [reply_bytes(r)], lambda b: sequences.appearance_from_reply(bytes(b))),
    "parse_etime": (lambda r: [etime(r)], age.parse_etime),
    "format_age": (
        lambda r: [
            pick(
                r,
                r.random() * 10 ** r.randrange(0, 9),
                -r.random() * 5,
                59.999,
                60.0,
                3599.5,
                86400.0,
                -0.5,
                1e18,
            )
        ],
        age.format_age,
    ),
    "pid_for": (lambda r: [pid_info(r)], age.pid_for),
    "theme_parse": (lambda r: [theme_spec(r)], theme._parse),
    "tab_strip": (lambda r: [tab_label(r)], tab_status.strip),
    "tab_compose": (
        lambda r: [text(r, 5), pick(r, *tab_status.GLYPHS, "weird", ""), logo(r)],
        tab_status.compose,
    ),
    "tab_states": (
        lambda r: [snapshot(r)],
        lambda s: sorted([k, v[0], v[1]] for k, v in tab_status.tab_states(s).items()),
    ),
    "naming": (lambda r: [naming_text(r)], lambda s: bool(renaming_guard._NAMING.search(s))),
    "session_id": (lambda r: [uuid(r)], lambda s: bool(auto_title.SESSION_ID.fullmatch(s))),
    "opening": (opening_args, auto_title._opening),
    "agent_title": (agent_title_args, auto_title._agent_title),
    "default_agent_name": (default_name_args, auto_title._default_agent_name),
    "candidates": (lambda r: [snapshot(r), owned(r)], candidates_py),
    "shorten_path": (lambda r: [str(cwd(r) or "")], items.shorten_path),
    "build_items": (lambda r: [snapshot(r)], lambda s: [item_py(i) for i in items.build_items(s)]),
    "build_pane_items": (
        lambda r: [snapshot(r)],
        lambda s: [item_py(i) for i in items.build_pane_items(s)],
    ),
    "painter": (painter_args, painter_py),
}

# --------------------------------------------------------------------------
# Running


class Probe(object):
    def __init__(self, path: str) -> None:
        self.process = subprocess.Popen(
            [path, "--probe"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )

    def call(self, name: str, args: List[Any]) -> Any:
        assert self.process.stdin and self.process.stdout
        self.process.stdin.write(json.dumps([name, *args]) + "\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        if not line:
            raise RuntimeError("the probe exited on %s %r" % (name, args))
        return json.loads(line)

    def close(self) -> None:
        if self.process.stdin:
            self.process.stdin.close()
        self.process.wait()


def normal(value: Any) -> Any:
    """The Python answer as it reads after a trip through JSON, NaN included."""
    return json.loads(json.dumps(value))


def same(first: Any, second: Any) -> bool:
    return json.dumps(first, sort_keys=True) == json.dumps(second, sort_keys=True)


def python_answer(function: Callable[..., Any], args: List[Any]) -> Tuple[bool, Any]:
    try:
        return True, normal(function(*args))
    except Exception as error:  # noqa: BLE001 -- any exception means "not compared"
        return False, type(error).__name__


def smaller(value: Any) -> List[Any]:
    """Candidates one step smaller than value, smallest first."""
    out: List[Any] = []
    if isinstance(value, str) and value:
        chars = list(value)
        for size in (len(chars) // 2, 1):
            if size:
                for start in range(0, len(chars), size):
                    out.append("".join(chars[:start] + chars[start + size :]))
    elif isinstance(value, list) and value:
        for index in range(len(value)):
            out.append(value[:index] + value[index + 1 :])
        for index, element in enumerate(value):
            for candidate in smaller(element)[:8]:
                out.append(value[:index] + [candidate] + value[index + 1 :])
    elif isinstance(value, dict) and value:
        for key in list(value):
            out.append({k: v for k, v in value.items() if k != key})
        for key, element in value.items():
            for candidate in smaller(element)[:8]:
                out.append({**value, key: candidate})
    return out


def shrink(probe: Probe, name: str, function: Callable[..., Any], args: List[Any]) -> List[Any]:
    def differs(candidate: List[Any]) -> bool:
        ok, expected = python_answer(function, candidate)
        return ok and not same(expected, probe.call(name, candidate))

    for _ in range(400):
        for index, argument in enumerate(args):
            for candidate in smaller(argument):
                attempt = args[:index] + [candidate] + args[index + 1 :]
                if differs(attempt):
                    args = attempt
                    break
            else:
                continue
            break
        else:
            return args
    return args


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--probe", required=True, help="a herdr-bar built with --features probe")
    parser.add_argument("--count", type=int, default=3000, help="inputs per case")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--only", action="append", default=[], help="run cases with this name")
    parser.add_argument("--show", type=int, default=3, help="mismatches to print per case")
    options = parser.parse_args(argv)

    probe = Probe(os.path.abspath(options.probe))
    failed = 0
    try:
        for name, (generate, function) in CASES.items():
            if options.only and name not in options.only:
                continue
            rng = random.Random("%s:%d" % (name, options.seed))
            raised = differ = 0
            shown = []
            for _ in range(options.count):
                args = normal(generate(rng))
                ok, expected = python_answer(function, args)
                if not ok:
                    raised += 1
                    continue
                actual = probe.call(name, args)
                if same(expected, actual):
                    continue
                differ += 1
                if len(shown) < options.show:
                    small = shrink(probe, name, function, args)
                    shown.append(
                        (small, python_answer(function, small)[1], probe.call(name, small))
                    )
            note = " (%d raised in Python)" % raised if raised else ""
            print("%-20s %s%s" % (name, "ok" if not differ else "%d differ" % differ, note))
            for args, expected, actual in shown:
                print("    args   %s" % json.dumps(args))
                print("    python %s" % json.dumps(expected))
                print("    rust   %s" % json.dumps(actual))
            failed += bool(differ)
    finally:
        probe.close()
    print(
        "%d of %d cases differ"
        % (failed, len([n for n in CASES if not options.only or n in options.only]))
    )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
