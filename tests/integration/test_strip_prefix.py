"""A route's strip_prefix (design specification section 4.5, ADR 0078).

A route names the leading part of its path that is where the
application lives. An application's release gets it as SCRIPT_NAME,
after a trusted X-Forwarded-Prefix and before a mount's path, with its
mounts and worker groups matched against the rest; an upstream is sent
the rest of the path, the part appended to X-Forwarded-Prefix; and a
static route's fallback takes it. `muructl apply` refuses a part that is
not a leading part of the route's path, and the key reaches the dry run,
the route list and the export. One grouped test, since each step needs
the daemon, the release and the routes.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import start_daemon
from support.upstream import Upstream

MOUNTED = support.ROOT / "tests" / "fixtures" / "projects" / "mounted"
STATIC = support.ROOT / "tests" / "fixtures" / "static"

PROXY = """
[proxy]
trusted = ["loopback"]
headers = ["x-forwarded-for", "x-forwarded-proto", "x-forwarded-host", "x-forwarded-prefix"]
"""


def routes(legacy: Upstream) -> str:
    return f"""
[host.site]
names = ["site.test"]

[[route]]
host = "site"
path = "/app"
app = "mounted"
strip_prefix = "/app"

[[route]]
host = "site"
path = "/sub/path/reports/"
app = "mounted"
strip_prefix = "/sub/path/"

[[route]]
host = "site"
path = "/plain"
app = "mounted"

[[route]]
host = "site"
path = "/files"
static = "{STATIC}"
fallback = {{ app = "mounted" }}
strip_prefix = "/files"

[upstream.legacy]
addresses = ["{legacy.address}"]

[[route]]
host = "site"
path = "/legacy/v1"
upstream = "legacy"
strip_prefix = "/legacy"
"""


def test_a_route_strips_its_prefix_for_releases_upstreams_and_fallbacks(tmp_path: Path) -> None:
    (tmp_path / "daemon").mkdir()
    daemon = start_daemon(tmp_path / "daemon", config_extra=PROXY)
    legacy = Upstream("legacy")

    def seen(path: str, **headers: str) -> dict[str, Any]:
        response = daemon.request(path, host="site.test", headers=headers)
        assert response.status == 200, (path, response.body)
        report: dict[str, Any] = json.loads(response.body)
        return report

    try:
        daemon.deploy("mounted", MOUNTED)
        config = tmp_path / "routes.toml"
        config.write_text(routes(legacy))

        # The dry run says what each route strips.

        planned = daemon.muructl("apply", "-f", str(config), "--dry-run", check=False).stdout
        assert "to mounted, strip_prefix /app" in planned, planned
        assert f"static {STATIC}, else mounted, strip_prefix /files" in planned, planned
        daemon.muructl("apply", "-f", str(config))

        # The application lives at the part: SCRIPT_NAME is the part, and
        # the rest is PATH_INFO, empty for the part itself.

        def wsgi(path: str, **headers: str) -> tuple[str, str, str]:
            report = seen(path, **headers)
            return report["mount"], report["script_name"], report["path_info"]

        assert wsgi("/app") == ("default", "/app", "")
        assert wsgi("/app/") == ("default", "/app", "/")
        assert wsgi("/app/foo") == ("default", "/app", "/foo")
        assert wsgi("/app/foo", **{"X-Forwarded-Prefix": "/pfx"}) == ("default", "/pfx/app", "/foo")

        # Mounts are matched against the rest, so a mount's path is under
        # the part, and ASGI's path stays the whole path.

        asgi = seen("/app/asgi/x")
        assert (asgi["mount"], asgi["root_path"], asgi["path"]) == (
            "asgi",
            "/app/asgi",
            "/app/asgi/x",
        )
        assert wsgi("/app/reports/r") == ("reports", "/app/reports", "/r")

        # A route narrower than the part strips only the part, and the
        # worker group for the mount's path takes it.

        reports = seen("/sub/path/reports/r")
        assert (reports["script_name"], reports["path_info"], reports["group"]) == (
            "/sub/path/reports",
            "/r",
            "reports",
        )

        # A route that strips nothing gives the whole path, as before.

        assert wsgi("/plain/x") == ("default", "", "/plain/x")

        # A static route's fallback takes the route's part; a file is
        # served from the directory as it is without one.

        assert wsgi("/files/missing") == ("default", "/files", "/missing")
        assert daemon.request("/files/fox.txt", host="site.test").status == 200

        # An upstream is sent the rest of the path, and the part as the
        # forwarded prefix, after a trusted one.

        sent = seen("/legacy/v1/orders?id=3")
        assert sent["path"] == "/v1/orders?id=3"
        assert sent["headers"]["x-forwarded-prefix"] == "/legacy"
        sent = seen("/legacy/v1", **{"X-Forwarded-Prefix": "/pfx"})
        assert (sent["path"], sent["headers"]["x-forwarded-prefix"]) == ("/v1", "/pfx/legacy")

        # The route list and the export show the key.

        assert "[strip_prefix /app]" in daemon.muructl("route", "list").stdout
        exported = daemon.muructl("config", "export").stdout
        assert 'strip_prefix = "/app"' in exported, exported

        # A part that is not a leading part of the route's path on
        # segment boundaries is refused, and so is one on a static route
        # with nothing to fall back to.

        bad = tmp_path / "bad.toml"
        bad.write_text(
            '[host.site]\nnames = ["site.test"]\n\n'
            '[[route]]\nhost = "site"\npath = "/sub/path/api"\napp = "mounted"\n'
            'strip_prefix = "/sub/pa"\n\n'
            f'[[route]]\nhost = "site"\npath = "/docs"\nstatic = "{STATIC}"\n'
            'strip_prefix = "/docs"\n'
        )
        refused = daemon.muructl("apply", "-f", str(bad), check=False)
        said = refused.stdout + refused.stderr
        assert refused.returncode != 0
        assert (
            "route site /sub/path/api: strip_prefix /sub/pa is not a leading part of its path "
            "on segment boundaries" in said
        ), said
        assert "route site /docs: strip_prefix applies to a static route's fallback" in said
    finally:
        legacy.stop()
        daemon.stop()
