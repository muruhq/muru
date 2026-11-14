"""Preload modes and lifecycle hooks (design specification sections 6.1 to 6.3)."""

from __future__ import annotations

import json
import os
import shutil
import signal
import sysconfig
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from support import server as support

WORKERS = 2


@pytest.fixture
def hooked() -> Iterator[support.Server]:
    instance = support.start(
        "preload_wsgi:application", workers=WORKERS, args=["--hooks", "hooks_log"]
    )

    try:
        yield instance
    finally:
        instance.stop()


def hook_lines(server: support.Server, name: str) -> list[dict[str, str]]:
    """The fields of each `HOOK name` line, which workers' lines carry after their tag."""

    marker = f"HOOK {name} "
    lines = []

    for line in server.log().splitlines():
        at = line.find(marker)
        if at >= 0:
            lines.append(dict(word.split("=", 1) for word in line[at:].split()[2:]))

    return lines


def import_pids(server: support.Server, count: int) -> set[int]:
    return {json.loads(server.get("/").body)["import_pid"] for _ in range(count)}


def test_hooks_run_in_the_host_and_in_each_worker(hooked: support.Server) -> None:
    host = str(hooked.host_pid())
    for name in ("post_fork", "on_ready"):
        assert hooked.wait_for_log(f"HOOK {name} "), name
    on_load = hook_lines(hooked, "on_load")
    pre_fork = hook_lines(hooked, "pre_fork")
    post_fork = hook_lines(hooked, "post_fork")
    assert [line["pid"] for line in on_load] == [host]
    assert [line["pid"] for line in pre_fork] == [host]
    assert on_load[0]["worker"] == "None"
    assert on_load[0]["app"] == "preload-wsgi"
    assert len(post_fork) == WORKERS
    assert {line["worker"] for line in post_fork} == {"0", "1"}
    assert all(line["pid"] != host for line in post_fork)
    assert len(hook_lines(hooked, "on_ready")) == WORKERS

    # Preloaded: every worker runs the application the host imported.

    assert import_pids(hooked, WORKERS * 8) == {int(host)}
    info = json.loads(hooked.get("/").body)
    assert info["worker_id"] in (0, 1)
    assert info["app"] == "preload-wsgi"

    # The workers run this interpreter fresh, so the build decides, not
    # whether an extension re-enabled the GIL in this process.

    free_threaded = sysconfig.get_config_var("Py_GIL_DISABLED") == 1
    assert info["process_model"] == ("free-threaded" if free_threaded else "threaded")


def test_worker_output_is_tagged_with_the_worker(hooked: support.Server) -> None:
    # Section 11.2: a worker's stderr lines carry its index; its stdout
    # lines name the stream as well. The host's own lines are untagged.

    assert hooked.wait_for_log("HOOK post_fork ")
    log = hooked.log()
    assert any(line.startswith("muru.worker[0]: HOOK post_fork ") for line in log.splitlines())
    assert any(line.startswith("muru.worker[1]: HOOK post_fork ") for line in log.splitlines())
    assert any(line.startswith("HOOK on_load ") for line in log.splitlines())
    hooked.mark_log()
    assert hooked.get("/print").status == 200
    assert hooked.wait_for_log(".stdout: printed by the application")


def test_sigusr1_runs_on_reopen_logs(hooked: support.Server) -> None:
    assert hooked.wait_for_log("HOOK on_ready ")
    pid = hooked.worker_pids()[0]
    hooked.mark_log()
    os.kill(pid, signal.SIGUSR1)
    assert hooked.wait_for_log(f"HOOK on_reopen_logs pid={pid} worker=0")
    assert hooked.get("/").status == 200


def test_drain_and_exit_run_on_shutdown(hooked: support.Server) -> None:
    assert hooked.wait_for_log("HOOK on_ready ")
    hooked.stop()
    assert len(hook_lines(hooked, "on_drain")) == WORKERS
    assert len(hook_lines(hooked, "on_exit")) == WORKERS

    for name in ("on_drain", "on_exit"):
        for line in hook_lines(hooked, name):
            assert line["pid"] != str(hooked.host_pid())


