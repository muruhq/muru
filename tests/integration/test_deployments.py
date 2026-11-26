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


# What shop also defines, and what api names of it beside the host its
# route is on: a rate limit by a host of its own, and a client class by a
# rate limit and by a worker group.

SHARED = (
    "\n[[client_class]]\nname = 'partner'\nheaders = { X-Partner = '.+' }\n\n"
    "[rate_limit.steady]\nrate = '100/m'\n"
)
API_NAMES = (
    "\n[app.api.release.r1.groups.partners]\npaths = ['/partners/*']\n"
    "match = { client_class = ['partner'] }\n\n"
    "[host.api]\nnames = ['api.test']\nrate_limits = ['steady']\n\n"
    "[rate_limit.partners]\nrate = '10/s'\nmatch = { client_class = ['partner'] }\n"
)


def said(completed: subprocess.CompletedProcess[str]) -> str:
    return completed.stdout + completed.stderr


def test_deployments_own_what_they_declare(daemon: Daemon, tmp_path: Path) -> None:
    files = tmp_path / "deploy.d"
    files.mkdir()
    (files / "shop.toml").write_text(SHOP_FILE + SHARED)
    (files / "api.toml").write_text(API_FILE + API_NAMES)

    def apply(*arguments: str) -> subprocess.CompletedProcess[str]:
        return daemon.muructl("apply", *arguments, check=False)

    def deployments() -> dict[str, list[str]]:
        listed = json.loads(daemon.muructl("deployment", "list", "--json").stdout)
        return {entry["name"]: entry.get("apps", []) for entry in listed["deployments"]}

    # What a command makes is default's, and one apply carries several
    # deployments, a route in one naming a host another adds.

    daemon.deploy("landing", SHOP)
    assert "DEPLOYMENT" not in daemon.muructl("app", "list").stdout
    applied = apply("-f", str(files))
    assert applied.returncode == 0, said(applied)
    assert deployments() == {"api": ["api"], "default": ["landing"], "shop": ["shop"]}

    # The lists say whose each object is once a deployment is named,
    # and show one deployment's, and show prints one as its files would
    # state it (issue 943).

    def listed_as(kind: str, *arguments: str) -> list[dict[str, str]]:
        answer = json.loads(daemon.muructl(kind, "list", "--json", *arguments).stdout)
        return list(next(iter(answer.values()), []))

    table = daemon.muructl("app", "list").stdout.splitlines()
    assert table[0].split()[:2] == ["APP", "DEPLOYMENT"], table
    assert ["api", "api"] in [line.split()[:2] for line in table], table
    assert {app["name"]: app["deployment"] for app in listed_as("app")} == {
        "api": "api",
        "landing": "default",
        "shop": "shop",
    }
    assert [app["name"] for app in listed_as("app", "--deployment", "shop")] == ["shop"]
    assert [route["path"] for route in listed_as("route", "--deployment", "api")] == ["/api"]
    assert [host["name"] for host in listed_as("host", "--deployment", "api")] == ["api"]
    assert "DEPLOYMENT" in daemon.muructl("route", "list").stdout
    hosts = [line.split()[:2] for line in daemon.muructl("host", "list").stdout.splitlines()]
    assert ["HOST", "DEPLOYMENT"] in hosts and ["api", "api"] in hosts, hosts
    shown = tomllib.loads(daemon.muructl("deployment", "show", "api").stdout)
    assert shown["deployment"] == "api" and list(shown["app"]) == ["api"], shown

    # Only its owner changes an object.

    stolen = tmp_path / "stolen.toml"
    stolen.write_text(SHOP_FILE + SHARED + release("api", threads=2))
    refused = apply("-f", str(stolen), "--dry-run")
    assert refused.returncode == 1
    assert (
        "app api belongs to the deployment api, and an apply of the deployment shop does not "
        "change it"
    ) in said(refused), said(refused)

    # Nothing is removed while another deployment names it: api's route
    # is on shop's host.

    hostless = tmp_path / "hostless.toml"
    hostless.write_text("deployment = 'shop'\n\n" + release("shop") + SHARED)
    kept = apply("-f", str(hostless), "--prune", "--dry-run")
    assert kept.returncode == 1
    assert "host shop is not removed: route shop /api of deployment api names it" in said(kept), (
        said(kept)
    )

    # Whatever kind names it: a host its rate limit, and a rate limit and
    # a worker group their client class (issue 944).

    bare = tmp_path / "bare.toml"
    bare.write_text(SHOP_FILE)
    kept = apply("-f", str(bare), "--prune", "--dry-run")
    assert kept.returncode == 1

    for refusal in [
        "rate_limit steady is not removed: host api of deployment api names it",
        "client_class partner is not removed: rate_limit partners of deployment api names it",
        "client_class partner is not removed: api@r1: worker group partners of deployment api "
        "names it",
    ]:
        assert refusal in said(kept), said(kept)

    # A prune reaches the deployments the files name and no others, and
    # with =all every deployment, default among them.

    own = apply("-f", str(files / "api.toml"), "--prune", "--dry-run")
    assert own.returncode == 0 and "no changes" in own.stdout, said(own)
    everything = apply("-f", str(files / "shop.toml"), "--prune=all", "--dry-run")
    assert everything.returncode == 2, said(everything)
    removed = {line.split()[1] for line in everything.stdout.splitlines() if "remove-app" in line}
    assert removed == {"landing", "api"}, everything.stdout

    # What the files keep may not name what the same apply removes: with
    # api's files alone, its route would be left on a host that goes.

    orphaned = apply("-f", str(files / "api.toml"), "--prune=all", "--dry-run")
    assert orphaned.returncode == 1
    assert (
        "route shop /api: names host shop, which the files do not declare, so --prune would "
        "remove it"
    ) in said(orphaned), said(orphaned)

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
    assert blocked.returncode == 1 and "of deployment api names it" in said(blocked), said(blocked)
    assert remove("nowhere", "--yes").returncode != 0
    listed = remove("api", "--dry-run")
    assert listed.returncode == 0 and "remove-app" in listed.stdout, said(listed)
    assert "with its data directory" not in listed.stdout, listed.stdout
    assert "api" in deployments()

    # The applications' data directories stay unless --data, which lists
    # each with what it holds before anything goes.

    data = daemon.base / "state" / "data"
    (data / "api").mkdir(parents=True, exist_ok=True)
    (data / "api" / "kept.txt").write_text("x" * 2048)
    (data / "shop").mkdir(parents=True, exist_ok=True)
    listed = remove("api", "--data", "--dry-run")
    assert f"with its data directory {data / 'api'} (" in listed.stdout, listed.stdout
    assert (data / "api").exists()

    assert remove("api", "--data", "--yes").returncode == 0
    assert not (data / "api").exists()
    assert remove("shop", "--yes").returncode == 0
    assert (data / "shop").exists()
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


