"""muructl open (design specification sections 10.7 and 13.1): the URL of
a development run from its registry record, of an application on a daemon
from its routing, and its dashboard, signed in, opened through the
launcher $BROWSER names."""

from __future__ import annotations

import http.client
import os
import subprocess
import time
import urllib.parse
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon


def browser(tmp_path: Path) -> tuple[Path, Path]:
    """A launcher that writes the URL it is given to a file."""

    opened = tmp_path / "opened"
    script = tmp_path / "browser"
    script.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{opened}'\n")
    script.chmod(0o755)
    return script, opened


def wait_for_lines(path: Path, count: int) -> list[str]:
    deadline = time.monotonic() + 20

    while time.monotonic() < deadline:
        if path.exists() and len(lines := path.read_text().splitlines()) >= count:
            return lines

        time.sleep(0.05)

    raise AssertionError(f"the launcher was not run {count} times")


def get(url: str, cookie: str = "") -> http.client.HTTPResponse:
    parts = urllib.parse.urlsplit(url)
    connection = http.client.HTTPConnection(parts.hostname, parts.port, timeout=10)
    target = parts.path + (f"?{parts.query}" if parts.query else "")
    connection.request("GET", target, headers={"Cookie": cookie} if cookie else {})
    return connection.getresponse()


def test_open_finds_a_runs_url_and_signs_in_to_a_daemons_dashboard(tmp_path: Path) -> None:
    script, opened = browser(tmp_path)
    muructl = support.muructl_path()
    assert muructl is not None
    env = {**os.environ, "BROWSER": str(script)}

    def open_(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(muructl), "open", *arguments],
            capture_output=True,
            text=True,
            env=env,
            cwd=support.FIXTURES,
            timeout=60,
            check=False,
        )

    # A run with HTTPS opens its own https URL once it answers.

    run = support.start(
        "plain_wsgi:application",
        workers=1,
        threads=1,
        https=True,
        args=["--https", "--open"],
        env={"BROWSER": str(script)},
    )

    try:
        assert wait_for_lines(opened, 1) == [f"https://localhost:{run.https_port}/"]

        printed = open_("--print")
        assert printed.stdout.strip() == f"https://localhost:{run.https_port}/", printed.stderr
        assert open_("--http", "--print", "/admin/").stdout.strip() == (
            f"http://localhost:{run.port}/admin/"
        )

        # The run's dashboard needs no sign-in.

        assert open_("--dashboard").returncode == 0
        dashboard = wait_for_lines(opened, 2)[1]
        assert get(dashboard).status == 200, dashboard
    finally:
        run.stop()

    # A daemon's application is found by its routing, and its dashboard
    # is opened with a single-use link that signs the browser in.

    base = tmp_path / "daemon"
    base.mkdir()
    port = support.free_port()
    daemon = start_daemon(base, arguments=["--dashboard", f"127.0.0.1:{port}"])

    try:
        daemon.deploy("shop", SHOP)
        socket = ["--socket", str(daemon.run_dir / "admin.sock")]
        listed = open_("shop", "--print", *socket)
        assert listed.stdout.strip() == f"http://localhost:{daemon.port}/", listed.stderr

        assert open_("--dashboard", *socket).returncode == 0
        link = wait_for_lines(opened, 3)[2]
        assert f"127.0.0.1:{port}/login?code=" in link, link
        signed = get(link)
        assert signed.status == 303
        cookie = signed.getheader("Set-Cookie", "").split(";")[0]
        front = get(f"http://127.0.0.1:{port}/", cookie)
        assert front.status == 200

        # The node page lists where the application is reached.

        assert f"<code>http://localhost:{daemon.port}/</code>" in front.read().decode()
        assert get(link).status == 403, "the link is used once"
    finally:
        daemon.stop()
