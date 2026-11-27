"""Rollouts whose capacity follows traffic (design specification section
9.16): with `rollout canary --capacity shared` the baseline and the
candidate share the worker count the baseline has when the rollout
starts, each sized by its share of the traffic. The candidate's workers
are in place before a step's traffic moves and the baseline's leave
after it, or, where the node's budget has no room, the baseline gives
workers up first. A rollback grows the baseline back at once, and the
rollout's pins are lifted when it ends.
"""

from __future__ import annotations

import json
import time
from collections.abc import Callable
from pathlib import Path

from support.daemon import SHOP, Daemon, start_daemon
from support.load import Load

FOUR = ["--processes", "4"]
SMALL = [*FOUR, "--hold", "--initial-processes", "1"]
CANARY = ["--steps", "25,50,75", "--hold", "2s", "--capacity", "shared"]


def wait_until(predicate: Callable[[], bool], timeout: float = 60) -> bool:
    deadline = time.monotonic() + timeout

    while not predicate():
        if time.monotonic() >= deadline:
            return False

        time.sleep(0.25)

    return True


def workers_of(daemon: Daemon, release: str) -> str:
    """The WORKERS column of the default group of `shop@release`."""

    lines = daemon.muructl("scale", "status", f"shop@{release}").stdout.splitlines()
    fields = next(line for line in lines[2:] if line.startswith("default")).split()
    slots = next(index for index, field in enumerate(fields) if "/" in field)
    return " ".join(fields[1:slots])


def rollout(daemon: Daemon) -> dict[str, object]:
    shown = json.loads(daemon.muructl("rollout", "status", "shop", "--json").stdout)
    latest: dict[str, object] = shown["rollouts"][-1]
    return latest


def capacity_events(daemon: Daemon, after: int = 0) -> list[str]:
    """The message of each `rollout.capacity` event past a sequence number."""

    events = json.loads(daemon.muructl("events", "--json").stdout)["events"]
    return [
        event["message"]
        for event in events
        if event["type"] == "rollout.capacity" and int(event["sequence"]) > after
    ]


def last_sequence(daemon: Daemon) -> int:
    events = json.loads(daemon.muructl("events", "--json").stdout)["events"]
    return max(int(event["sequence"]) for event in events)


