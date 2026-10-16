"""The process bench has to be timing the plugin, not its own plumbing.

One real session against the fake Herdr: if the harness stops recognizing a
drawn frame, stops getting keystrokes through, or loses the exit, the numbers
it saves are fiction, so these checks fail before a baseline can be saved.
"""

from __future__ import annotations

import os
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "scripts"))
sys.path.insert(0, ROOT)


@unittest.skipIf(os.name == "nt", "pseudo-terminals and Unix sockets are POSIX only")
class ProcessBenchTest(unittest.TestCase):
    def test_one_session_is_measured_end_to_end(self):
        import bench_process

        result = bench_process.bar_session(bench_process.DEFAULT_CMD,
                                           bench_process.bulk_snapshot(0))
        self.assertGreater(result["first_frame_ms"], 0.0)
        # Every key typed and every backspace produced a frame of its own.
        self.assertEqual(len(result["keystroke_ms"]), 2 * len(bench_process.QUERY))
        self.assertGreater(result["cpu_ms"], 0.0)
        self.assertGreater(result["max_rss_kb"], 0)
        self.assertGreater(result["calls"], 0)

    def test_bulk_sessions_add_rows(self):
        import bench_process

        data = bench_process.bulk_snapshot(5)
        base = bench_process.bulk_snapshot(0)
        self.assertEqual(len(data["tabs"]), len(base["tabs"]) + 5)
        self.assertEqual(len(data["agents"]), len(base["agents"]) + 5)


if __name__ == "__main__":
    unittest.main()
