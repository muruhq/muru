"""An application's own release settings (issue 899): what `muructl app
set` puts on the application, every release deployed after takes, under a
deploy's own flags and over the source's [tool.muru], and an export keeps.
An applied file sets them too (issue 925), from the one file that declares
the application's own settings."""

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

        exported = tomllib.loads(daemon.muructl("deployment", "export").stdout)
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


def test_an_applied_file_sets_the_applications_settings(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path)
    files = tmp_path / "files"
    files.mkdir()
    shop = files / "shop.toml"

    try:
        # Issue 925: muructl dropped these on the way to the daemon, for
        # one file as for a directory.

        shop.write_text(
            "[app.shop]\nprocesses = 3\nmax_lifetime = '3600s'\n\n"
            "[app.shop.env]\nDATABASE_URL = 'postgres://db/shop'\n\n"
            "[app.shop.timeouts]\nhard_timeout = '40s'\n\n"
            "[app.shop.memory]\nsoft_limit = '200MB'\n"
        )
        applied = daemon.muructl("apply", "-f", str(shop))
        assert applied.returncode == 0, applied.stderr

        shown = json.loads(daemon.muructl("app", "show", "shop", "--json").stdout)
        assert shown["processes"] == 3, shown
        assert shown["env"][0]["name"] == "DATABASE_URL", shown

        # The export writes what the file declared, so it applies back
        # without a change.

        exported = tomllib.loads(daemon.muructl("deployment", "export").stdout)["app"]["shop"]
        assert exported["processes"] == 3 and exported["max_lifetime"] == "3600s", exported
        assert exported["env"] == {"DATABASE_URL": "postgres://db/shop"}, exported
        assert exported["timeouts"] == {"hard_timeout": "40s"}, exported
        assert exported["memory"]["soft_limit"] == "200MB", exported

        again = daemon.muructl("apply", "-f", str(files), "--dry-run", check=False)
        assert again.returncode == 0, again.stdout + again.stderr
        assert "no changes" in again.stdout, again.stdout

        # A file states the table in full: one without a setting takes it
        # away.

        shop.write_text("[app.shop]\nprocesses = 3\n")
        daemon.muructl("apply", "-f", str(files))
        shown = json.loads(daemon.muructl("app", "show", "shop", "--json").stdout)
        assert shown.get("env", []) == [] and shown["processes"] == 3, shown

        # The application's own settings go in one file of an apply: a
        # second file that sets any of them is refused, where the later
        # file's used to win.

        (files / "threads.toml").write_text("[app.shop]\nthreads = 2\n")
        refused = daemon.muructl("apply", "-f", str(files), check=False)
        assert refused.returncode != 0
        assert "app shop has settings declared twice" in refused.stderr, refused.stderr
        assert "threads.toml" in refused.stderr, refused.stderr
    finally:
        daemon.stop()
