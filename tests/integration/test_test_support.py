"""The tests' own clean-up (design specification section 15): every
instance the session starts carries the session's label, and what a test
leaves running is found through the instance registry."""

from __future__ import annotations

import os
import subprocess
import sys
import uuid

from support import instances
from support import server as support


def test_a_labelled_leftover_is_found_and_stopped() -> None:
    # A process carrying the ID of an instance that is gone and a label of
    # its own stands for what a failed test leaves behind.

    label = f"test-{uuid.uuid4().hex[:8]}"
    process = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(300)"],
        env={
            **os.environ,
            "MURU_INSTANCE_ID": uuid.uuid4().hex,
            "MURU_INSTANCE": "gone",
            instances.LABEL_VARIABLE: label,
        },
    )

    try:
        rows, orphans = instances.leftovers(label, set(), allow_detached=False)
        assert rows == [] and [orphan["pid"] for orphan in orphans] == [process.pid], orphans
        assert f"pid {process.pid}" in instances.describe(rows, orphans)

        instances.end(rows, label)
        assert process.wait(timeout=10) != 0
        assert instances.leftovers(label, set(), allow_detached=False) == ([], [])
    finally:
        process.kill()


def test_the_session_labels_what_it_starts() -> None:
    assert instances.label().startswith("pytest-"), instances.label()
    assert support.muructl_path() is not None
