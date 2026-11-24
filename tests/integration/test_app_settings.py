"""An application's own release settings (issue 899): what `muructl app
set` puts on the application, every release deployed after takes, under a
deploy's own flags and over the source's [tool.muru], and an export keeps."""

from __future__ import annotations

import json
import sys
import tomllib
from pathlib import Path

from support.daemon import SHOP, start_daemon


def test_a_setting_on_the_application_reaches_every_later_release(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)

    try:
        # Set before the first deploy, so the application is made for it.

        set_ = daemon.muructl(
            "app", "set", "shop", "--env", "DATABASE_URL=postgres://db/shop", "--threads", "3"
        )
        assert set_.returncode == 0, set_.stderr
        assert "no running release changes" in set_.stdout, set_.stdout
        assert "env.DATABASE_URL = postgres://db/shop" in set_.stdout, set_.stdout

        shown = json.loads(daemon.muructl("app", "show", "shop", "--json").stdout)
        assert shown["threads"] == 3 and shown["env"][0]["name"] == "DATABASE_URL", shown

        # A deploy takes them; its own flag wins over the application's.

        deployed = daemon.muructl(
            "deploy", "shop", str(SHOP), "--python", sys.executable, "--threads", "2"
        )
        assert deployed.returncode == 0, deployed.stderr

        inspected = json.loads(daemon.muructl("debug", "inspect", "shop", "--json").stdout)
        assert inspected["workers"][0]["result"]["environment"]["DATABASE_URL"] == (
            "postgres://db/shop"
        )

        plan = daemon.muructl("deploy", "shop", str(SHOP), "--python", sys.executable, "--dry-run")
        assert "env DATABASE_URL (the application)" in plan.stdout, plan.stdout
        assert any(
            line.split() == ["threads", "=", "3", "(the", "application)"]
            for line in plan.stdout.splitlines()
        ), plan.stdout

        # The export writes them on the application's table, and unset
        # takes one away.

        exported = tomllib.loads(daemon.muructl("config", "export").stdout)
        assert exported["app"]["shop"]["threads"] == 3, exported
        assert exported["app"]["shop"]["env"] == {"DATABASE_URL": "postgres://db/shop"}

        unset = daemon.muructl("app", "unset", "shop", "env.DATABASE_URL", "--json")
        assert json.loads(unset.stdout).get("env", []) == [], unset.stdout

        refused = daemon.muructl("app", "unset", "shop", "entrypoint", check=False)
        assert refused.returncode != 0
        assert "muru: error: entrypoint is not an application setting" in refused.stderr, (
            refused.stderr
        )
    finally:
        daemon.stop()
