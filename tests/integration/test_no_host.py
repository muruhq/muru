"""One application serves without hosts; a second is refused (specification section 4.3)."""

from __future__ import annotations

import json
import sqlite3
import sys
from pathlib import Path

from support.daemon import FACTORY, SHOP, Daemon, start_daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture

HOSTS = (
    "[host.shop]\nnames = ['shop.test']\n\n[[route]]\nhost = 'shop'\napp = 'shop'\n\n"
    "[host.other]\nnames = ['other.test']\n\n[[route]]\nhost = 'other'\napp = 'other'\n"
)


def release(app: str) -> str:
    """A declared first release of the shop project, for an application."""

    return (
        f"[app.{app}.release.r1]\nsource = '{SHOP}'\nentrypoint = 'shop.wsgi:application'\n"
        f"python = '{sys.executable}'\n"
    )


def deploy(daemon: Daemon, app: str, source: Path, *flags: str, check: bool = True) -> object:
    return daemon.muructl(
        "deploy", app, str(source), "--python", sys.executable, *flags, check=check
    )


def apply(daemon: Daemon, base: Path, text: str, *flags: str, check: bool = True) -> object:
    path = base / "site.toml"
    path.write_text(text)
    return daemon.muructl("apply", "-f", str(path), *flags, check=check)


def test_a_second_application_needs_hosts_or_hold(daemon: Daemon, tmp_path: Path) -> None:
    deploy(daemon, "shop", SHOP)
    assert daemon.request("/", host="anything").status == 200

    # A second application serving is refused before it starts, and the
    # plan says so too. The refusal carries the tables to paste.

    refused = deploy(daemon, "blog", FACTORY, check=False)
    assert refused.returncode == 1
    assert "every request goes to shop" in refused.stderr, refused.stderr
    assert '[host.blog]\nnames = ["blog.example.com"]' in refused.stderr, refused.stderr
    assert '[[route]]\nhost = "shop"\napp = "shop"' in refused.stderr, refused.stderr
    apps = json.loads(daemon.muructl("app", "list", "--json").stdout)["apps"]
    assert [app["name"] for app in apps if app.get("current_release")] == ["shop"], apps

    planned = deploy(daemon, "blog", FACTORY, "--dry-run", check=False)
    assert planned.returncode == 1 and "every request goes to shop" in planned.stdout, planned

    # Held it runs, and a promotion is refused in its turn.

    deploy(daemon, "blog", FACTORY, "--hold")
    assert daemon.request("/").status == 200
    promoted = daemon.muructl("rollout", "bluegreen", "blog@r1", check=False)
    assert promoted.returncode == 1 and "every request goes to shop" in promoted.stderr, promoted

    # An apply that would add a second serving application is refused, and
    # one that declares hosts for both is not; removing the hosts again is.

    second = release("other")
    refused = apply(daemon, tmp_path, second, check=False)
    assert refused.returncode == 1 and "with no hosts declared" in refused.stdout, refused.stdout

    apply(daemon, tmp_path, HOSTS + second)
    assert daemon.request("/", host="shop.test").status == 200

    stripped = apply(daemon, tmp_path, second + release("third"), "--prune", check=False)
    assert stripped.returncode == 1 and "no hosts declared" in stripped.stdout, stripped.stdout


def test_several_serving_without_hosts_is_answered_421(tmp_path: Path) -> None:
    # Hosts let both applications serve; a store changed by hand then
    # leaves no hosts, which a restart reads.

    first = start_daemon(tmp_path)

    try:
        apply(first, tmp_path, HOSTS + release("shop") + release("other"))
        assert first.request("/", host="other.test").status == 200
    finally:
        first.stop(shutdown=False)

    with sqlite3.connect(tmp_path / "state" / "muru.sqlite") as store:
        store.execute("DELETE FROM routes")
        store.execute("DELETE FROM hosts")

    second = start_daemon(tmp_path, run_dir=first.run_dir, port=first.port)

    try:
        answered = second.request("/", host="shop.test")
        assert answered.status == 421, answered.body
        assert b"no_host" in answered.body
        assert b"shop" not in answered.body and b"other" not in answered.body

        assert "routing.no_host" in second.events()
        status = second.muructl("status").stdout
        assert "other, shop serve with no hosts declared" in status, status
    finally:
        second.stop()
