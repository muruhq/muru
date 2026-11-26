"""Named deployments (issue 936): a `deployment` key names a set of
objects applied, pruned and removed together, with `default` for files
without one and for what commands make."""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path

from support import server as support
from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def release(app: str, threads: int = 1) -> str:
    return (
        f"[app.{app}.release.r1]\nsource = '{SHOP}'\npython = '{sys.executable}'\n"
        f"threads = {threads}\n\n"
    )


SHOP_FILE = (
    "deployment = 'shop'\n\n"
    + release("shop")
    + "[host.shop]\nnames = ['shop.test']\ndefault = true\n\n"
    + "[[route]]\nhost = 'shop'\napp = 'shop'\n"
)
API_FILE = (
    "deployment = 'api'\n\n"
    + release("api")
    + "[[route]]\nhost = 'shop'\npath = '/api'\napp = 'api'\n"
)


def said(completed: subprocess.CompletedProcess[str]) -> str:
    return completed.stdout + completed.stderr


def test_deployments_own_what_they_declare(daemon: Daemon, tmp_path: Path) -> None:
    files = tmp_path / "deploy.d"
    files.mkdir()
    (files / "shop.toml").write_text(SHOP_FILE)
    (files / "api.toml").write_text(API_FILE)

    def apply(*arguments: str) -> subprocess.CompletedProcess[str]:
        return daemon.muructl("apply", *arguments, check=False)

    def deployments() -> dict[str, list[str]]:
        listed = json.loads(daemon.muructl("deployment", "list", "--json").stdout)
        return {entry["name"]: entry.get("apps", []) for entry in listed["deployments"]}

    # What a command makes is default's, and one apply carries several
    # deployments, a route in one naming a host another adds.

    daemon.deploy("landing", SHOP)
    applied = apply("-f", str(files))
    assert applied.returncode == 0, said(applied)
    assert deployments() == {"api": ["api"], "default": ["landing"], "shop": ["shop"]}

    # Only its owner changes an object.

    stolen = tmp_path / "stolen.toml"
    stolen.write_text(SHOP_FILE + release("api", threads=2))
    refused = apply("-f", str(stolen), "--dry-run")
    assert refused.returncode == 1
    assert (
        "app api belongs to the deployment api, and an apply of the deployment shop does not "
        "change it"
    ) in said(refused), said(refused)

    # Nothing is removed while another deployment names it: api's route
    # is on shop's host.

    hostless = tmp_path / "hostless.toml"
    hostless.write_text("deployment = 'shop'\n\n" + release("shop"))
    kept = apply("-f", str(hostless), "--prune", "--dry-run")
    assert kept.returncode == 1
    assert ("host shop is not removed: route shop /api of another deployment names it") in said(
        kept
    ), said(kept)

    # A prune reaches the deployments the files name and no others, and
    # with =all every deployment, default among them.

    own = apply("-f", str(files / "api.toml"), "--prune", "--dry-run")
    assert own.returncode == 0 and "no changes" in own.stdout, said(own)
    everything = apply("-f", str(files / "api.toml"), "--prune=all", "--dry-run")
    assert everything.returncode == 2, said(everything)
    removed = {line.split()[1] for line in everything.stdout.splitlines() if "remove-app" in line}
    assert removed == {"landing", "shop"}, everything.stdout

    # A named deployment's settings are its files': app set is refused
    # there, and works on what a command made.

    theirs = daemon.muructl("app", "set", "api", "--processes", "2", check=False)
    assert theirs.returncode != 0
    assert (
        "api belongs to the deployment api, whose files state its settings: change [app.api] "
        "there and apply"
    ) in theirs.stderr, theirs.stderr
    ours = daemon.muructl("app", "set", "landing", "--env", "MODE=live")
    assert "warning" not in ours.stderr, ours.stderr

    # A named deployment takes an object from default by declaring it,
    # which the plan says, and the object is then its own.

    site = tmp_path / "site.toml"
    site.write_text(
        "deployment = 'site'\n\n[app.landing]\nserving = 'r1'\n\n[app.landing.env]\nMODE = 'live'\n"
    )
    taken = apply("-f", str(site))
    assert taken.returncode == 0, said(taken)
    assert "app landing moves from the deployment default to site" in said(taken), said(taken)
    assert deployments() == {"api": ["api"], "default": [], "shop": ["shop"], "site": ["landing"]}
    assert daemon.muructl("app", "set", "landing", "--processes", "2", check=False).returncode != 0

    # A deployment is removed as a unit, after the ones that name it.

    def remove(*arguments: str) -> subprocess.CompletedProcess[str]:
        return daemon.muructl("deployment", "remove", *arguments, check=False)

    blocked = remove("shop", "--yes")
    assert blocked.returncode == 1 and "of another deployment names it" in said(blocked), said(
        blocked
    )
    assert remove("nowhere", "--yes").returncode != 0
    listed = remove("api", "--dry-run")
    assert listed.returncode == 0 and "remove-app" in listed.stdout, said(listed)
    assert "api" in deployments()

    assert remove("api", "--yes").returncode == 0
    assert remove("shop", "--yes").returncode == 0
    assert deployments() == {"default": [], "site": ["landing"]}
    assert "holds nothing" in remove("default", "--yes").stdout


def test_render_writes_the_deployment_and_a_daemons_file_takes_none(
    daemon: Daemon, tmp_path: Path
) -> None:
    muructl = support.muructl_path()
    assert muructl is not None
    rendered = subprocess.run(
        [
            str(muructl),
            "render",
            "shop",
            str(SHOP),
            "--release",
            "2026-11-26",
            "--deployment",
            "shop",
            "--no-git-metadata",
            "--python",
            sys.executable,
        ],
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    ).stdout
    table = tomllib.loads(rendered)
    assert table["deployment"] == "shop" and "2026-11-26" in table["app"]["shop"]["release"]

    file = tmp_path / "shop-2026-11-26.toml"
    file.write_text(rendered)
    assert daemon.muructl("apply", "-f", str(file)).returncode == 0
    listed = json.loads(daemon.muructl("deployment", "list", "--json").stdout)
    assert {entry["name"] for entry in listed["deployments"]} == {"default", "shop"}
