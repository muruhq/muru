"""The daemon's dashboard and JSON gateway (design specification section
10.7): the node, its applications and releases, hosts and routes, recent
requests, events and log tails, as pages and as JSON on a loopback
address the daemon serves itself, for a signed-in session."""

from __future__ import annotations

import http.client
import http.server
import json
import re
import subprocess
import sys
import threading
import time
import urllib.parse
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest
from support import server as support
from support.daemon import SHOP, Daemon, start_daemon


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


# The bearer token each test's dashboard was signed in with, by port, so
# the helpers send it unless told not to.

TOKENS: dict[int, str] = {}


def fetch(
    port: int, path: str, headers: dict[str, str] | None = None, signed: bool = True
) -> tuple[int, dict[str, str], bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    sent = dict(headers or {})

    if signed and port in TOKENS:
        sent.setdefault("Authorization", f"Bearer {TOKENS[port]}")

    try:
        connection.request("GET", path, headers=sent)
        response = connection.getresponse()
        return response.status, {k.lower(): v for k, v in response.getheaders()}, response.read()
    finally:
        connection.close()


def fetch_json(port: int, path: str) -> Any:
    status, headers, body = fetch(port, path)
    assert status == 200, body
    assert headers["content-type"] == "application/json"
    return json.loads(body)


def sign_in(daemon: Daemon, port: int) -> None:
    """Have the helpers send a token that may look."""

    TOKENS[port] = daemon.muructl("dashboard", "login", "--token").stdout.strip()


@pytest.fixture
def served(tmp_path: Path) -> Iterator[tuple[Daemon, int]]:
    """A daemon with its dashboard on a free loopback port, signed in."""

    port = support.free_port()
    instance = start_daemon(tmp_path, arguments=["--dashboard", f"127.0.0.1:{port}"])

    try:
        sign_in(instance, port)
        yield instance, port
    finally:
        TOKENS.pop(port, None)
        instance.stop()


def test_pages_show_the_node_its_releases_requests_events_and_logs(
    served: tuple[Daemon, int],
) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert daemon.request("/catalogue").status == 200
    wait_for(lambda: "shop: served /catalogue" in daemon.muructl("logs", "shop").stdout)

    status, headers, body = fetch(port, "/")
    assert status == 200
    assert headers["content-type"].startswith("text/html")
    assert "default-src 'none'" in headers["content-security-policy"]
    assert "frame-ancestors 'self'" in headers["content-security-policy"]
    page = body.decode()
    assert "<title>Muru: daemon</title>" in page
    assert f"http://127.0.0.1:{daemon.port}" in page
    assert "shop@r1" in page and "ready" in page
    assert '<th class="num">Threads</th>' in page and '<td class="num">0/6</td>' in page
    assert '<th class="num">Loop</th>' not in page
    assert "/catalogue" in page
    assert "release.ready" in page
    assert "shop: served /catalogue" in page

    for path in (
        "/fragment/node/overview",
        "/fragment/node/requests",
        "/fragment/node/events",
        "/fragment/node/logs",
    ):
        status, headers, body = fetch(port, path)
        assert status == 200, path
        assert headers["content-type"].startswith("text/html")
        assert body

    assert fetch(port, "/static/htmx.min.js")[0] == 200
    assert fetch(port, "/missing")[0] == 404
    assert f"dashboard http://127.0.0.1:{port}/" in daemon.muructl("status").stdout


def test_the_gateway_serves_the_same_figures_as_json(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert daemon.request("/one").status == 200

    status = fetch_json(port, "/v1/status")
    assert status["pid"] == daemon.process.pid
    assert status["listeners"] == [f"http://127.0.0.1:{daemon.port}"]
    assert status["snapshot_version"] >= 1
    assert status["summary"]["errors"] == 0

    apps = fetch_json(port, "/v1/apps")
    assert [app["name"] for app in apps] == ["shop"]
    assert apps[0]["current"] == "r1"
    assert apps[0]["releases"][0]["state"] == "ready"
    assert apps[0]["releases"][0]["serving"] is True
    assert apps[0]["releases"][0]["workers"] == 2
    assert apps[0]["releases"][0]["slots_total"] == 6

    # The shop is WSGI, two processes of three threads, with no event
    # loop, so its slots are all on the threads.

    assert apps[0]["releases"][0]["threads"] == {"busy": 0, "slots": 6}
    assert apps[0]["releases"][0]["event_loop"] is None

    releases = fetch_json(port, "/v1/apps/shop/releases")
    assert releases == apps[0]["releases"]
    assert fetch(port, "/v1/apps/none/releases")[0] == 404

    requests = fetch_json(port, "/v1/requests")
    assert requests[0]["target"] == "/one"
    assert requests[0]["release"] == "shop@r1"
    assert requests[0]["status"] == 200

    events = fetch_json(port, "/v1/events")
    assert events[0]["sequence"] > events[-1]["sequence"], "newest first"
    assert "release.ready" in [event["kind"] for event in events]
    assert fetch_json(port, "/v1/hosts") == []
    assert fetch_json(port, "/v1/routes") == []

    status, headers, body = fetch(port, "/metrics")
    assert status == 200
    assert headers["content-type"].startswith("text/plain; version=0.0.4")
    assert 'muru_requests_total{app="shop"' in body.decode()


def test_worker_pages_show_status_stacks_tasks_and_memory(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert daemon.request("/catalogue").status == 200

    # The daemon page links each running release to its workers, whose
    # page shows what debug status shows and links each worker to its own.

    page = fetch(port, "/")[2].decode()
    assert '<a href="workers?release=shop@r1">shop@r1</a>' in page
    status, headers, body = fetch(port, "/workers?release=shop@r1")
    assert status == 200 and headers["content-type"].startswith("text/html")
    page = body.decode()
    assert "<title>Muru: shop@r1 workers</title>" in page
    assert '<a href="worker?release=shop@r1&worker=0">0</a>' in page
    assert '<a href="worker?release=shop@r1&worker=1">1</a>' in page
    assert page.count('class="state-ready">ready') == 2
    assert 'hx-get="fragment/workers/stacks?release=shop@r1"' in page

    # The stacks are merged across workers as muructl debug stacks merges
    # them; the status fragment polls.

    stacks = fetch(port, "/fragment/workers/stacks?release=shop@r1")[2].decode()
    assert "<h3>MainThread in 2 workers (0, 1)</h3>" in stacks
    assert "muru-control" not in stacks
    fragment = fetch(port, "/fragment/workers/status?release=shop@r1")[2].decode()
    assert fragment.count("<tr>") == 3

    # One worker's page: its status, stacks, tasks, and memory when asked
    # for, which is recorded as muructl debug memory is.

    page = fetch(port, "/worker?release=shop@r1&worker=1")[2].decode()
    assert "<title>Muru: shop@r1 worker 1</title>" in page
    assert page.count('class="state-ready">ready') == 1
    assert 'hx-get="fragment/workers/memory?release=shop@r1&worker=1"' in page
    stacks = fetch(port, "/fragment/workers/stacks?release=shop@r1&worker=1")[2].decode()
    assert "<h3>MainThread in worker 1</h3>" in stacks and "worker 0" not in stacks
    tasks = fetch(port, "/fragment/workers/tasks?release=shop@r1&worker=1")[2].decode()
    assert "worker 1 (pid" in tasks and "no event loop" in tasks
    memory = fetch(port, "/fragment/workers/memory?release=shop@r1&worker=1")[2].decode()
    assert "worker 1 (pid" in memory and "GC counts" in memory and "<td>dict</td>" in memory
    assert "debug.memory" in daemon.muructl("events").stdout

    # An ASGI worker's tasks.

    assert daemon.deploy(
        "asgi",
        support.FIXTURES,
        "--entrypoint",
        "plain_asgi:application",
        "--processes",
        "1",
        "--hold",
    ) == ("muru: release asgi@r1 is ready and held: not serving")
    tasks = fetch(port, "/fragment/workers/tasks?release=asgi@r1")[2].decode()
    assert "loop lag" in tasks

    # The same answers as JSON, shaped as muructl debug --json prints them.

    answer = fetch_json(port, "/v1/debug/status?release=shop@r1")
    assert answer["release"] == "r1"
    assert [worker["index"] for worker in answer["workers"]] == [0, 1]
    assert all(worker["ok"] and worker["result"]["ready"] for worker in answer["workers"])
    answer = fetch_json(port, "/v1/debug/stacks?release=shop@r1&worker=0")
    assert len(answer["workers"]) == 1
    assert any(t["name"] == "MainThread" for t in answer["workers"][0]["result"]["threads"])
    tasks = fetch_json(port, "/v1/debug/tasks?release=asgi@r1")
    assert tasks["workers"][0]["result"]["lag_ms"] is not None

    # What cannot be asked.

    status, _, body = fetch(port, "/v1/debug/status?release=shop@r9")
    assert status == 409 and b"not running" in body
    assert fetch(port, "/v1/debug/inspect?release=shop@r1")[0] == 404
    assert fetch(port, "/workers")[0] == 400
    assert fetch(port, "/worker?release=shop@r1")[0] == 400
    assert fetch(port, "/worker?release=shop@r1&worker=x")[0] == 400
    page = fetch(port, "/workers?release=shop@r9")[2].decode()
    assert "not running" in page


def test_the_dashboard_is_loopback_only(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError, match="loopback"):
        start_daemon(tmp_path, arguments=["--dashboard", "0.0.0.0:0"])


def read_sse(
    port: int,
    path: str,
    wanted: str,
    timeout: float = 15.0,
    count: int = 1,
    containing: tuple[str, ...] = (),
) -> str:
    """Read a Server-Sent Events stream until `count` messages of `wanted` type
    have arrived, and one of `containing` with them when it names any."""

    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    headers = {"Authorization": f"Bearer {TOKENS[port]}"} if port in TOKENS else {}

    try:
        connection.request("GET", path, headers=headers)
        response = connection.getresponse()
        assert response.status == 200
        assert response.getheader("Content-Type") == "text/event-stream"
        deadline = time.monotonic() + timeout
        collected = b""

        while time.monotonic() < deadline:
            chunk = response.read1(4096) if hasattr(response, "read1") else response.read(4096)

            if not chunk:
                break

            collected += chunk

            complete = collected.rsplit(b"\n\n", 1)[0] + b"\n\n" if b"\n\n" in collected else b""

            enough = complete.count(f"event: {wanted}\n".encode()) >= count
            found = not containing or any(text.encode() in complete for text in containing)

            if enough and found:
                return collected.decode()

        raise AssertionError(f"no {wanted} event in {collected!r}")
    finally:
        connection.close()


def test_the_history_grows_and_streams_for_the_charts(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

    # Points are differences between successive readings a second apart,
    # so requests keep flowing while the first ones are waited for.

    def recorded() -> bool:
        for _ in range(20):
            assert daemon.request("/catalogue").status == 200

        history = fetch_json(port, "/v1/history?range=1h")
        points = history["points"]
        return len(points) >= 2 and sum(point[1] for point in points) > 0

    wait_for(recorded)
    history = fetch_json(port, "/v1/history?range=1h")
    assert history["range"] == "1h" and history["step_s"] == 1
    points = history["points"]
    assert len(points) >= 2
    assert all(len(point) == 8 for point in points)
    assert points[-1][0] > points[0][0], "seconds in order"
    assert any(point[6] == 2 for point in points), "two workers listening"
    assert fetch_json(port, "/v1/history?range=24h")["step_s"] == 60

    # The stream keeps delivering: one point a second, not just the first.

    stream = read_sse(port, "/v1/history/stream", "point", count=3)
    assert stream.count("event: point\ndata: [") >= 3

    # The page carries the charts and their script, which the policy
    # allows alongside the stylesheet.

    status, headers, body = fetch(port, "/")
    assert status == 200
    page = body.decode()
    assert (
        'id="charts"' in page and '"static/charts.js"' in page and '"static/uplot.min.js"' in page
    )
    assert "style-src 'self' 'unsafe-inline'" in headers["content-security-policy"]

    for asset, kind in [
        ("/static/uplot.min.js", "text/javascript"),
        ("/static/uplot.min.css", "text/css"),
        ("/static/sse.js", "text/javascript"),
        ("/static/charts.js", "text/javascript"),
    ]:
        status, headers, body = fetch(port, asset)
        assert status == 200 and headers["content-type"] == kind and body, asset


def test_events_stream_as_rows_and_json(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    outcome: list[str] = []

    def listen() -> None:
        # A worker's own events can come first, so read on to the drain's.

        kinds = ('"kind":"release.', '"kind":"traffic.')
        outcome.append(read_sse(port, "/v1/events/stream", "json", containing=kinds))

    listener = threading.Thread(target=listen)
    listener.start()
    time.sleep(0.5)
    assert daemon.muructl("drain", "shop@r1").stdout.strip() == "muru: release shop@r1 is stopped"
    listener.join(timeout=20)
    assert outcome, "no event arrived on the stream"
    text = outcome[0]
    assert "event: event\ndata: <tr class=" in text
    assert '"kind":"release.' in text or '"kind":"traffic.' in text
    page = fetch(port, "/")[2].decode()
    assert 'sse-connect="v1/events/stream"' in page and 'id="event-rows"' in page


def test_scaling_rollouts_and_maintenance_are_shown(served: tuple[Daemon, int]) -> None:
    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    daemon.muructl("deploy", "shop", str(SHOP), "--python", sys.executable, "--hold")
    daemon.muructl(
        "rollout", "canary", "shop@r2", "--steps", "20", "--hold", "60s", "--min-requests", "1000"
    )
    daemon.muructl("maintenance", "on", "shop")

    rollouts = fetch_json(port, "/v1/rollouts")
    assert rollouts[0]["candidate"] == "r2" and rollouts[0]["baseline"] == "r1"
    assert rollouts[0]["state"] == "running" and rollouts[0]["percent"] == 20

    scaling = fetch_json(port, "/v1/scaling")
    rows = {(row["release"], row["group"]): row for row in scaling}
    assert rows[("shop@r1", "default")]["workers"] == "2"
    assert rows[("shop@r1", "default")]["policy"].startswith("static")
    assert rows[("shop@r1", "default")]["diagnosis"]

    apps = fetch_json(port, "/v1/apps")
    assert apps[0]["maintenance_since"]

    page = fetch(port, "/")[2].decode()
    assert "maintenance since" in page
    assert "<h3>Rollouts</h3>" in page and "<h3>Scaling</h3>" in page


def post_form(
    port: int, path: str, fields: dict[str, str], headers: dict[str, str]
) -> tuple[int, bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)

    try:
        body = urllib.parse.urlencode(fields)
        connection.request(
            "POST",
            path,
            body=body,
            headers={"Content-Type": "application/x-www-form-urlencoded", **headers},
        )
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def fetch_with(port: int, path: str, headers: dict[str, str]) -> str:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)

    try:
        connection.request("GET", path, headers=headers)
        return connection.getresponse().read().decode()
    finally:
        connection.close()


def test_everything_but_the_metrics_and_assets_needs_a_session(
    served: tuple[Daemon, int],
) -> None:
    daemon, port = served
    status, headers, body = fetch(port, "/", signed=False)
    assert status == 401 and headers["content-type"].startswith("text/html")
    assert "muructl dashboard login" in body.decode()

    for path in (
        "/fragment/node/overview",
        "/v1/status",
        "/v1/apps",
        "/v1/history",
        "/v1/history/stream",
        "/v1/events/stream",
        "/workers?release=shop@r1",
        "/fragment/workers/stacks?release=shop@r1",
        "/v1/debug/status?release=shop@r1",
    ):
        status, _, body = fetch(port, path, signed=False)
        assert status == 401 and b"not signed in" in body, path

    assert fetch(port, "/v1/apps", {"Authorization": "Bearer " + "0" * 64}, signed=False)[0] == 401
    assert fetch(port, "/metrics", signed=False)[0] == 200
    assert fetch(port, "/static/htmx.min.js", signed=False)[0] == 200

    # A session may look, and nothing on the dashboard changes anything:
    # the pages have no forms, and a POST to a path that once took an
    # action is refused as every POST is (issue 547).

    assert fetch(port, "/v1/apps")[0] == 200
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"
    assert "<form" not in fetch(port, "/")[2].decode()
    assert "<form" not in fetch(port, "/workers?release=shop@r1")[2].decode()
    refused = post_form(
        port,
        "/actions/maintenance",
        {"app": "shop", "state": "on"},
        {"Authorization": f"Bearer {TOKENS[port]}"},
    )
    assert refused[0] == 405 and b"only GET is served" in refused[1]
    assert daemon.request("/").status == 200

    assert fetch(port, "/login?code=anything", signed=False)[0] == 403

    # A link for a dashboard reached at another address.

    link = daemon.muructl(
        "dashboard", "login", "--url", "https://jupyter.test/proxy/8090/"
    ).stdout.strip()
    assert re.fullmatch(r"https://jupyter\.test/proxy/8090/login\?code=[0-9a-f]{64}", link), link


class PrefixProxy(http.server.ThreadingHTTPServer):
    """Serve a backend under `/proxy/PORT/`, as jupyter-server-proxy does,
    rewriting the Host header to the backend's."""

    def __init__(self, backend: int) -> None:
        self.backend = backend
        super().__init__(("127.0.0.1", 0), PrefixProxyHandler)


class PrefixProxyHandler(http.server.BaseHTTPRequestHandler):
    server: PrefixProxy

    def log_message(self, format: str, *args: Any) -> None:
        pass

    def forward(self) -> None:
        prefix = f"/proxy/{self.server.backend}"
        assert self.path.startswith(prefix + "/"), self.path
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length) if length else None
        headers = {k: v for k, v in self.headers.items() if k.lower() != "host"}
        connection = http.client.HTTPConnection("127.0.0.1", self.server.backend, timeout=10)

        try:
            connection.request(self.command, self.path[len(prefix) :], body=body, headers=headers)
            response = connection.getresponse()
            payload = response.read()
            self.send_response(response.status)

            for name, value in response.getheaders():
                if name.lower() not in ("transfer-encoding", "connection", "content-length"):
                    self.send_header(name, value)

            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        finally:
            connection.close()

    do_GET = forward


def test_the_dashboard_works_under_a_proxy_path_prefix(tmp_path: Path) -> None:
    port = support.free_port()
    daemon = start_daemon(
        tmp_path,
        arguments=["--dashboard", f"127.0.0.1:{port}"],
    )
    proxy = PrefixProxy(port)
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    proxy_port = proxy.server_address[1]
    base = f"http://127.0.0.1:{proxy_port}/proxy/{port}/"

    def through(
        url: str, headers: dict[str, str] | None = None
    ) -> tuple[int, dict[str, str], bytes]:
        parsed = urllib.parse.urlsplit(url)
        target = parsed.path + (f"?{parsed.query}" if parsed.query else "")
        return fetch(proxy_port, target, headers, signed=False)

    try:
        assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

        # Signing in through the proxy lands on the page under the prefix.

        link = daemon.muructl("dashboard", "login", "--url", base).stdout.strip()
        assert link.startswith(base + "login?code=")
        status, headers, _ = through(link)
        assert status == 303
        page_url = urllib.parse.urljoin(link, headers["location"])
        assert page_url == base
        cookie = {"Cookie": headers["set-cookie"].split(";")[0]}

        status, _, body = through(page_url, cookie)
        assert status == 200
        page = body.decode()

        # Every address the page names resolves under the prefix.

        addresses = re.findall(r'(?:href|src|hx-get|sse-connect)="([^"]+)"', page)
        assert addresses and not [address for address in addresses if address.startswith("/")]

        for address in addresses:
            url = urllib.parse.urljoin(page_url, address)
            assert url.startswith(base), url

            if "stream" not in address:
                assert through(url, cookie)[0] == 200, url

        history = urllib.parse.urljoin(page_url, "v1/history?range=1h")
        assert json.loads(through(history, cookie)[2])["range"] == "1h"
    finally:
        proxy.shutdown()
        daemon.stop()


def test_the_processes_are_listed_as_the_daemon_supervises_them(
    served: tuple[Daemon, int],
) -> None:
    """`muructl status --processes` (issue 595): the daemon, its edge, the
    release's host with its relay and workers beneath it, titled as each
    says; narrowed to an application; frame after frame with --watch; and
    the dashboard's section from the same answer."""

    daemon, port = served
    assert daemon.deploy("shop", SHOP) == "muru: release shop@r1 is ready and serving"

    def tree(*more: str) -> dict[str, Any]:
        listed = daemon.muructl("status", "--processes", "--json", *more)
        answer: dict[str, Any] = json.loads(listed.stdout)
        return answer

    def host_of(answer: dict[str, Any]) -> dict[str, Any]:
        return next(child for child in answer["root"]["children"] if child["role"] == "host")

    wait_for(lambda: len(host_of(tree())["children"]) == 3)
    answer = tree()
    root = answer["root"]
    assert (answer["mode"], root["role"], root["pid"]) == ("daemon", "daemon", daemon.process.pid)
    assert [child["role"] for child in root["children"]] == ["edge", "host"]
    edge, host = root["children"]
    state = (daemon.base / "state").resolve()

    # Issue 604: the daemon's row names whose daemon it is, as its title
    # does, and a host's says what its release does with traffic.

    assert root["what"] == f"user daemon, state {state}", root["what"]
    assert edge["what"] == f"listening on 127.0.0.1:{daemon.port}"
    assert (host["what"], host["state"]) == ("shop@r1 serving", "serving")
    relay, *workers = host["children"]
    assert relay["what"] == "shop@r1 output to apps/shop/r1.log", relay
    assert relay["os_parent_pid"] == root["pid"] and not relay["readopted"]
    assert [(worker["group"], worker["index"]) for worker in workers] == [
        ("default", 0),
        ("default", 1),
    ]
    assert all(process["user"] and process["rss_bytes"] for process in [root, edge, host])

    # What the default mount runs as, which chooses GIL/LAG (issue 607).

    assert [process["interface"] for process in [host, *workers]] == ["wsgi"] * 3

    def title(pid: int) -> str:
        return subprocess.run(
            ["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True, check=False
        ).stdout.strip()

    # The daemon's state directory is not the default for a user's, so
    # the titles name it.

    assert title(root["pid"]) == f"muru daemon {state}"
    assert title(edge["pid"]) == f"muru edge {state}"
    assert title(host["pid"]) == "muru host shop@r1"
    assert title(workers[0]["pid"]) == "muru worker shop@r1 default/0"

    # On Linux the short name too, which top shows.

    if sys.platform == "linux":
        names = {
            Path(f"/proc/{pid}/comm").read_text().strip()
            for pid in (root["pid"], edge["pid"], host["pid"], relay["pid"], workers[0]["pid"])
        }
        assert names == {"muru-daemon", "muru-edge", "muru-host", "muru-relay", "muru-worker"}, (
            names
        )

    # Narrowed to the application, the daemon stays and the edge goes.

    narrowed = tree("--app", "shop@r1")
    assert [child["role"] for child in narrowed["root"]["children"]] == ["host"]
    text = daemon.muructl("status", "--processes").stdout
    assert text.splitlines()[0].startswith("edge "), text
    assert "USER" in text and "├─ edge" in text and "└─ host" in text, text

    # --watch, not on a terminal, prints one JSON document a line.

    muructl = support.muructl_path()
    assert muructl is not None
    socket = str(daemon.run_dir / "admin.sock")
    watching = subprocess.Popen(
        [str(muructl), "status", "--processes", "--watch", "200ms", "--json", "--socket", socket],
        stdout=subprocess.PIPE,
        text=True,
    )

    try:
        assert watching.stdout is not None
        frames = [json.loads(watching.stdout.readline()) for _ in range(2)]
        assert frames[1]["sampled_unix_ms"] > frames[0]["sampled_unix_ms"]
    finally:
        watching.kill()
        watching.wait()

    # The dashboard shows the same processes.

    shown = fetch_json(port, "/v1/processes")
    assert [child["pid"] for child in host_of(shown)["children"]] == [
        child["pid"] for child in host["children"]
    ]
    page = fetch(port, "/fragment/processes")[2].decode()
    assert "<th>User</th>" in page and "shop@r1 output to apps/shop/r1.log" in page, page
