"""Overlays (design specification section 9.10): named partial
configuration laid over routes and applications while active, by hand or
by a schedule's window, with preconditions, previews and promotion. The
base configuration is never changed until an overlay is promoted.

One daemon serves every test here, with two releases of an application
that reports the release and mode it served with, behind a named route.
"""

from __future__ import annotations

import http.client
import json
import shutil
import sys
import time
import tomllib
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from support import server as support
from support.daemon import Daemon, start_daemon

FIXTURE = support.ROOT / "tests" / "fixtures" / "projects" / "clients"


@dataclass
class Site:
    daemon: Daemon
    base: Path
    project: Path

    def request(self, method: str = "GET") -> support.Response:
        connection = http.client.HTTPConnection("127.0.0.1", self.daemon.port, timeout=30)

        try:
            connection.request(method, "/")
            raw = connection.getresponse()
            return support.Response(raw.status, raw.reason, raw.getheaders(), raw.read())
        finally:
            connection.close()

    def seen(self) -> dict[str, str | None]:
        response = self.request()
        assert response.status == 200, response.body
        return json.loads(response.body)

    def overlay(self, *arguments: str, check: bool = True) -> str:
        completed = self.daemon.muructl("overlay", *arguments, check=check)
        return completed.stdout + completed.stderr

    def apply(self, overlays: str, check: bool = True, prune: bool = False) -> str:
        config = self.base / "site.toml"
        config.write_text(
            f"""[app.clients.release.r1]
source = '{self.project}'
python = '{sys.executable}'
processes = 1

[app.clients.release.r2]
source = '{self.project}'
python = '{sys.executable}'
processes = 1

[host.site]
names = ['site.test']
listeners = ['127.0.0.1:{self.daemon.port}']
default = true

[[route]]
name = 'root'
host = 'site'
path = '/'
app = 'clients'
"""
            + overlays
        )
        prune_flag = ["--prune"] if prune else []
        completed = self.daemon.muructl("apply", *prune_flag, "-f", str(config), check=check)
        return completed.stdout + completed.stderr

    def state(self, release: str) -> str:
        return next(
            row["state"]
            for row in self.daemon.releases("clients")
            if row["release"] == f"clients@{release}"
        )


def eventually(check: Callable[[], bool], timeout: float = 15.0) -> bool:
    deadline = time.monotonic() + timeout

    while time.monotonic() < deadline:
        if check():
            return True

        time.sleep(0.2)

    return False


