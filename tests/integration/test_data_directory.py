"""An application's data directory and each release's static directory
(issues 849 and 850, design specification sections 4.9 and 13.8).

Under the daemon the data directory is STATE/data/APP, shared by every
release and kept when releases go, with media and uploads parts Muru
makes; the media part is a file hint root and may be served by a
`media:` static entry, and nothing else of it ever is. Each release has
STATE/static/APP/RELEASE, named `static:`, made empty when the release is
created and removed with it. `muructl run` keeps both under the user
state location, never in the project, and a command run beside it gets
the same places. `muructl serve` has a data directory only when one is
named, and it must be there.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
import time
from pathlib import Path

from support import server as support
from support.daemon import start_daemon

APP = textwrap.dedent(
    """
    import json
    import os


    def application(environ, start_response):
        path = environ["PATH_INFO"]

        if path == "/hint":
            hinted = os.path.join(os.environ["MURU_MEDIA_DIR"], "report.txt")
            start_response("200 OK", [("Content-Type", "text/plain"), ("X-Muru-Sendfile", hinted)])
            return [b""]

        if path.startswith("/static/"):
            start_response("200 OK", [("Content-Type", "text/plain")])
            return [b"from the application\\n"]

        names = ("MURU_DATA_DIR", "MURU_MEDIA_DIR", "MURU_UPLOADS_DIR", "MURU_STATIC_DIR")
        body = json.dumps({name: os.environ.get(name) for name in names})
        start_response("200 OK", [("Content-Type", "application/json")])
        return [body.encode()]
    """
)

COLLECT = textwrap.dedent(
    """
    import os
    import pathlib

    target = pathlib.Path(os.environ["MURU_STATIC_DIR"])
    target.mkdir(parents=True, exist_ok=True)
    (target / "site.css").write_text("body { color: teal }\\n")
    print("collected into", target)
    """
)


def write_project(project: Path, extra: str = "") -> None:
    project.mkdir(parents=True, exist_ok=True)
    (project / "app.py").write_text(APP)
    (project / "collect.py").write_text(COLLECT)
    (project / "pyproject.toml").write_text(
        textwrap.dedent(
            """
            [project]
            name = "shop"
            version = "0"

            [tool.muru]
            entrypoint = "app:application"
            processes = 1

            [[tool.muru.static]]
            path = "/static/"
            dir = "static:"

            [[tool.muru.static]]
            path = "/media/"
            dir = "media:public"

            [tool.muru.commands.collectstatic]
            run = ["python", "collect.py"]
            timeout = "1m"
            """
        )
        + extra
    )


def places(body: bytes) -> dict[str, str]:
    return json.loads(body)


def test_the_daemon_keeps_data_across_releases_and_a_static_directory_per_release(
    tmp_path: Path,
) -> None:
    project = tmp_path / "shop"
    write_project(project)
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon")
    state = tmp_path / "daemon" / "state"
    data = state / "data" / "shop"

    try:
        assert daemon.deploy("shop", project) == "muru: release shop@r1 is ready and serving"
        first = places(daemon.get().body)
        assert first == {
            "MURU_DATA_DIR": str(data),
            "MURU_MEDIA_DIR": str(data / "media"),
            "MURU_UPLOADS_DIR": str(data / "uploads"),
            "MURU_STATIC_DIR": str(state / "static" / "shop" / "r1"),
        }

        for part in (data, data / "media", data / "uploads"):
            assert part.stat().st_mode & 0o777 == 0o700, part

        assert list((state / "static" / "shop" / "r1").iterdir()) == []

        # Media is a file hint root with nothing configured, and media:
        # serves a part of it; the static directory is the release's own.

        (data / "media" / "report.txt").write_text("a report\n")
        (data / "media" / "public").mkdir()
        (data / "media" / "public" / "avatar.txt").write_text("an avatar\n")
        assert daemon.get("/hint").body == b"a report\n"
        assert daemon.get("/media/avatar.txt").body == b"an avatar\n"
        assert daemon.get("/static/site.css").body == b"from the application\n"
        collected = daemon.muructl("command", "run", "shop", "collectstatic").stdout
        assert f"collected into {state / 'static' / 'shop' / 'r1'}" in collected
        assert daemon.get("/static/site.css").body == b"body { color: teal }\n"

        # A command and exec see what the release sees.

        shown = daemon.muructl(
            "exec", "shop", "--", "python", "-c", "import os; print(os.environ['MURU_DATA_DIR'])"
        ).stdout
        assert shown.strip() == str(data)

        # The next release shares the data directory and starts with an
        # empty static directory of its own.

        (data / "app.db").write_text("kept")
        assert daemon.deploy("shop", project) == "muru: release shop@r2 is ready and serving"
        second = places(daemon.get().body)
        assert second["MURU_DATA_DIR"] == str(data)
        assert second["MURU_STATIC_DIR"] == str(state / "static" / "shop" / "r2")
        assert daemon.get("/static/site.css").body == b"from the application\n"

        # Only media is ever served.

        for arguments, why in (
            (["--sendfile-root", "data:"], "the data directory is never served"),
            (["--static", f"/files/={data / 'uploads'}"], "is the uploads part"),
            (["--sendfile-root", str(state)], "contains the data directory"),
        ):
            refused = daemon.muructl(
                "deploy", "shop", str(project), "--python", sys.executable, *arguments, check=False
            )
            assert refused.returncode != 0
            assert why in refused.stderr, refused.stderr

        # What data show and data check say of it.

        shown_data = json.loads(daemon.muructl("data", "show", "shop", "--json", "--sizes").stdout)
        assert [part["name"] for part in shown_data["parts"]] == ["data", "media", "uploads"]
        assert shown_data["parts"][1]["root"] and shown_data["parts"][0]["bytes"] > 0
        assert daemon.muructl("data", "check", "shop").returncode == 0
        (data / "media" / "copy.sqlite3").write_text("x")
        checked = daemon.muructl("data", "check", "shop", check=False)
        assert checked.returncode == 1
        assert "copy.sqlite3 is a database file in the media part" in checked.stdout
        status = daemon.muructl("status", "--app", "shop").stdout
        assert f"media    {data / 'media'}" in status and "a file hint root" in status

        # A release that goes takes its static directory; the data
        # directory stays, and goes with the application only when asked.

        assert daemon.deploy("shop", project) == "muru: release shop@r3 is ready and serving"
        daemon.wait_for_state("shop@r1", "stopped")
        daemon.muructl("release", "remove", "shop@r1", "--yes")
        assert not (state / "static" / "shop" / "r1").exists()
        assert (data / "app.db").read_text() == "kept"
        removed = daemon.muructl("app", "remove", "shop", "--yes").stdout
        assert f"the data directory {data} is kept" in removed, removed
        assert (data / "app.db").exists()
        assert not (state / "static" / "shop").exists()
        assert daemon.deploy("shop", project) == "muru: release shop@r1 is ready and serving"
        removed = daemon.muructl("app", "remove", "shop", "--data", "--yes").stdout
        assert f"removed the data directory {data}" in removed, removed
        assert not data.exists()
    finally:
        daemon.stop()


def test_a_run_keeps_its_places_out_of_the_project_and_its_commands_share_them(
    tmp_path: Path,
) -> None:
    project = tmp_path / "shop"
    write_project(project)
    server = support.start(
        None,
        workers=1,
        threads=1,
        chdir=project,
        env={"MURU_DATA_DIR": "/somewhere/else"},
    )

    try:
        given = places(server.get("/").body)
        data = Path(given["MURU_DATA_DIR"])
        assert data.parent.name.startswith("shop-")
        assert data.parent.parent == Path(os.environ["MURU_STATE_DIR"]) / "runs"
        assert given["MURU_STATIC_DIR"] == str(data.parent / "static")
        assert not str(data).startswith(str(project))
        log = server.log()
        assert f"media    {data / 'media'}, a file hint root" in log, log
        assert "empty for now: its requests reach the application" in log, log

        # Until something fills the static directory its requests reach
        # the application; a command run beside the run fills it.

        assert server.get("/static/site.css").body == b"from the application\n"
        env = {**os.environ, "MURU_RUN_DIR": os.environ["MURU_RUN_DIR"]}
        subprocess.run(
            [str(support.muructl_path()), "command", "run", "collectstatic"],
            cwd=project,
            env=env,
            check=True,
            capture_output=True,
        )
        deadline = time.monotonic() + 10

        while server.get("/static/site.css").body != b"body { color: teal }\n":
            assert time.monotonic() < deadline, "the collected file was not served"
            time.sleep(0.1)
    finally:
        server.stop()

    # With nothing serving the project, a command run on its own uses the
    # same places.

    alone = subprocess.run(
        [
            str(support.muructl_path()),
            "exec",
            "--python",
            sys.executable,
            "--",
            "python",
            "-c",
            "import os; print(os.environ['MURU_DATA_DIR'])",
        ],
        cwd=project,
        capture_output=True,
        text=True,
        check=True,
    )
    assert alone.stdout.strip() == str(data)

    # muructl serve has a data directory only when one is named, and it
    # must be there.

    missing = subprocess.run(
        [
            str(support.muructl_path()),
            "serve",
            "--chdir",
            str(project),
            "--python",
            sys.executable,
            "--data-dir",
            str(tmp_path / "no-volume"),
            "--listen",
            "127.0.0.1:0",
            "--admin-listen",
            "off",
            "--run-dir",
            str(support.short_dir("muru-sv-")),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert missing.returncode != 0
    said = missing.stdout + missing.stderr
    assert "does not exist; mount a volume there" in said, said
