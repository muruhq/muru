"""The tests' own clean-up: what a failed test leaves running is ended."""

from __future__ import annotations

import subprocess
import sys
import time

from support import server as support


def test_what_runs_from_a_short_directory_is_ended() -> None:
    directory = support.short_dir("muru-run-")
    process = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(300)", str(directory)]
    )

    try:
        deadline = time.monotonic() + 5

        while not subprocess.run(
            ["pgrep", "-f", str(directory)], capture_output=True, text=True, check=False
        ).stdout:
            assert time.monotonic() < deadline, "the process never appeared"
            time.sleep(0.05)

        support.end_processes_using(directory)
        assert process.wait(timeout=5) != 0
    finally:
        process.kill()