def at(seconds: float) -> str:
    """A moment `seconds` from now, as an RFC 3339 time."""

    return (datetime.now(UTC) + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


OVERLAYS = """
[overlay.read-only]
routes.root = { mode = 'read-only', deny = ['POST', 'PUT'] }

[overlay.pinned]
routes.root.traffic = { pin = 'r1' }
preconditions = { release_ready = 'r1' }
priority = 2

[overlay.broken]
routes.root.traffic = { split = { r9 = 50 } }
preconditions = { release_ready = 'r9' }
priority = 1
"""


@pytest.fixture(scope="module")
def site(tmp_path_factory: pytest.TempPathFactory) -> Iterator[Site]:
    base = tmp_path_factory.mktemp("overlays")
    project = base / "project"
    shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
    (project / "muru.toml").write_text('entrypoint = "clients:wsgi_app"\n')
    daemon = start_daemon(base)

    try:
        site = Site(daemon, base, project)
        site.apply(OVERLAYS)
        daemon.muructl("release", "wait", "clients@r2", "--timeout", "60")
        assert eventually(lambda: site.seen()["release"] == "r2")
        yield site
    finally:
        daemon.stop()


def test_overlays_are_listed_idle_until_activated(site: Site) -> None:
    listed = site.overlay("list")
    assert "read-only" in listed and "idle" in listed
    assert "route root" in listed
    assert "by hand" in listed


def test_an_overlay_applies_while_active_and_the_base_shows_through_after(site: Site) -> None:
    assert site.seen()["mode"] is None
    preview = site.overlay("preview", "read-only")
    assert "route root: mode (none) -> read-only" in preview
    assert "route root: methods.deny (none) -> POST, PUT" in preview

    assert "activated by hand" in site.overlay("activate", "read-only")
    assert site.seen()["mode"] == "read-only"
    refused = site.request("POST")
    assert refused.status == 503
    assert b"read_only" in refused.body
    assert "active (manual)" in site.overlay("list")
    assert "already active" in site.overlay("activate", "read-only", check=False)

    assert "ended by hand" in site.overlay("end", "read-only")
    assert site.seen()["mode"] is None
    assert site.request("POST").status == 200
    assert "overlay.activated" in site.daemon.events()
    assert "overlay.ended" in site.daemon.events()


def test_an_activation_for_a_time_ends_on_its_own(site: Site) -> None:
    site.overlay("activate", "read-only", "--for", "2s")
    assert site.seen()["mode"] == "read-only"
    assert eventually(lambda: site.seen()["mode"] is None)


def test_a_pin_sends_every_request_to_one_release(site: Site) -> None:
    preview = site.overlay("preview", "pinned")
    assert "route root: traffic.split (none) -> r1 100%" in preview

    site.overlay("activate", "pinned")
    assert eventually(lambda: site.seen()["release"] == "r1")
    assert all(site.seen()["release"] == "r1" for _ in range(10))

    site.overlay("end", "pinned")
    assert eventually(lambda: site.seen()["release"] == "r2")


def test_failing_preconditions_keep_an_overlay_from_activating(site: Site) -> None:
    refused = site.overlay("activate", "broken", check=False)
    assert "was not activated: clients@r9 does not exist" in refused
    assert site.seen()["release"] == "r2"


def test_a_schedule_activates_and_ends_an_overlay_in_its_window(site: Site) -> None:
    window = f"""
[overlay.window]
apps.clients = {{ maintenance = true }}
trigger = {{ schedule = {{ start = '{at(3)}', end = '{at(7)}' }} }}
"""
    site.apply(OVERLAYS + window)
    assert "from " in site.overlay("list")
    assert site.request().status == 200
    assert eventually(lambda: site.request().status == 503)
    assert b"maintenance" in site.request().body
    assert "[maintenance by overlay window]" in site.daemon.muructl("app", "list").stdout
    assert eventually(lambda: site.request().status == 200)
    assert "[maintenance" not in site.daemon.muructl("app", "list").stdout

    events = site.daemon.muructl("events").stdout
    assert "its schedule's window began" in events
    assert "ended: its schedule" in events


def test_config_show_effective_attributes_values_to_overlays(site: Site) -> None:
    site.apply(OVERLAYS, prune=True)
    shown = site.daemon.muructl("config", "show", "--effective").stdout
    assert "No active overlay" in shown

    # Each serving release's settings say which layer they came from.

    assert "#   clients@r2: entrypoint = clients:wsgi_app ([tool.muru])" in shown
    assert "#   clients@r2: processes = 1 (the release table)" in shown
    assert "#   clients@r2: threads = 4 (default)" in shown

    site.overlay("activate", "read-only")
    shown = site.daemon.muructl("config", "show", "--effective").stdout
    assert "#   route root: mode = read-only (overlay read-only)" in shown
    assert "#   route root: methods.deny = POST, PUT (overlay read-only)" in shown
    effective = tomllib.loads(shown)
    assert effective["route"][0]["mode"] == "read-only"
    assert "overlay" not in effective

    # The base is unchanged.

    base = tomllib.loads(site.daemon.muructl("config", "show").stdout)
    assert "mode" not in base["route"][0]
    assert "route root" in site.daemon.muructl("config", "show", "clients", "--effective").stdout
    site.overlay("end", "read-only")


def test_an_overlay_chooses_a_routes_error_page(site: Site) -> None:
    upgrade = """
[overlay.upgrade]
apps.clients = { maintenance = true }
routes.root.error_pages = { '503:maintenance' = { template = '<p>Upgrading, back soon</p>' } }
"""
    site.apply(OVERLAYS + upgrade)
    preview = site.overlay("preview", "upgrade")
    assert "route root: error_pages.503:maintenance (none) -> a template" in preview

    site.overlay("activate", "upgrade")
    assert eventually(lambda: site.request().status == 503)
    assert site.request().body == b"<p>Upgrading, back soon</p>"
    shown = site.daemon.muructl("config", "show", "--effective").stdout
    assert "route root: error_pages.503:maintenance = a template (overlay upgrade)" in shown

    site.overlay("end", "upgrade")
    assert eventually(lambda: site.request().status == 200)
    site.apply(OVERLAYS, prune=True)


def test_an_overlays_page_must_exist_and_a_refused_activation_does_not_stand(site: Site) -> None:
    # Issue 779: the page is relative to the applied file's directory.

    page = site.base / "pages" / "down.html"
    page.parent.mkdir(exist_ok=True)
    page.unlink(missing_ok=True)

    def down(trigger: str = "") -> str:
        return (
            OVERLAYS
            + "\n[overlay.down]\napps.clients = { maintenance = true }\n"
            + "routes.root.error_pages = { '503:maintenance' = { file = 'pages/down.html' } }\n"
            + trigger
        )

    refused = site.apply(down(), check=False)
    assert f"overlay down: routes.root.error_pages.503:maintenance: {page} is not a file" in (
        refused
    ), refused

    # A page gone after the apply: the edge refuses the snapshot, and the
    # overlay is not recorded as active.

    page.write_text("<p>Down</p>")
    site.apply(down())
    page.unlink()
    refused = site.overlay("activate", "down", check=False)
    assert "overlay down was not activated: the edge refused the snapshot" in refused, refused
    assert "maintenance" not in site.daemon.muructl("app", "list").stdout
    assert site.request().status == 200

    # By its schedule, it is skipped, saying why.

    page.write_text("<p>Down</p>")
    site.apply(down(f"trigger = {{ schedule = {{ start = '{at(2)}' }} }}\n"))
    page.unlink()
    assert eventually(lambda: "overlay.skipped" in site.daemon.muructl("events").stdout)
    events = site.daemon.muructl("events").stdout
    assert "not activated: the edge refused the snapshot" in events, events
    assert not [
        line for line in events.splitlines() if "overlay.activated" in line and "down" in line
    ], events
    assert "skipped" in site.overlay("list")
    assert site.request().status == 200
    site.apply(OVERLAYS, prune=True)


def test_an_applications_mode_reaches_each_of_its_routes(site: Site) -> None:
    backup = """
[overlay.backup]
apps.clients = { mode = 'backup', deny = ['POST'], answer = 'method_not_allowed' }
"""
    site.apply(OVERLAYS + backup)
    preview = site.overlay("preview", "backup")
    assert "app clients: mode (none) -> backup" in preview

    site.overlay("activate", "backup")
    assert site.seen()["mode"] == "backup"
    refused = site.request("POST")
    assert refused.status == 405
    assert (
        "app clients: mode = backup (overlay backup)"
        in site.daemon.muructl("config", "show", "--effective").stdout
    )

    site.overlay("end", "backup")
    assert site.seen()["mode"] is None
    assert site.request("POST").status == 200
    site.apply(OVERLAYS, prune=True)


def test_a_recurring_schedule_covers_each_window(site: Site) -> None:
    every_minute = """
[overlay.always]
routes.root = { mode = 'recurring' }
trigger = { schedule = { cron = '* * * * *', duration = '1m', timezone = 'Australia/Sydney' } }
"""
    site.apply(every_minute, prune=True)
    assert "cron '* * * * *' for 1m Australia/Sydney" in site.overlay("list")
    assert eventually(lambda: site.seen()["mode"] == "recurring")

    # Ended by hand, it stays ended for the rest of the minute's window.

    site.overlay("end", "always")
    assert site.seen()["mode"] is None
    assert "ended until" in site.overlay("list")


def test_advisory_and_latched_schedules(site: Site) -> None:
    windows = f"""
[overlay.advised]
routes.root = {{ mode = 'advised' }}
trigger = {{ schedule = {{ start = '{at(2)}', end = '{at(30)}' }} }}
advisory = true

[overlay.latched]
apps.clients = {{ maintenance = true }}
trigger = {{ schedule = {{ start = '{at(2)}', end = '{at(4)}' }} }}
latch = true
"""
    site.apply(windows, prune=True)
    listed = site.overlay("list")
    assert "advisory" in listed and "latched" in listed

    # The latched overlay stays past its window's end; the advisory one
    # only says it would have activated.

    assert eventually(lambda: site.request().status == 503)
    time.sleep(4)
    assert site.request().status == 503
    listed = site.overlay("list")
    assert "active (schedule)" in listed
    assert "advised until" in listed
    events = site.daemon.muructl("events").stdout
    assert "would activate: its schedule's window began; it is advisory" in events

    site.overlay("end", "latched")
    assert site.seen()["mode"] is None


def test_a_change_clears_a_skip_so_the_new_window_judges_it_again(site: Site) -> None:
    # Issue 755: a window with no end skips for good, until an apply
    # changes the overlay.

    def retry(start: str, release: str) -> str:
        return f"""
[overlay.retry]
routes.root = {{ mode = 'retried' }}
trigger = {{ schedule = {{ start = '{start}' }} }}
preconditions = {{ release_ready = '{release}' }}
"""

    site.apply(retry(at(1), "r9"), prune=True)
    assert eventually(lambda: "skipped (activate it by hand" in site.overlay("list"))

    applied = site.apply(retry(at(2), "r2"), prune=True)
    assert "its skip is cleared" in applied
    assert eventually(lambda: site.seen()["mode"] == "retried")
    assert "active (schedule)" in site.overlay("list")

    site.overlay("end", "retry")
    assert site.seen()["mode"] is None


def test_warm_up_starts_a_release_ahead_of_its_window(site: Site) -> None:
    # The apply holds r1 beside r2, which serves, so it is stopped first
    # for the warm-up to start.

    site.apply(OVERLAYS, prune=True)
    site.daemon.muructl("drain", "clients@r1")
    assert site.state("r1") == "stopped"

    cutover = f"""
[overlay.cutover]
routes.root.traffic = {{ pin = 'r1' }}
trigger = {{ schedule = {{ start = '{at(3600)}' }} }}
warm_up = {{ lead = '2h', start_release = 'r1' }}
"""
    site.apply(cutover, prune=True)
    assert eventually(lambda: site.state("r1") == "ready", timeout=30)
    assert site.seen()["release"] == "r2"
    events = site.daemon.muructl("events").stdout
    assert "warming up: its window begins in 1h" in events
    assert "starting clients@r1" in events

    # Without the overlay nothing keeps it running.

    site.apply(OVERLAYS, prune=True)
    assert eventually(lambda: site.state("r1") == "retained", timeout=30)


def test_promotion_makes_an_overlay_the_base(site: Site) -> None:
    site.apply(OVERLAYS)
    site.overlay("activate", "read-only")
    assert "promoted" in site.overlay("promote", "read-only")
    assert site.seen()["mode"] == "read-only"
    assert "promoted" in site.overlay("list")

    exported = site.daemon.muructl("config", "export").stdout
    assert 'mode = "read-only"' in exported
    assert "overlay.promoted" in site.daemon.events()

    # Issue 776: it is not activated again, and stays promoted, until an
    # apply changes it.

    refused = site.overlay("activate", "read-only", check=False)
    assert "read-only was promoted" in refused, refused
    assert "promoted" in site.overlay("list")

    changed = site.apply(OVERLAYS.replace("deny = ['POST', 'PUT']", "deny = ['POST']"))
    assert "its promotion is cleared" in changed, changed
    assert "activated" in site.overlay("activate", "read-only")
    site.overlay("end", "read-only")


def test_apply_refuses_overlays_that_do_not_fit(site: Site) -> None:
    refused = site.apply(
        OVERLAYS
        + """
[overlay.clash]
routes.root = { mode = 'limited' }

[overlay.nowhere]
routes.missing = { mode = 'x' }
apps.elsewhere = { maintenance = true }
""",
        check=False,
    )
    assert "overlays clash, read-only all set routes.root.mode" in refused
    assert "overlay nowhere: no route is named missing" in refused
    assert "overlay nowhere: no application named elsewhere" in refused


def test_promoting_a_pin_makes_the_release_serve(site: Site) -> None:
    # Issue 759: last, since it changes the serving release. While the
    # pin is active, release list says where the route's traffic goes.

    site.apply(OVERLAYS)
    site.overlay("activate", "pinned")
    assert eventually(lambda: site.seen()["release"] == "r1")
    listed = site.daemon.muructl("release", "list", "clients").stdout
    assert "traffic: root 100% (overlay pinned)" in listed, listed
    assert "traffic: none, root sends 100% to r1 (overlay pinned)" in listed, listed
    assert "held: not serving" not in listed, listed

    promoted = site.overlay("promote", "pinned")
    assert "clients@r1 serves in place of clients@r2, which is retained" in promoted, promoted
    assert '[app.clients]\nserving = "r1"' in promoted, promoted
    assert {row["release"] for row in site.daemon.releases("clients") if row["serving"]} == {
        "clients@r1"
    }
    assert "traffic:" not in site.daemon.muructl("release", "list", "clients").stdout

    # The route has no pin of its own, so the files, which name no
    # serving release, keep the promotion when applied again.

    exported = tomllib.loads(site.daemon.muructl("config", "export").stdout)
    assert [route.get("traffic") for route in exported["route"]] == [None]
    site.apply(OVERLAYS)
    assert site.seen()["release"] == "r1"
