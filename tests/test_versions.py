"""The four places the version lives must agree: the release tag is checked
against them, and install.sh downloads the release named by the manifest."""

import os
import re
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SOURCES = (
    ("herdr-plugin.toml", r'^version\s*=\s*"([^"]+)"'),
    ("Cargo.toml", r'^version\s*=\s*"([^"]+)"'),
    ("src/cli.rs", r'^pub const VERSION: &str = "([^"]+)"'),
    ("src/herdr_bar/__init__.py", r'^__version__\s*=\s*"([^"]+)"'),
)


def read_versions():
    found = {}
    for path, pattern in SOURCES:
        with open(os.path.join(ROOT, path), encoding="utf-8") as handle:
            match = re.search(pattern, handle.read(), re.M)
        found[path] = match.group(1) if match else None
    return found


class VersionsTest(unittest.TestCase):
    def test_every_source_agrees(self):
        found = read_versions()
        self.assertNotIn(None, found.values(), found)
        self.assertEqual(len(set(found.values())), 1, found)


if __name__ == "__main__":
    unittest.main()
