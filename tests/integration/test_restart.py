"""Restarting a worker by hand (design specification section 7.6).

`muructl restart` replaces one worker of a release through the path a
recycle takes, so no request is lost, or every worker in turn with --all.
With --force the worker leaves routing at once, its stacks are written to
the log and it is killed without being asked to stop, which also recovers
a worker that is frozen.
"""

from __future__ import annotations

import json
import os
import signal
import time
from collections.abc import Callable

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture
from support.load import Load

daemon = daemon_fixture


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


def workers(daemon: Daemon) -> dict[int, int]:
    """The pid serving at each worker index, from the host's status."""

    status = json.loads(daemon.muructl("debug", "status", "shop", "--json").stdout)
    return {
        int(worker["index"]): int(worker["pid"])
        for worker in status["workers"]
        if worker["ok"] and worker["result"]["state"] == "ready"
    }


def deploy_shop(daemon: Daemon) -> dict[int, int]:
    assert daemon.deploy(
        "shop",
        SHOP,
        "--processes",
        "2",
        "--timeout",
        "hard_drain_grace=1s",
        "--timeout",
        "shutdown=2s",
    ) == ("muru: release shop@r1 is ready and serving")
    pids = workers(daemon)
    assert sorted(pids) == [0, 1], pids
    return pids


def test_a_worker_is_replaced_with_no_request_lost(daemon: Daemon) -> None:
    before = deploy_shop(daemon)

    with Load(daemon.port) as load:
        lines = daemon.muructl("restart", "shop", "--worker", "0").stdout.splitlines()
        time.sleep(0.5)

    load.check("a restart")
    after = workers(daemon)
    assert after[0] != before[0] and after[1] == before[1], (before, after)
    assert lines == [
        f"muru: worker 0 (pid {before[0]}) is being replaced by pid {after[0]}",
        f"muru: worker 0 is served by pid {after[0]}; pid {before[0]} has stopped",
    ]
    events = daemon.events()
    assert "worker.restarted" in events, events
    log = daemon.log()
    assert f"worker 0 (pid {before[0]}) replaced by pid {after[0]}: restarted on request" in log


def test_a_worker_is_named_by_pid_and_a_missing_one_is_refused(daemon: Daemon) -> None:
    before = deploy_shop(daemon)
    daemon.muructl("restart", "shop@r1", "--pid", str(before[1]))
    after = workers(daemon)
    assert after[1] != before[1] and after[0] == before[0], (before, after)

    refused = daemon.muructl("restart", "shop", "--worker", "7", check=False)
    assert refused.returncode != 0
    assert "shop@r1 has no serving worker 7" in refused.stderr, refused.stderr


def test_force_recovers_a_frozen_worker(daemon: Daemon) -> None:
    before = deploy_shop(daemon)
    daemon.wait_for_routing("shop")
    os.kill(before[0], signal.SIGSTOP)

    try:
        started = time.monotonic()
        lines = daemon.muructl("restart", "shop", "--worker", "0", "--force").stdout.splitlines()
        assert time.monotonic() - started < 20
        assert lines[0].endswith(", killed at once"), lines
        after = workers(daemon)
        assert after[0] != before[0], (before, after)
        assert daemon.get("/").status == 200
        log = daemon.log()
        assert "restarted on request, killed at once" in log, log
    finally:
        try:
            os.kill(before[0], signal.SIGCONT)
        except ProcessLookupError:
            pass


def test_all_restarts_every_worker_in_turn_by_one_call(daemon: Daemon) -> None:
    """Section 10.14: the release's host takes the turns, and the call
    waits, returns at once or is cancelled."""

    before = deploy_shop(daemon)

    with Load(daemon.port) as load:
        output = daemon.muructl("restart", "shop", "--all").stdout
        time.sleep(0.5)

    load.check("a rolling restart")
    after = workers(daemon)
    assert after[0] != before[0] and after[1] != before[1], (before, after)
    assert output.splitlines() == [
        "muru: 2 workers of shop@r1 marked to be replaced, each in its turn",
        "muru: the restart of shop@r1 has finished",
    ]

    events = daemon.muructl("events").stdout
    assert events.count("worker.restarted") == 2, events
    started, finished = events.index("restart.started"), events.index("restart.finished")
    assert started < events.index("worker.restarted") < finished, events
    assert "worker.swap_waiting" not in events, events

    # It returns at once when asked to, the release says how far it has
    # got, and a cancel leaves the worker not yet replaced as it was.

    asked = daemon.muructl("restart", "shop", "--all", "--no-wait", "--json").stdout
    answer = json.loads(asked)
    assert [worker["pid"] for worker in answer["workers"]] == [after[0], after[1]], answer
    assert answer["restart"]["total"] == 2, answer
    shown = daemon.muructl("release", "show", "shop@r1").stdout
    assert "of 2 workers still to be replaced, each in its turn" in shown, shown

    cancelled = daemon.muructl("restart", "shop", "--cancel").stdout
    assert cancelled == "muru: the restart of shop@r1 is cancelled: 1 worker left as it was\n"
    wait_for(lambda: "restart  " not in daemon.muructl("release", "show", "shop@r1").stdout)
    final = workers(daemon)
    assert final[0] != after[0] and final[1] == after[1], (after, final)
    assert "cancelled: 1 of the 2 marked workers" in daemon.muructl("events").stdout

    # With nothing under way a cancel says so, a group is one the
    # release has, and the restart is no debug operation.

    for arguments, expected in [
        (["restart", "shop", "--cancel"], "no restart is under way"),
        (["restart", "shop", "--group", "nosuch"], "no worker group nosuch"),
        (["restart", "shop", "--all", "--force"], "'--all' cannot be used with '--force'"),
    ]:
        refused = daemon.muructl(*arguments, check=False)
        assert refused.returncode != 0 and expected in refused.stderr, refused.stderr
