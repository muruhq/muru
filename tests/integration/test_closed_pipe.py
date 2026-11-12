"""A muructl command whose reader has gone (issue 608).

A command that only prints is ended by SIGPIPE when the reader of its
output has closed, as `head` and `grep -q` close it, and says nothing on
standard error, as other command-line tools do; clap's help is left to
handle a closed pipe its own way.
"""

from __future__ import annotations

import os
import signal
import subprocess
from pathlib import Path

from support import server as support


def closed_reader(arguments: list[str]) -> subprocess.CompletedProcess[bytes]:
    """Run muructl with its standard output a pipe whose read end is closed."""

    muructl = support.muructl_path()
    assert muructl is not None
    read, write = os.pipe()
    os.close(read)

    try:
        return subprocess.run(
            [str(muructl), *arguments],
            stdout=write,
            stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL,
            check=False,
            timeout=30,
        )
    finally:
        os.close(write)


def test_a_command_that_prints_ends_quietly_when_its_reader_has_gone(tmp_path: Path) -> None:
    # With no daemon running, daemon status prints where one would keep
    # its files and exits 3; its first line meets the closed pipe.

    status = closed_reader(
        [
            "daemon",
            "status",
            "--state-dir",
            str(tmp_path / "state"),
            "--run-dir",
            str(support.short_dir("muru-run-")),
            "--log-dir",
            str(tmp_path / "logs"),
            "--config",
            str(tmp_path / "muru.toml"),
            "--cache-dir",
            str(tmp_path / "cache"),
        ]
    )
    assert status.returncode == -signal.SIGPIPE, (status.returncode, status.stderr)
    assert b"panic" not in status.stderr, status.stderr

    shown = closed_reader(["--help"])
    assert shown.returncode == 0, (shown.returncode, shown.stderr)
    assert b"panic" not in shown.stderr, shown.stderr
