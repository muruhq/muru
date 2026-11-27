"""The node budget manager (design specification sections 7.4, 7.5 and
13.6): the `[daemon.budget]` table caps the workers and memory the
releases hold between them, scale-ups are admitted against it with an
event for what it holds back, a release whose memory does not fit is not
started, and schedules are checked against it when applied and ahead of
their windows.
"""

from __future__ import annotations

import json
import sys
import threading
import time
from collections.abc import Callable
from datetime import UTC, datetime, timedelta
from pathlib import Path

from support.daemon import FACTORY, SHOP, Daemon, start_daemon


def shop(tmp_path: Path, extra: str = "", releases: tuple[str, ...] = ("r1",)) -> Path:
    """The shop with one thread per worker and a quick spare policy."""

    config = tmp_path / "shop.toml"
    tables = "".join(
        f"""[app.shop.release.{release}]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
threads = 1

[app.shop.release.{release}.scaling]
mode = 'spare'
min_workers = 1
max_workers = 4
scale_out_window = '1s'
hold_after_growth = '1s'

"""
        for release in releases
    )
    config.write_text(tables + extra)
    return config


def at(seconds: float) -> str:
    """A moment `seconds` from now, as an RFC 3339 time."""

    return (datetime.now(UTC) + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


def ready_workers(daemon: Daemon) -> int:
    lines = daemon.muructl("scale", "status", "shop").stdout.splitlines()
    row = next(line.split() for line in lines[2:] if line.startswith("default"))
    return int(row[1].rstrip("+"))


def wait_for_event(daemon: Daemon, kind: str, timeout: float = 30) -> list[str]:
    deadline = time.monotonic() + timeout
    found: list[str] = []

    while not found and time.monotonic() < deadline:
        found = [line for line in daemon.muructl("events").stdout.splitlines() if kind in line]

        if not found:
            time.sleep(0.5)

    return found


def wait_until(predicate: Callable[[], bool], timeout: float = 30) -> bool:
    deadline = time.monotonic() + timeout

    while not predicate():
        if time.monotonic() >= deadline:
            return False

        time.sleep(0.5)

    return True


def budget_line(daemon: Daemon, expected: str | None = None, timeout: float = 20) -> str:
    """What `daemon status` says the releases hold of the budget, once it
    says `expected` when given: the figures are taken once a second."""

    deadline = time.monotonic() + timeout

    while True:
        lines = daemon.muructl("daemon", "status").stdout.splitlines()
        line = next((line for line in lines if line.startswith("budget")), "")

        if expected is None or expected in line or time.monotonic() >= deadline:
            return line

        time.sleep(0.5)


def test_scale_ups_are_admitted_within_the_worker_budget(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra="[daemon.budget]\nworkers = 3\n")
    stopping = threading.Event()

    def load() -> None:
        while not stopping.is_set():
            try:
                daemon.get("/slow?ms=400")
            except OSError:
                time.sleep(0.1)

    threads = [threading.Thread(target=load, daemon=True) for _ in range(4)]

    try:
        config = shop(tmp_path, "[app.shop]\npriority = 5\n")
        applied = daemon.muructl("apply", "-f", str(config)).stdout
        assert "set-priority" in applied, applied
        assert "[priority 5]" in daemon.muructl("app", "list").stdout

        # Section 7.9: the one worker and the room held to replace it.

        held = "budget   2 of 3 workers, 1 worker held for swaps"
        assert budget_line(daemon, "2 of 3") == held

        # Four requests in flight against one single-threaded worker: the
        # group asks for more than the node has room for beside what it
        # holds for swaps, and is given what fits.

        for thread in threads:
            thread.start()

        denied = wait_for_event(daemon, "scaling.denied_by_budget")
        assert denied, daemon.muructl("scale", "status", "shop").stdout
        assert "1 of the 3 more workers asked for fit the node's worker budget" in denied[0]
        assert "at priority 5" in denied[0]

        time.sleep(4)
        assert ready_workers(daemon) == 2
        assert budget_line(daemon, "3 of 3") == "budget   3 of 3 workers, 1 worker held for swaps"
        assert "held at 2" in daemon.muructl("scale", "status", "shop").stdout

        # A floor beyond the budget is reported when applied, and once
        # ahead of its window.

        schedule = f"""[app.shop]
priority = 5

[[app.shop.schedule]]
name = 'busy'
start = '{at(120)}'
end = '{at(600)}'
min_workers = 4
"""
        applied = daemon.muructl("apply", "-f", str(shop(tmp_path, schedule))).stdout
        assert "warning: schedule shop.busy: a floor of 4 workers for shop" in applied, applied
        assert "the node's worker budget has room for 0" in applied, applied

        ahead = wait_for_event(daemon, "schedule.over_budget")
        assert ahead and "a floor of 4 workers for shop (default group)" in ahead[0], ahead
    finally:
        stopping.set()

        for thread in threads:
            if thread.is_alive():
                thread.join(timeout=10)

        daemon.stop()


def test_the_budget_holds_room_for_swaps_and_a_recycle_waits_without_it(tmp_path: Path) -> None:
    """Section 7.9: a group's surge allowance counts as held, is withdrawn
    when the budget has no room and for a release that takes no traffic,
    and a recycle waits for it when nothing may stop first."""

    daemon = start_daemon(tmp_path, config_extra="[daemon.budget]\nworkers = 3\n")

    try:
        config = tmp_path / "shop.toml"
        config.write_text(
            f"[app.shop.release.r1]\nsource = '{SHOP}'\npython = '{sys.executable}'\n"
            "processes = 2\nthreads = 1\nmax_requests = 20\n"
        )
        assert daemon.muructl("apply", "-f", str(config)).returncode == 0
        held = "budget   3 of 3 workers, 1 worker held for swaps"
        assert budget_line(daemon, "held for swaps") == held
        assert "surge allowance" not in daemon.muructl("release", "show", "shop@r1").stdout

        # A third worker takes the room the allowance held. The workers
        # come due for a recycle and wait, since max_unavailable is 0.

        daemon.muructl("scale", "shop", "--processes", "3")
        assert wait_until(lambda: budget_line(daemon) == "budget   3 of 3 workers")
        shown = daemon.muructl("release", "show", "shop@r1").stdout
        assert "no surge allowance now, since the node's worker budget has no room" in shown

        for _ in range(90):
            assert daemon.get("/").status == 200

        waits = wait_for_event(daemon, "worker.swap_waiting")
        assert waits, daemon.muructl("events").stdout
        assert "the node's worker budget has no room for another worker" in waits[0]
        assert not wait_for_event(daemon, "worker.recycled", timeout=3)

        # Back at two workers the allowance is granted again, and the
        # recycles that waited go ahead, one at a time.

        daemon.muructl("scale", "shop", "--processes", "2")
        assert wait_for_event(daemon, "worker.recycled")
        assert budget_line(daemon, "held for swaps") == held

        # A held release takes no traffic, so it is granted no allowance.

        daemon.deploy("shop", SHOP, "--hold", "--processes", "1")
        assert wait_until(
            lambda: (
                "no surge allowance now, since the release takes no traffic"
                in daemon.muructl("release", "show", "shop@r2").stdout
            )
        ), daemon.muructl("release", "show", "shop@r2").stdout
    finally:
        daemon.stop()


def test_a_release_whose_memory_does_not_fit_is_not_started(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, config_extra="[daemon.budget]\nmemory = '1MB'\n")

    try:
        # Nothing is known of the shop's memory before it runs, so the
        # first release starts.

        assert daemon.muructl("apply", "-f", str(shop(tmp_path))).returncode == 0

        deadline = time.monotonic() + 20

        while budget_line(daemon).startswith("budget   0B") and time.monotonic() < deadline:
            time.sleep(0.5)

        held = budget_line(daemon)
        assert held.endswith("of 1.0MB memory") and not held.startswith("budget   0B"), held

        # Measured, its next release is over the budget: apply warns, and
        # the deploy is refused without a record.

        planned = daemon.muructl(
            "apply", "-f", str(shop(tmp_path, releases=("r1", "r2"))), check=False
        )
        assert planned.returncode == 1, planned.stdout + planned.stderr
        assert "warning: shop@r2: the release needs about" in planned.stdout, planned.stdout
        assert "so its deploy would be refused" in planned.stdout
        assert "not started: shop: the release needs about" in planned.stdout

        releases = daemon.muructl("release", "list", "shop").stdout
        assert "r2" not in releases, releases
        assert wait_for_event(daemon, "scaling.denied_by_budget")
    finally:
        daemon.stop()


def workers_of(daemon: Daemon, release: str) -> str:
    """The WORKERS column of the default group of `shop@release`."""

    lines = daemon.muructl("scale", "status", f"shop@{release}").stdout.splitlines()
    fields = next(line for line in lines[2:] if line.startswith("default")).split()
    slots = next(index for index, field in enumerate(fields) if "/" in field)
    return " ".join(fields[1:slots])


def test_a_release_starts_small_to_fit_the_budget_and_takes_its_count_once_it_serves(
    tmp_path: Path,
) -> None:
    """Issue 964: `--initial-processes` is the count the budget checks a
    deploy at and the count the release keeps while it takes no traffic."""

    # Room for six workers at 200MB and for the two hosts, which are
    # counted at their own memory where it is measured.

    daemon = start_daemon(tmp_path, config_extra="[daemon.budget]\nmemory = '1300MB'\n")
    declared = ["--processes", "4", "--memory", "hard_limit=200MB"]

    def held() -> int:
        """The megabytes `daemon status` says the releases hold."""

        figure = [*budget_line(daemon).split(), "", "0B"][1]
        units = {"GB": 1024.0, "MB": 1.0, "KB": 1 / 1024, "B": 1 / 1024 / 1024}
        unit = next(unit for unit in units if figure.endswith(unit))
        return int(float(figure.removesuffix(unit)) * units[unit])

    try:
        # Four workers at their hard limit and the room held to replace
        # one: a second copy at full size does not fit, and neither does
        # an initial count over the declared one make sense.

        assert (
            daemon.deploy("shop", SHOP, *declared) == "muru: release shop@r1 is ready and serving"
        )
        assert wait_until(lambda: 1000 <= held() < 1100), budget_line(daemon)
        assert "of 1.3GB memory, 1 worker held for swaps" in budget_line(daemon)
        refused = daemon.muructl("deploy", "shop", str(SHOP), *declared, check=False)
        assert "the release needs about" in refused.stderr, refused.stderr
        assert "the node's memory budget of 1.3GB has" in refused.stderr, refused.stderr
        over = daemon.muructl(
            "deploy", "shop", str(SHOP), *declared, "--initial-processes", "5", check=False
        )
        assert "5 initial processes are more than the 4 the release is declared with" in (
            over.stderr
        ), over.stderr

        # With one worker fewer in the serving release, the new one fits
        # at one worker, declared at four.

        daemon.muructl("scale", "shop", "--processes", "3")
        assert wait_until(lambda: 800 <= held() < 900), budget_line(daemon)
        small = daemon.deploy("shop", SHOP, *declared, "--hold", "--initial-processes", "1")
        assert small.splitlines()[0] == "muru: release shop@r2 is ready and held: not serving"
        shown = daemon.muructl("release", "show", "shop@r2").stdout
        assert "4 processes of 3 threads; 1 to start with and while it takes no traffic" in shown
        described = json.loads(daemon.muructl("release", "show", "shop@r2", "--json").stdout)
        assert (described["processes"], described["initial_processes"]) == (4, 1), described

        # It stays there while it takes no traffic. A pin moves it, and
        # lifting the pin brings it back.

        time.sleep(3)
        assert workers_of(daemon, "r2") == "1"
        daemon.muructl("scale", "shop@r2", "--processes", "2")
        assert wait_until(lambda: workers_of(daemon, "r2") == "2 pinned")
        daemon.muructl("scale", "shop@r2", "--unpin")
        assert wait_until(lambda: workers_of(daemon, "r2") == "1")
        status = daemon.muructl("scale", "status", "shop@r2").stdout
        assert "held at its initial count of 1: the release takes no traffic" in status, status

        # Given the traffic, it takes the count it is declared with, as
        # the budget admits it, with no pin holding it there.

        daemon.muructl("rollout", "bluegreen", "shop@r2")
        assert wait_until(lambda: workers_of(daemon, "r2") == "4"), daemon.muructl("events").stdout
        events = daemon.muructl("events").stdout
        assert "back toward the static count of 4" in events, events
    finally:
        daemon.stop()


def scaled_to_zero(daemon: Daemon) -> bool:
    lines = daemon.muructl("scale", "status", "shop").stdout.splitlines()
    return any(line.split()[:2] == ["default", "0"] for line in lines[2:])


def test_a_wake_from_zero_is_not_held_back_by_the_worker_budget(tmp_path: Path) -> None:
    # The node allows two workers: the factory's serving release and its
    # retained one hold them while the shop is at zero. Each wake of the
    # shop, by a fork while its host stays and then by a cold start once
    # its host has stopped, takes the node over the limit, and the
    # factory's retained release stops to make room.

    daemon = start_daemon(
        tmp_path,
        config_extra="[limits]\nqueue_timeout = '30s'\n\n[daemon.budget]\nworkers = 2\n",
    )
    config = tmp_path / "apps.toml"
    config.write_text(
        f"""[app.shop.release.r1]
source = '{SHOP}'
python = '{sys.executable}'
processes = 1
threads = 1

[app.shop.release.r1.scaling]
mode = 'spare'
min_workers = 0
max_workers = 2
idle_reap_after = '2s'
scale_in_window = '2s'
hold_after_growth = '1s'
reap_interval = '1s'
min_lifetime = '1s'
host_idle_timeout = '6s'

[app.factory.release.r1]
source = '{FACTORY}'
python = '{sys.executable}'
processes = 1
threads = 1

[host.shop]
names = ["shop.test"]
default = true

[host.factory]
names = ["factory.test"]

[[route]]
host = "shop"
app = "shop"

[[route]]
host = "factory"
app = "factory"
"""
    )

    def retained_stops(release: str) -> bool:
        # The release's row, then its note on the line below.

        lines = daemon.muructl("release", "list", "factory").stdout.splitlines()
        row = next((index for index, line in enumerate(lines) if line.startswith(release)), None)

        if row is None:
            return False

        shown = " ".join(lines[row : row + 2])
        return "stopped" in shown and "the node's worker budget for shop@r1" in shown

    try:
        assert daemon.muructl("apply", "-f", str(config)).returncode == 0
        assert "is ready and serving" in daemon.deploy("factory", FACTORY)
        assert wait_until(lambda: scaled_to_zero(daemon)), daemon.muructl(
            "scale", "status", "shop"
        ).stdout
        assert budget_line(daemon, "2 of 2") == "budget   2 of 2 workers"

        # The warm wake forks a worker over the limit.

        assert daemon.request("/", host="shop.test").status == 200
        woken = wait_for_event(daemon, "over the node's budget of 2 workers")
        assert woken and "scaling.decision" in woken[0], woken
        assert wait_until(lambda: retained_stops("factory@r1")), daemon.muructl(
            "release", "list", "factory"
        ).stdout

        # The cold wake starts the stopped host over the limit.

        assert "is ready and serving" in daemon.deploy("factory", FACTORY)
        assert wait_until(lambda: bool(wait_for_event(daemon, "release.idle", 1)), timeout=40)
        assert daemon.request("/", host="shop.test").status == 200
        woken = wait_for_event(daemon, "release.woken")
        assert woken and "over the node's budget of 2 workers" in woken[0], woken
        assert wait_until(lambda: retained_stops("factory@r2")), daemon.muructl(
            "release", "list", "factory"
        ).stdout
    finally:
        daemon.stop()
