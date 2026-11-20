"""The instances the test session starts (design specification sections
13.8 and 15): each carries the session's label in its registry entry and
its processes' environment, so what a test leaves running is found by
asking the instance registry, pruned, and failed as the test's own."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import time
from pathlib import Path
from typing import Any

from support import server

LABEL_VARIABLE = "MURU_INSTANCE_LABEL"


def label() -> str:
    """The session's label, set when the session starts."""

    return os.environ.get(LABEL_VARIABLE, "")


def labelled_ids(wanted: str) -> set[str]:
    """The IDs of the registry entries with the label: a directory listing
    and a small read for each entry, so the check after every test costs
    next to nothing when nothing was started."""

    found = set()

    try:
        entries = list(server.user_registry().iterdir())
    except OSError:
        return found

    for entry in entries:
        if entry.name.startswith("."):
            continue

        try:
            record = json.loads((entry / "instance.json").read_text())
        except (OSError, ValueError):
            continue

        if record.get("label") == wanted:
            found.add(entry.name)

    return found


def muructl(*arguments: str) -> dict[str, Any] | None:
    """A muructl instance command's JSON answer, or None without muructl."""

    path = server.muructl_path()

    if path is None:
        return None

    completed = subprocess.run(
        [str(path), "instance", *arguments, "--json"],
        capture_output=True,
        text=True,
        check=False,
        timeout=120,
    )

    try:
        answer: dict[str, Any] = json.loads(completed.stdout)
    except ValueError:
        return None

    return answer


def leftovers(
    wanted: str, new: set[str], allow_detached: bool
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """The instances among `new` still running, or detached unless that is
    allowed, and every orphan with the label. What an instance that has
    just gone leaves is given a moment to go too, as a host does once it
    sees its run has ended."""

    deadline = time.monotonic() + 3

    while True:
        answer = muructl("list", "--orphans", "--label", wanted) or {}
        rows = [
            row
            for row in answer.get("instances", [])
            if row.get("id") in new
            and (row["state"] == "running" or (row["state"] == "detached" and not allow_detached))
        ]
        orphans = answer.get("orphans", [])

        if not (rows or orphans) or time.monotonic() > deadline:
            return rows, orphans

        time.sleep(0.2)


def describe(rows: list[dict[str, Any]], orphans: list[dict[str, Any]]) -> str:
    """What was left, one line each: kind, tag, pid and, for a process,
    its title."""

    lines = [f"{row['kind']} {row['tag']} ({row['state']}, pid {row.get('pid')})" for row in rows]
    lines += [
        f"{orphan['role']} pid {orphan['pid']}: {orphan['title']} ({orphan['why']})"
        for orphan in orphans
    ]
    return "\n".join(lines)


def end(rows: list[dict[str, Any]], wanted: str) -> None:
    """Stop what a test left: each running instance asked to stop with
    SIGTERM, then everything of the label pruned, detached processes and
    orphans included."""

    for row in rows:
        if row["state"] == "running":
            try:
                os.kill(row["pid"], signal.SIGTERM)
            except ProcessLookupError:
                pass

    deadline = time.monotonic() + 15

    while time.monotonic() < deadline:
        answer = muructl("list", "--label", wanted) or {}
        ids = {row["id"] for row in rows}

        if not any(
            row.get("id") in ids and row["state"] == "running"
            for row in answer.get("instances", [])
        ):
            break

        time.sleep(0.2)

    for row in rows:
        muructl("prune", "--instance", row["id"], "--detached", "--yes", "--grace", "2s")

    muructl("prune", "--label", wanted, "--yes", "--grace", "2s")


def sweep(wanted: str) -> dict[str, Any] | None:
    """Prune everything with the label at the end of the session,
    detached instances included, and say what was found."""

    if not wanted or server.muructl_path() is None:
        return None

    return muructl("prune", "--label", wanted, "--detached", "--yes", "--grace", "2s")


def pids_of(socket: Path) -> set[int]:
    """The pids an instance supervises, from its process tree: what must
    be gone once it has shut down."""

    path = server.muructl_path()
    assert path is not None
    completed = subprocess.run(
        [str(path), "status", "--processes", "--json", "--socket", str(socket)],
        capture_output=True,
        text=True,
        check=True,
        timeout=60,
    )
    stack = [json.loads(completed.stdout)["root"]]
    pids = set()

    while stack:
        process = stack.pop()
        pids.add(process["pid"])
        stack.extend(process.get("children", []))

    return pids