def test_export_writes_a_deployment_to_a_file(daemon: Daemon, tmp_path: Path) -> None:
    # Issue 937: one file carries one deployment key, so a daemon with
    # several deployments is exported one at a time, or into a directory.

    files = tmp_path / "deploy.d"
    files.mkdir()
    (files / "shop.toml").write_text(SHOP_FILE)
    (files / "api.toml").write_text(API_FILE)

    def export(*arguments: str) -> subprocess.CompletedProcess[str]:
        return daemon.muructl("deployment", "export", *arguments, check=False)

    # With no deployment named there is one file to write, as before.

    daemon.deploy("landing", SHOP)
    whole = tomllib.loads(export().stdout)
    assert "deployment" not in whole and list(whole["app"]) == ["landing"], whole

    assert daemon.muructl("apply", "-f", str(files)).returncode == 0
    unnamed = export()
    assert unnamed.returncode != 0
    assert (
        "this daemon has the deployments api, default, shop; name one, as in muructl deployment "
        "export api, or write a file for each with --dir DIR"
    ) in unnamed.stderr, unnamed.stderr

    shop = tomllib.loads(export("shop").stdout)
    assert shop["deployment"] == "shop" and list(shop["app"]) == ["shop"], shop
    assert list(shop["host"]) == ["shop"] and [route["app"] for route in shop["route"]] == ["shop"]
    assert "deployment" not in tomllib.loads(export("default").stdout)
    assert export("nowhere").returncode != 0

    # A directory holds a file for each, and applies back unchanged.

    out = tmp_path / "exported"
    written = export("--dir", str(out))
    assert written.returncode == 0, said(written)
    assert sorted(path.name for path in out.iterdir()) == ["api.toml", "default.toml", "shop.toml"]
    back = daemon.muructl("apply", "-f", str(out), "--prune=all", "--dry-run", check=False)
    assert back.returncode == 0 and "no changes" in back.stdout, said(back)

    again = export("--dir", str(out))
    assert again.returncode != 0 and "is not empty" in again.stderr, again.stderr

    # One application, written as a deployment of its own, moves out of
    # default when the file is applied.

    moving = tmp_path / "landing.toml"
    moving.write_text(export("--app", "landing", "--as", "site").stdout)
    assert tomllib.loads(moving.read_text())["deployment"] == "site"
    moved = daemon.muructl("apply", "-f", str(moving))
    assert "app landing moves from the deployment default to site" in said(moved), said(moved)
