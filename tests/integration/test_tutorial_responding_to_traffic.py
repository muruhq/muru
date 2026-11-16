"""The steps of the conditions and overlays tutorial, with its own files.

docs/tutorials/responding-to-traffic.md includes the files under
docs/tutorials/code/crawlers, and this test runs them through the page's
steps, so a change that breaks one fails here rather than for a reader.
The condition's waits and windows and the overlays' minimum and cooldown
are shortened, and the load runs are shorter, to keep the test short; the
states and events are the same.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

from support import server as support
from support.daemon import Daemon, start_daemon

CRAWLERS = support.ROOT / "docs" / "tutorials" / "code" / "crawlers"

SHORTER = {
    "watch.toml": {"[10s]": "[5s]", '"10s"': '"3s"'},
    "respond.toml": {
        'min_active = "20s"': 'min_active = "3s"',
        'cooldown = "1m"': 'cooldown = "20s"',
    },
    "snapshot.toml": {
        'min_active = "20s"': 'min_active = "3s"',
        'cooldown = "1m"': 'cooldown = "20s"',
    },
    "capacity.toml": {
        'min_active = "20s"': 'min_active = "3s"',
        'cooldown = "1m"': 'cooldown = "20s"',
    },
}


def wait_for(condition: Callable[[], bool], timeout: float = 30, every: float = 0.5) -> None:
    deadline = time.monotonic() + timeout

    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(every)


def text(daemon: Daemon, *arguments: str, check: bool = True) -> str:
    completed = daemon.muructl(*arguments, check=check)
    return completed.stdout + completed.stderr


def test_the_responding_to_traffic_tutorial(tmp_path: Path) -> None:
    project = tmp_path / "crawlers"
    shutil.copytree(CRAWLERS, project)

    for name, changes in SHORTER.items():
        path = project / name
        content = path.read_text()

        for before, after in changes.items():
            assert before in content, (name, before)
            content = content.replace(before, after)

        path.write_text(content)

    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")
    url = f"http://127.0.0.1:{daemon.port}/"

    # Issue 731: each load sends the people's requests for longer than
    # any step needs, since a condition with no traffic has no figure and
    # holds its state; a step waits for what it shows while the load
    # runs, then stops the load and reads what it printed.

    loads: list[subprocess.Popen[str]] = []

    def load(crawl: int) -> subprocess.Popen[str]:
        running = subprocess.Popen(
            [sys.executable, str(project / "load.py"), "60", str(crawl), url],
            stdout=subprocess.PIPE,
            text=True,
        )
        loads.append(running)
        return running

    def stop(running: subprocess.Popen[str]) -> str:
        running.terminate()
        return running.communicate(timeout=10)[0]

    def show() -> str:
        return text(daemon, "condition", "show", "crawler-surge")

    def overlays() -> str:
        return text(daemon, "overlay", "list")

    try:
        # A shop with crawlers.

        assert daemon.deploy("shop", project) == "muru: release shop@r1 is ready and serving"
        applied = text(daemon, "apply", "-f", str(project / "site.toml"))
        assert "create-client-class client_class ai-crawler  created" in applied
        assert "create-route    route shop /             created" in applied

        # Watch a condition: unknown without traffic, pending, firing,
        # and resolved once the crawlers stop, with nothing else done.

        text(daemon, "apply", "-f", str(project / "watch.toml"))
        assert "crawler-surge unknown   -" in text(daemon, "condition", "list")
        running = load(8)
        wait_for(lambda: "state       pending: when has held for" in show(), every=0.25)
        assert "crawler-surge pending" in text(daemon, "condition", "list")
        wait_for(lambda: "state       firing since" in show())
        assert "is 0.6" in show()
        wait_for(lambda: "state       resolved since" in show())
        output = stop(running)
        assert " x 429" not in output, output
        events = text(daemon, "events")
        assert "condition.firing     crawler-surge            firing, when held for 3s" in events
        assert "condition.resolved   crawler-surge            resolved, clear for 3s" in events

        # Respond with an overlay: the crawlers' answers turn to 429s
        # while it is active, and it ends after the condition resolves.

        applied = text(daemon, "apply", "-f", str(project / "respond.toml"))
        assert "create-overlay  overlay crawler-response created" in applied
        assert "crawler-response         idle       condition crawler-surge" in overlays()
        running = load(8)
        wait_for(lambda: "active (condition) condition crawler-surge, firing" in overlays())
        wait_for(lambda: "cooldown until" in overlays())
        output = stop(running)
        last = output.splitlines()[-1]
        assert "x 429" in last.split("crawlers")[1], output
        assert "x 429" not in last.split("crawlers")[0], output
        events = text(daemon, "events")
        assert "overlay crawler-response activated: its condition crawler-surge is firing" in events
        assert "overlay crawler-response ended: its condition crawler-surge resolved" in events

        # A burst inside the cooldown is skipped, and the cooldown
        # outlasts it.

        running = load(6)
        wait_for(lambda: "overlay.skipped" in text(daemon, "events"))
        assert "its cooldown after it last ended lasts until" in text(daemon, "events")
        wait_for(lambda: "state       resolved since" in show())
        stop(running)
        assert text(daemon, "events").count("overlay crawler-response activated") == 1
        wait_for(lambda: "cooldown" not in overlays(), timeout=30)

        # A response that cannot see itself recover: without the latch,
        # apply warns; with it, the overlay outlasts its condition until
        # it is ended by hand.

        unlatched = project / "unlatched.toml"
        snapshot = (project / "snapshot.toml").read_text()
        unlatched.write_text(snapshot.replace("latch = true\n", ""))
        warned = text(daemon, "apply", "--dry-run", "-f", str(unlatched), check=False)
        assert (
            "muru: warning: overlay crawler-response takes shop's ai-crawler requests away"
            in warned
        ), warned
        applied = text(daemon, "apply", "-f", str(project / "snapshot.toml"))
        assert "warning" not in applied, applied
        running = load(60)
        wait_for(
            lambda: "active (condition) condition crawler-surge, latched, resolved" in overlays()
        )
        output = stop(running)
        assert " x 429" not in output, output
        assert "muru: overlay crawler-response ended by hand" in text(
            daemon, "overlay", "end", "crawler-response"
        )

        # Capacity on a condition, and a cap over it by hand.

        text(daemon, "apply", "-f", str(project / "respond.toml"))
        text(daemon, "apply", "-f", str(project / "capacity.toml"))
        running = load(30)
        wait_for(
            lambda: (
                "scheduled: 3-4 workers, no scale-in from crawler-capacity"
                in text(daemon, "scale", "status", "shop")
            )
        )
        wait_for(lambda: text(daemon, "scale", "status", "shop").splitlines()[2].split()[1] == "3")
        capped = text(daemon, "scale", "shop", "--cap", "2", "--for", "1m")
        assert "muru: shop default capped at 2 workers until" in capped
        assert "muru: shop default is at 2 workers" in capped
        status = text(daemon, "scale", "status", "shop")
        assert "scheduled: 2-2 workers, no scale-in from crawler-capacity" in status, status
        assert "capped: at most 2 workers from shop:cap" in status, status
        assert "muru: overlay shop:cap ended by hand" in text(daemon, "overlay", "end", "shop:cap")
        stop(running)
    finally:
        for running in loads:
            if running.poll() is None:
                running.kill()
                running.communicate()

        daemon.stop()