def test_unsafe_import_falls_back_to_per_worker_imports() -> None:
    server = support.start(
        "unsafe_import_wsgi:application", workers=WORKERS, args=["--hooks", "hooks_log"]
    )

    try:
        log = server.log()
        assert "preload: the import of unsafe_import_wsgi left a connected socket" in log
        assert "falling back to per-worker imports" in log
        pids = import_pids(server, WORKERS * 8)
        assert pids == set(server.worker_pids().values())
        assert server.host_pid() not in pids

        # on_load and pre_fork ran once in the host before the check, then
        # run again in each worker, before its post_fork.

        for name in ("on_load", "pre_fork"):
            # A worker's lines reach the log through its relay, a moment
            # after the worker has written them.

            deadline = time.monotonic() + 10

            while len(hook_lines(server, name)) < WORKERS + 1 and time.monotonic() < deadline:
                time.sleep(0.05)

            lines = hook_lines(server, name)
            assert lines[0] == {
                "pid": str(server.host_pid()),
                "worker": "None",
                "app": "unsafe-import-wsgi",
            }, name
            assert sorted(line["worker"] for line in lines[1:]) == ["0", "1"], name
    finally:
        server.stop()


def test_preload_true_fails_on_an_unsafe_import() -> None:
    with pytest.raises(RuntimeError) as excinfo:
        support.start("unsafe_import_wsgi:application", args=["--preload", "true"])
    message = str(excinfo.value)
    assert "left a connected socket" in message
    assert "preload is true" in message


def test_what_pre_fork_closes_passes_the_import_check() -> None:
    # Section 6.2: pre_fork runs before the check, so a socket the import
    # left open and pre_fork closes is no finding, even with preload true.

    server = support.start(
        "unsafe_import_wsgi:application",
        workers=WORKERS,
        args=["--preload", "true", "--hooks", "hooks_reopen_socket"],
    )

    try:
        assert "preload:" not in server.log()
        assert import_pids(server, WORKERS * 8) == {server.host_pid()}
    finally:
        server.stop()


def test_preload_false_imports_in_each_worker() -> None:
    server = support.start("preload_wsgi:application", workers=WORKERS, args=["--preload", "false"])

    try:
        assert "preloaded" not in server.log()
        assert import_pids(server, WORKERS * 8) == set(server.worker_pids().values())
    finally:
        server.stop()


INSTRUMENT = """
import json, os, sys


def configure(settings):
    print(f"INSTRUMENT configure {__name__} pid={os.getpid()} {json.dumps(settings)}",
          file=sys.stderr, flush=True)
"""

SEES = """
import json, os


def application(environ, start_response):
    body = json.dumps({"listed": "MURU_INSTRUMENTATION" in os.environ}).encode()
    start_response("200 OK", [("Content-Type", "application/json")])
    return [body]
"""


def test_instrumentation_modules_listed_in_the_project_run_before_the_application(
    tmp_path: Path,
) -> None:
    # Section 6.15: muru.local.toml's module first, then the project's,
    # each configured with its own settings in the host, before the hooks
    # module; the application never sees the list.

    (tmp_path / "local_tool.py").write_text(INSTRUMENT)
    (tmp_path / "project_tool.py").write_text(INSTRUMENT)
    (tmp_path / "sees.py").write_text(SEES)
    shutil.copy(support.FIXTURES / "hooks_log.py", tmp_path)
    (tmp_path / "pyproject.toml").write_text(
        "[tool.muru]\n"
        "entrypoint = 'sees:application'\n"
        "hooks = 'hooks_log'\n\n"
        "[[tool.muru.instrumentation]]\n"
        "module = 'project_tool'\n"
        "settings = { output = 'times.json', top = 5 }\n"
    )
    (tmp_path / "muru.local.toml").write_text("[[instrumentation]]\nmodule = 'local_tool'\n")
    server = support.start(None, chdir=tmp_path, workers=WORKERS)

    try:
        host = server.host_pid()
        lines = [
            line[line.find("INSTRUMENT ") :]
            if "INSTRUMENT " in line
            else line[line.find("HOOK ") :]
            for line in server.log().splitlines()
            if "INSTRUMENT " in line or "HOOK on_load" in line
        ]
        assert lines[:3] == [
            f"INSTRUMENT configure local_tool pid={host} {{}}",
            f'INSTRUMENT configure project_tool pid={host} {{"output": "times.json", "top": 5}}',
            f"HOOK on_load pid={host} worker=None app=sees",
        ], server.log()
        assert json.loads(server.get("/").body) == {"listed": False}
    finally:
        server.stop()


