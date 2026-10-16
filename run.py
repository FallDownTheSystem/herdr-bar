#!/usr/bin/env python3
"""Entry point of the Python build of the Herdr bar.

The plugin now ships a Rust binary at ``bin/herdr-bar`` and the manifest runs
it directly. This file stays for two callers: a title watcher started by an
older, Python release, which re-executes ``run.py`` when the plugin files
change and so hands over to the binary here, and the Python reference that
the parity and bench scripts compare the binary against. Set
``HERDR_BAR_PYTHON=1`` to run the Python implementation even when the binary
is there.

Herdr launches manifest commands with the plugin directory as the working
directory, so the Python path only has to put ``src`` on the import path.
"""

from __future__ import annotations

import os
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(ROOT, "src"))


def _binary():
    """The built binary next to this file, unless the Python build is forced."""
    if os.environ.get("HERDR_BAR_PYTHON"):
        return None
    name = "herdr-bar.exe" if os.name == "nt" else "herdr-bar"
    path = os.path.join(ROOT, "bin", name)
    return path if os.path.isfile(path) else None


def _run_binary(path: str) -> int:
    if os.name != "nt":
        # exec keeps the pid and every inherited descriptor, including the
        # watcher lock whose number is on the command line.
        os.execv(path, [path, *sys.argv[1:]])
    import subprocess
    flags = 0
    try:
        import ctypes
        if not ctypes.windll.kernel32.GetConsoleWindow():
            flags = 0x08000000  # CREATE_NO_WINDOW: a detached watcher stays hidden
    except (AttributeError, OSError):
        pass
    try:
        return subprocess.call([path, *sys.argv[1:]], creationflags=flags)
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    binary = _binary()
    if binary is not None:
        sys.exit(_run_binary(binary))
    if sys.argv[1:] == ["--open"]:
        # The Windows open action only runs the Herdr CLI; skip the bar's imports.
        from herdr_bar.opener import open_bar
        sys.exit(open_bar())
    from herdr_bar.cli import main
    sys.exit(main(sys.argv[1:]))