def test_a_canary_that_shares_capacity_sizes_both_releases_by_their_share(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(tmp_path)

    try:
        # The candidate is held at its own four workers, which the
        # rollout brings down to its share before anything else.

        assert daemon.deploy("shop", SHOP, *FOUR) == "muru: release shop@r1 is ready and serving"
        daemon.deploy("shop", SHOP, *FOUR, "--hold")

        with Load(daemon.port, "/info") as load:
            started = daemon.muructl("rollout", "canary", "shop@r2", *CANARY).stdout
            assert started == (
                "muru: rollout 1 of shop@r2 started against r1: "
                "step 1 of 3: 25% for 2s, on a shared total of 4 workers\n"
            )
            first = rollout(daemon)
            assert (first["capacity"], first["totals"]) == ("shared", {"default": 4}), first

            # The two releases are the rollout's to size while it runs.

            refused = daemon.muructl("scale", "shop@r1", "--unpin", check=False)
            assert "is sized by rollout 1, which shares capacity" in refused.stderr, refused.stderr

            assert wait_until(lambda: rollout(daemon)["state"] == "promoted"), rollout(daemon)
            time.sleep(0.5)

        load.check("a canary that shares capacity")

        # At each step the candidate's workers were in place before the
        # traffic moved, and the baseline's left after it: 3 and 1, 2 and
        # 2, 1 and 3, then the whole total ahead of the promotion.

        assert capacity_events(daemon) == [
            "r2 default: drained from 4 to 1 worker, its share at 25%",
            "r1 default: drained from 4 to 3 workers, its share with r2 at 25%",
            "r2 default: grown from 1 to 2 workers, its share at 50%, ahead of the traffic",
            "r1 default: drained from 3 to 2 workers, its share with r2 at 50%",
            "r2 default: grown from 2 to 3 workers, its share at 75%, ahead of the traffic",
            "r1 default: drained from 2 to 1 worker, its share with r2 at 75%",
            "r2 default: grown from 3 to 4 workers, the whole total, ahead of its promotion",
        ]

        # The pins are lifted: the candidate serves at the count it is
        # declared with, and the baseline is kept warm.

        ended = rollout(daemon)
        assert (ended["percent"], ended["sizing"]) == (100, False), ended
        assert workers_of(daemon, "r2") == "4"
        assert wait_until(lambda: workers_of(daemon, "r1") == "1")
        daemon.muructl("scale", "shop@r2", "--processes", "4")
        daemon.muructl("scale", "shop@r2", "--unpin")

        # A rollout that is aborted grows the baseline back before it
        # takes the traffic again, without waiting, and lifts its pins.

        daemon.deploy("shop", SHOP, *SMALL)
        before = last_sequence(daemon)
        daemon.muructl("rollout", "canary", "shop@r3", *CANARY)
        assert wait_until(lambda: workers_of(daemon, "r2") == "3 pinned"), rollout(daemon)
        aborted = daemon.muructl("rollout", "abort", "shop").stdout
        assert "rolled_back: aborted by hand" in aborted, aborted
        assert capacity_events(daemon, before)[-1] == (
            "r2 default: grown back from 3 to 4 workers of its 4, before it takes the traffic again"
        )
        assert wait_until(lambda: workers_of(daemon, "r2") == "4")
        assert wait_until(lambda: workers_of(daemon, "r3") == "1")
    finally:
        daemon.stop()


def test_without_room_in_the_budget_the_baseline_gives_up_its_workers_first(
    tmp_path: Path,
) -> None:
    daemon = start_daemon(tmp_path, config_extra="[daemon.budget]\nworkers = 4\n")

    try:
        daemon.deploy("shop", SHOP, *FOUR, "--max-unavailable", "1")
        daemon.deploy("shop", SHOP, *SMALL)
        most = 0

        with Load(daemon.port, "/info") as load:
            daemon.muructl("rollout", "canary", "shop@r2", *CANARY)

            # Past the first step, whose one worker the candidate had
            # before the rollout, the node never holds more than its
            # budget. The figure is taken once a second.

            def held() -> int:
                status = daemon.muructl("daemon", "status").stdout.splitlines()
                return int(next(line for line in status if line.startswith("budget")).split()[1])

            assert wait_until(lambda: held() == 4), rollout(daemon)

            while rollout(daemon)["state"] == "running":
                most = max(most, held())
                time.sleep(0.25)

        load.check("a canary that shares capacity within a workers budget")
        assert most == 4, most
        assert rollout(daemon)["state"] == "promoted", rollout(daemon)

        # With no room for the candidate's workers the baseline gave one
        # up ahead of each step's traffic, as its max_unavailable allows,
        # and the candidate took the room it left.

        gave = (
            "ahead of the traffic: the node's budget has no room for the workers of r2 until it has"
        )
        assert capacity_events(daemon) == [
            "r1 default: drained from 4 to 3 workers, its share with r2 at 25%",
            f"r1 default: drained from 3 to 2 workers, {gave}",
            "r2 default: grown from 1 to 2 workers, its share at 50%, ahead of the traffic",
            f"r1 default: drained from 2 to 1 worker, {gave}",
            "r2 default: grown from 2 to 3 workers, its share at 75%, ahead of the traffic",
            "r2 goes on short of its share at 100%, the node's budget having no room: "
            "default at 3 of 4 workers",
        ]

        # The baseline keeps its one worker while it has traffic, so the
        # budget leaves the candidate one short of the total, which it
        # is promoted with.

        assert workers_of(daemon, "r2").split()[0] == "3"
    finally:
        daemon.stop()