WRAP_TOOL = """
import os, sys


def wrap_application(context, target, application, interface):
    print(f"WRAP {target.kind} {target.name or '-'} {interface} pid={os.getpid()}",
          file=sys.stderr, flush=True)

    if interface == "wsgi":
        def wrapped(environ, start_response):
            def starting(status, headers, exc_info=None):
                return start_response(status, [*headers, ("X-Wrapped", "wsgi")], exc_info)

            return application(environ, starting)

        return wrapped

    async def wrapped_asgi(scope, receive, send):
        if scope["type"] == "lifespan":
            print("WRAP lifespan", file=sys.stderr, flush=True)
            return await application(scope, receive, send)

        async def sending(message):
            if message["type"] == "http.response.start":
                headers = [*message.get("headers", []), (b"x-wrapped", b"asgi")]
                message = {**message, "headers": headers}

            await send(message)

        return await application(scope, receive, sending)

    return wrapped_asgi
"""

WRAPPED_APP = """
def make():
    def application(environ, start_response):
        start_response("200 OK", [("Content-Type", "text/plain")])
        return [b"plain"]

    return application


async def api(scope, receive, send):
    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            else:
                await send({"type": "lifespan.shutdown.complete"})
                return

    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"api"})
"""


def wrap_lines(server: support.Server) -> list[str]:
    return [line[line.find("WRAP ") :] for line in server.log().splitlines() if "WRAP " in line]


def test_wrap_application_wraps_every_entry_point_in_each_worker(tmp_path: Path) -> None:
    # Section 6.15: a host-phase factory and an ASGI mount, preloaded,
    # then a worker-phase factory imported in each worker; each entry
    # point's application is wrapped in the worker, lifespan included.

    (tmp_path / "wrap_tool.py").write_text(WRAP_TOOL)
    (tmp_path / "wrapped_app.py").write_text(WRAPPED_APP)
    project = (
        "[tool.muru]\nentrypoint = 'wrapped_app:make'\nfactory = true\n"
        "factory_phase = '{phase}'\npreload = '{preload}'\n{mount}"
        "[[tool.muru.instrumentation]]\nmodule = 'wrap_tool'\n"
    )
    mount = (
        "[tool.muru.mounts.api]\npath = '/api'\nentrypoint = 'wrapped_app:api'\n"
        "interface = 'asgi'\n"
    )
    (tmp_path / "pyproject.toml").write_text(
        project.format(phase="host", preload="auto", mount=mount)
    )
    server = support.start(None, chdir=tmp_path, workers=WORKERS)

    try:
        assert server.get("/").header("X-Wrapped") == "wsgi"
        assert server.get("/api/").header("X-Wrapped") == "asgi"
        lines = wrap_lines(server)
        workers = {f"pid={pid}" for pid in server.worker_pids().values()}
        entrypoints = [line for line in lines if line.startswith("WRAP entrypoint - wsgi")]
        mounts = [line for line in lines if line.startswith("WRAP mount api asgi")]
        assert {line.split()[-1] for line in entrypoints} == workers, lines
        assert {line.split()[-1] for line in mounts} == workers, lines
        assert lines.count("WRAP lifespan") == WORKERS, lines
    finally:
        server.stop()

    (tmp_path / "pyproject.toml").write_text(
        project.format(phase="worker", preload="false", mount="")
    )
    server = support.start(None, chdir=tmp_path, workers=1)

    try:
        assert server.get("/").header("X-Wrapped") == "wsgi"
        assert wrap_lines(server) == [f"WRAP entrypoint - wsgi pid={server.worker_pids()[0]}"]
    finally:
        server.stop()


def test_failing_on_load_hook_stops_the_host(tmp_path: os.PathLike[str]) -> None:
    with pytest.raises(RuntimeError) as excinfo:
        support.start("preload_wsgi:application", args=["--hooks", "hooks_failing_load"])
    assert "the on_load hook failed" in str(excinfo.value)
    assert "load-boom" in str(excinfo.value)


def test_single_serves_in_the_host_process_without_forking() -> None:
    server = support.start(
        "preload_wsgi:application", workers=3, threads=4, args=["--single", "--hooks", "hooks_log"]
    )

    try:
        assert server.wait_for_log("serving in this process without forking")
        info = json.loads(server.get("/").body)
        host = server.host_pid()
        assert info["pid"] == host
        assert info["import_pid"] == host
        assert info["worker_id"] == 0
        assert info["process_model"] in ("process-per-request", "free-threaded")
        assert server.worker_pids() == {}

        # Hooks still run, in this one process; nothing forks, so pre_fork
        # has no occasion.

        for name in ("on_load", "post_fork", "on_ready"):
            assert [line["pid"] for line in hook_lines(server, name)] == [str(host)], name
        assert hook_lines(server, "pre_fork") == []
    finally:
        server.stop()
