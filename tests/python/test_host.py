"""Unit tests for the host process helpers."""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import textwrap
import time
from pathlib import Path
from typing import Any

import pytest

from muru import _outside, host

FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "apps"


@pytest.fixture
def fixtures_on_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.syspath_prepend(str(FIXTURES))


def test_resolve_app_default_attribute(fixtures_on_path: None) -> None:
    app = host.resolve_app("plain_wsgi")
    assert callable(app)


def test_resolve_app_explicit_attribute(fixtures_on_path: None) -> None:
    assert host.resolve_app("plain_wsgi:application") is host.resolve_app("plain_wsgi")


def test_resolve_app_rejects_non_callable(fixtures_on_path: None) -> None:
    with pytest.raises(TypeError):
        host.resolve_app("plain_wsgi:HELLO")


def test_resolve_app_rejects_empty_module() -> None:
    with pytest.raises(ValueError, match="module:callable"):
        host.resolve_app(":application")


def test_main_requires_arguments() -> None:
    with pytest.raises(SystemExit) as excinfo:
        host.main([])
    assert excinfo.value.code == 2


def test_main_reports_unloadable_application(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    status = host.main(["--app", "no_such_module:application", "--socket-dir", str(tmp_path)])
    assert status == 1
    assert "cannot load no_such_module:application" in capfd.readouterr().err


def test_describe_status() -> None:
    assert host.describe(0) == "status 0"
    assert "signal" in host.describe(9) or "status" in host.describe(9)


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX only")
def test_python_is_posix() -> None:
    assert hasattr(host.os, "fork")


def test_socket_paths_carry_the_generation_of_a_replacement(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=2, threads=1)
    assert instance.socket_path(1) == str(tmp_path / "worker-1.sock")
    instance.generation[1] = 2
    assert instance.socket_path(1) == str(tmp_path / "worker-1-g2.sock")
    assert instance.socket_path(0) == str(tmp_path / "worker-0.sock")


def test_a_group_recycles_by_its_own_settings_else_the_releases(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    name, recycle = host.parse_group_recycle("api=5,off")
    api = host.Group(name, 1, 1, 1, 1, **recycle)
    batch = host.Group("batch", 1, 1, 1, 1)
    instance = host.Host(
        release,
        str(tmp_path),
        workers=1,
        threads=1,
        groups=[api, batch],
        max_requests=100,
        max_lifetime=60.0,
    )
    settled = {group.name: (group.max_requests, group.max_lifetime) for group in instance.groups}
    assert settled == {"default": (100, 60.0), "api": (5, 60.0), "batch": (100, 60.0)}

    for text in ("api=0,off", "api=5", "api=many,off", "api=off,0"):
        with pytest.raises(argparse.ArgumentTypeError):
            host.parse_group_recycle(text)


def test_workers_message_lists_the_routed_sockets_by_group(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    api = host.Group("api", 1, 1, 1, 1)
    instance = host.Host(release, str(tmp_path), workers=2, threads=1, groups=[api])
    instance.routed = {0: "/a", 1: None, 2: "/api-0"}
    instance.version = 3
    assert instance.workers_message() == {
        "type": "workers",
        "version": 3,
        "sockets": ["/a"],
        "groups": [{"name": "api", "sockets": ["/api-0"]}],
        "interfaces": {},
        "workers": [],
    }

    # The interfaces the host settled go with it, the default mount's
    # under the empty name; a mount its workers settle is left out.

    context = host.Context(app="plain_wsgi")
    settled = host.Mount.parse("ws=/ws=plain_asgi:application", context, 1)
    unsettled = host.Mount.parse("other=/other=plain_wsgi:application", context, 1)
    release.resolved = "wsgi"
    settled.release.resolved = "asgi"
    instance.mounts = [settled, unsettled]
    assert instance.workers_message()["interfaces"] == {"": "wsgi", "ws": "asgi"}

    # What a worker settled and reported is announced with the host's
    # own, and is news only the first time.

    assert instance.settle({"other": "wsgi", "odd": "soap"})
    assert not instance.settle({"other": "wsgi"})
    assert not instance.settle("wsgi")
    assert instance.workers_message()["interfaces"] == {"": "wsgi", "ws": "asgi", "other": "wsgi"}


def test_a_mount_takes_threads_of_its_own() -> None:
    context = host.Context(app="shop")
    mount = host.Mount.parse("reports=/reports=shop:reports=wsgi=2,6", context, 4)
    assert (mount.name, mount.path, mount.threads) == ("reports", "/reports", (2, 6))
    assert host.Mount.parse("reports=/reports=shop:reports", context, 4).threads is None

    for text in (
        "reports=/reports=shop:reports=wsgi=2",
        "reports=/reports=shop:reports=wsgi=3,2",
        "reports=/reports=shop:reports=wsgi=0,0",
        "reports=/reports=shop:reports=wsgi=-1,2",
    ):
        with pytest.raises(argparse.ArgumentTypeError):
            host.Mount.parse(text, context, 4)


def test_a_mount_takes_limits_of_its_own_whether_declared_or_left_to_be_detected() -> None:
    context = host.Context(app="shop")

    # The limits follow the threads, which may be left empty.

    detected = host.Mount.parse("live=/live=shop:live=auto==500,2000", context, 4)
    assert (detected.threads, detected.limits) == (None, (500, 2000))
    declared = host.Mount.parse("live=/live=shop:live=asgi==500,0", context, 4)
    assert (declared.threads, declared.limits) == (None, (500, 0))
    both = host.Mount.parse("live=/live=shop:live=auto=4,4=500,0", context, 4)
    assert (both.threads, both.limits) == ((4, 4), (500, 0))

    for text in (
        "live=/live=shop:live=wsgi==500,0",
        "live=/live=shop:live=auto==500",
        "live=/live=shop:live=auto==-1,0",
        "live=/live=shop:live=auto=",
    ):
        with pytest.raises(argparse.ArgumentTypeError):
            host.Mount.parse(text, context, 4)


def test_a_restart_is_refused_for_a_worker_the_host_cannot_replace(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)
    answers: list[dict[str, object]] = []
    instance.send_to_supervisor = answers.append  # type: ignore[method-assign]

    instance.take_request({"id": 1, "op": "restart", "args": {}})
    assert answers[-1] == {
        "type": "response",
        "id": 1,
        "workers": [],
        "error": "a worker index is needed",
    }

    instance.take_request({"id": 2, "op": "restart", "worker": 3, "args": {"force": True}})
    assert answers[-1]["error"] == "no worker 3"


def test_a_request_past_its_deadline_is_one_event_until_it_ends(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)
    sent: list[dict[str, object]] = []
    instance.send_to_supervisor = sent.append  # type: ignore[method-assign]
    state = host.WorkerState(0, 4242, 0, "w0.sock", time.monotonic(), -1)
    request = {"request_id": "r-1", "method": "GET", "path": "/hang", "age_ms": 300100}

    # Reported with every report while it lasts, it is raised once.

    for _ in range(3):
        instance.saw_past_deadline(state, request)
        instance.raise_past_deadline(state, time.monotonic())

    events = [message for message in sent if message.get("kind") == "request.past_deadline"]
    assert len(events) == 1
    assert "request r-1 GET /hang is past its deadline, 300.1 s" in str(events[0]["message"])

    # Once the worker stops reporting it, it is forgotten.

    later = (
        time.monotonic() + (host.PAST_DEADLINE_FORGET_REPORTS + 1) * host.REPORT_INTERVAL_SECONDS
    )
    instance.raise_past_deadline(state, later)
    assert state.past_deadline == {}


def replacing_host(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, spawned: int = 5001
) -> tuple[host.Host, host.WorkerState, list[dict[str, Any]], list[host.Pending]]:
    """A host with one worker, nothing forked, and every ask held for the test to answer."""

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)
    sent: list[dict[str, Any]] = []
    asked: list[host.Pending] = []

    def ask(
        state: host.WorkerState, op: str, args: dict[str, Any], timeout: float, then: Any
    ) -> None:
        asked.append(host.Pending(0, op, {state.pid: state.index}, then=then))

    state = host.WorkerState(0, 4242, 0, "w0.sock", time.monotonic(), -1)
    instance.states[state.pid] = state
    instance.workers[state.pid] = state.index
    monkeypatch.setattr(_outside, "WHOLE", False)
    instance.control_listener = object()  # type: ignore[assignment]
    instance.send_to_supervisor = sent.append  # type: ignore[method-assign]
    instance.ask = ask  # type: ignore[method-assign]
    instance.spawn = lambda index, replacing=False: spawned  # type: ignore[method-assign]
    return instance, state, sent, asked


def exits(monkeypatch: pytest.MonkeyPatch, *statuses: tuple[int, int]) -> None:
    """Have os.waitpid report these (pid, status) pairs, then nothing."""

    waiting = list(statuses)
    monkeypatch.setattr(os, "waitpid", lambda _pid, _flags: waiting.pop(0) if waiting else (0, 0))


def test_a_timeout_kill_is_reported_at_the_decision_and_not_again_at_the_exit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capfd: pytest.CaptureFixture[str]
) -> None:
    instance, state, sent, asked = replacing_host(tmp_path, monkeypatch)
    instance.replace(state, "a request ran too long", "worker.timeout_killed", "hard_timeout")

    # Waiting only for the stacks, at most a second, not the drain grace.

    assert not [message for message in sent if message.get("type") == "event"]
    stacks = next(pending for pending in asked if pending.op == "stacks")
    stacks.answers[4242] = {
        "ok": True,
        "result": {
            "threads": [
                {
                    "name": "t",
                    "request": {"elapsed_ms": 9},
                    "frames": [{"file": "/srv/app.py", "line": 13, "function": "application"}],
                }
            ]
        },
    }
    assert stacks.then is not None
    stacks.then(stacks)
    (event,) = [message for message in sent if message.get("type") == "event"]
    assert event["kind"] == "worker.timeout_killed"
    assert event["reason"] == "hard_timeout"
    assert event["message"] == (
        "worker 0 (pid 4242) replaced by pid 5001: a request ran too long; "
        "at app.py line 13 in application"
    )

    # The old worker going is a line in the log and no second event.

    exits(monkeypatch, (4242, 0))
    instance.reap()
    assert len([message for message in sent if message.get("type") == "event"]) == 1
    assert "worker 0 (pid 4242) exited after its drain grace" in capfd.readouterr().err


def kinds(sent: list[dict[str, Any]]) -> list[object]:
    return [message["kind"] for message in sent if message.get("type") == "event"]


def test_each_kill_for_memory_is_one_oom_killed_exit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "memory.events"
    events.write_text("oom 0\noom_kill 0\n")
    instance, _state, sent, _asked = replacing_host(tmp_path, monkeypatch)
    instance.cgroup = str(tmp_path)
    instance.oom_kills = 0
    instance.workers[4243] = 0

    # Two workers killed together: the first reaped sees the count at 2.

    events.write_text("oom 2\noom_kill 2\n")
    exits(monkeypatch, (4242, signal.SIGKILL), (4243, signal.SIGKILL))
    instance.reap()
    assert kinds(sent) == ["worker.oom_killed", "worker.oom_killed"]
    assert instance.oom_kills == 2

    # With every kill put down to a worker, a SIGKILL is someone else's.

    instance.workers[4244] = 0
    exits(monkeypatch, (4244, signal.SIGKILL))
    instance.reap()
    assert kinds(sent)[-1] == "worker.exited"


def test_a_kill_for_memory_during_a_replacement_is_counted_and_reported(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "memory.events"
    events.write_text("oom_kill 0\n")
    instance, state, sent, _asked = replacing_host(tmp_path, monkeypatch)
    instance.cgroup = str(tmp_path)
    instance.oom_kills = 0

    # The kernel takes the condemned worker: not a recycle, and the kill
    # is used up, so it cannot be put on a later exit.

    instance.replace(state, "over the limit", "worker.recycled", "memory")
    events.write_text("oom_kill 1\n")
    exits(monkeypatch, (4242, signal.SIGKILL))
    instance.reap()
    assert kinds(sent) == ["worker.oom_killed"]
    assert sent[-1]["reason"] == "oom_killed"
    assert instance.oom_kills == 1

    # A SIGKILL the host sent itself, past the stop budget, leaves a kill
    # the cgroup counted for the exit it belongs to.

    state = host.WorkerState(0, 4300, 0, "w0.sock", time.monotonic(), -1)
    instance.states[4300] = state
    instance.workers[4300] = 0
    instance.replace(state, "served its share", "worker.recycled", "max_requests")
    instance.replacing[0].killed = True
    events.write_text("oom_kill 2\n")
    exits(monkeypatch, (4300, signal.SIGKILL))
    instance.reap()
    assert kinds(sent)[-1] == "worker.recycled"
    assert instance.oom_kills == 1


def container_memory(cgroup: Path, used: int, inactive: int = 0, limit: int = 1000) -> None:
    (cgroup / "memory.max").write_text(f"{limit}\n")
    (cgroup / "memory.current").write_text(f"{used}\n")
    (cgroup / "memory.stat").write_text(f"anon 1\ninactive_file {inactive}\n")


def test_memory_left_counts_reclaimable_file_pages_and_needs_a_limit(tmp_path: Path) -> None:
    assert host.memory_left(None) is None
    container_memory(tmp_path, used=800, inactive=300)
    assert host.memory_left(str(tmp_path)) == 500
    (tmp_path / "memory.max").write_text("max\n")
    assert host.memory_left(str(tmp_path)) is None


def test_a_worker_short_of_room_stops_before_its_replacement_is_forked(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (tmp_path / "memory.events").write_text("oom_kill 0\n")
    container_memory(tmp_path, used=800, limit=1000)
    instance, state, sent, _asked = replacing_host(tmp_path, monkeypatch)
    forked: list[int] = []

    def spawn(index: int, replacing: bool = False) -> int:
        assert replacing
        forked.append(index)
        return 5001

    instance.cgroup = str(tmp_path)
    state.memory = {"uss": 300}
    instance.spawn = spawn  # type: ignore[method-assign]
    instance.routed[0] = state.path

    # 200 is left, less than the worker's 300, with or without a hard
    # limit: it leaves routing and nothing is forked while it holds its
    # memory.

    instance.replace(state, "uss is over the hard limit", "worker.recycled", "memory", hard=True)
    replacement = instance.replacing[0]
    assert forked == [] and replacement.stop_first and replacement.terminate_at is None
    assert instance.routed[0] is None and replacement.announced is not None

    # Told to stop once the edge has moved off it, then replaced on exit.

    instance.acked = replacement.announced
    instance.advance(replacement, time.monotonic())
    assert replacement.terminate_at is not None and not replacement.terminated
    exits(monkeypatch, (4242, 0))
    instance.reap()
    assert forked == [0] and 0 in instance.starting
    (event,) = [message for message in sent if message.get("kind") == "worker.recycled"]
    assert "replaced by pid 5001" in str(event["message"])


def test_a_worker_with_room_or_condemned_for_something_else_is_replaced_first(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    container_memory(tmp_path, used=200, limit=1000)
    instance, state, _sent, _asked = replacing_host(tmp_path, monkeypatch)
    instance.cgroup = str(tmp_path)
    state.memory = {"uss": 300}

    instance.replace(state, "over the limit", "worker.recycled", "memory")
    assert not instance.replacing[0].stop_first and instance.replacing[0].new_pid == 5001

    container_memory(tmp_path, used=900, limit=1000)
    instance.replacing.clear()
    state.replaced = False
    instance.replace(state, "served its share", "worker.recycled", "max_requests")
    assert not instance.replacing[0].stop_first and instance.replacing[0].new_pid == 5001


def test_the_memory_guard_has_a_worker_recycled_and_stopped_first(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    container_memory(tmp_path, used=950, limit=1000)
    instance, state, sent, _asked = replacing_host(tmp_path, monkeypatch)
    instance.cgroup = str(tmp_path)
    state.ready = True
    state.memory = {"uss": 300}

    # A worker replaced since it was chosen is left alone.

    instance.recycle_worker(7, 0, "0", {"pid": 9999, "reason": "short"})
    assert sent[-1]["error"] == "no worker 0 with pid 9999"
    assert not instance.replacing

    instance.recycle_worker(
        8,
        0,
        "0",
        {"pid": 4242, "reason": "the container was short of memory", "cause": "container_memory"},
    )
    assert sent[-1] == {"type": "response", "id": 8, "workers": [sent[-1]["workers"][0]]}
    replacement = instance.replacing[0]
    assert replacement.kind == "worker.recycled" and replacement.cause == "container_memory"
    assert replacement.stop_first and replacement.new_pid is None


def test_arguments_are_checked_against_the_interface(monkeypatch: pytest.MonkeyPatch) -> None:
    def wsgi(environ: object, start_response: object) -> None: ...
    def factory() -> None: ...
    def factory_with_defaults(config: object = None) -> None: ...
    def three(scope: object, receive: object, send: object) -> None: ...
    def any_args(*args: object, **kwargs: object) -> None: ...
    def keyword(environ: object, start_response: object, *, extra: object) -> None: ...

    async def asgi(scope: object, receive: object, send: object) -> None: ...

    async def two_async(scope: object, receive: object) -> None: ...

    assert host.resolve_interface("m:wsgi", wsgi, "auto") == "wsgi"
    assert host.resolve_interface("m:asgi", asgi, "auto") == "asgi"
    assert host.resolve_interface("m:any", any_args, "auto") == "wsgi"
    assert host.resolve_interface("m:three", three, "asgi") == "asgi"

    with pytest.raises(RuntimeError, match="set factory = true"):
        host.resolve_interface("m:factory", factory, "auto")

    with pytest.raises(RuntimeError, match="set factory = true"):
        host.resolve_interface("m:factory", factory_with_defaults, "auto")

    with pytest.raises(RuntimeError, match="as an ASGI application does, but is not a coroutine"):
        host.resolve_interface("m:three", three, "auto")

    with pytest.raises(RuntimeError, match="declared ASGI but takes 2 arguments"):
        host.resolve_interface("m:wsgi", wsgi, "asgi")

    with pytest.raises(RuntimeError, match="declared WSGI but takes 3 arguments"):
        host.resolve_interface("m:three", three, "wsgi")

    with pytest.raises(RuntimeError, match="coroutine function, so it cannot be served as WSGI"):
        host.resolve_interface("m:two", two_async, "auto")

    with pytest.raises(RuntimeError, match="keyword-only parameter 'extra'"):
        host.resolve_interface("m:keyword", keyword, "auto")

    # What Python cannot read the signature of is taken as it is.

    monkeypatch.setattr(host.inspect, "signature", lambda app: (_ for _ in ()).throw(ValueError()))
    assert host.resolve_interface("m:factory", factory, "auto") == "wsgi"


def test_what_a_dead_worker_left_is_stopped_then_killed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1, shutdown_timeout=2.0)
    instance.subreaper = True
    instance.workers = {4243: 0}
    sent: list[tuple[int, int]] = []
    instance.signal = lambda pid, signum: sent.append((pid, signum))  # type: ignore[method-assign]

    # The live worker is not an orphan; the other child the scan finds is.

    monkeypatch.setattr(host, "children_of", lambda pid: {4242, 4243})
    instance.adopt_orphans(0, 4000)
    assert list(instance.orphans) == [4242]
    assert instance.orphans[4242].left_by == 0

    # Section 6.10: the shutdown timeout to exit on its own, then SIGTERM,
    # then as long again before SIGKILL.

    now = time.monotonic()
    instance.advance_orphans(now + 1.0)
    assert sent == []
    instance.advance_orphans(now + 2.1)
    assert sent == [(4242, signal.SIGTERM)]
    instance.advance_orphans(now + 3.0)
    assert sent == [(4242, signal.SIGTERM)]
    instance.advance_orphans(now + 4.2)
    assert sent == [(4242, signal.SIGTERM), (4242, signal.SIGKILL)]

    # Adopting again finds nothing new, and a host that is no subreaper
    # adopts nothing at all.

    instance.adopt_orphans(0, 4000)
    assert list(instance.orphans) == [4242]
    instance.subreaper = False
    monkeypatch.setattr(host, "children_of", lambda pid: {4242, 4243, 4244})
    instance.adopt_orphans(0, 4000)
    assert list(instance.orphans) == [4242]


def test_become_subreaper_is_linux_only() -> None:
    if sys.platform == "linux":
        pytest.skip("on Linux the subreaper takes; the test below covers it")
    assert host.become_subreaper() is False
    assert host.children_of(os.getpid()) == set()


@pytest.mark.requires_linux
def test_a_subreaper_sees_orphaned_grandchildren_as_its_children() -> None:
    # In a process of its own, since the subreaper setting is for life.

    script = textwrap.dedent(
        """
        import os
        import signal
        import time

        from muru import host

        assert host.become_subreaper()
        read_end, write_end = os.pipe()
        pid = os.fork()

        if pid == 0:
            grandchild = os.fork()

            if grandchild == 0:
                time.sleep(60)
                os._exit(0)

            os.write(write_end, str(grandchild).encode())
            os._exit(0)

        os.waitpid(pid, 0)
        grandchild = int(os.read(read_end, 32))
        deadline = time.monotonic() + 5

        while grandchild not in host.children_of(os.getpid()) and time.monotonic() < deadline:
            time.sleep(0.01)

        print("adopted", grandchild in host.children_of(os.getpid()))
        os.kill(grandchild, signal.SIGTERM)
        print("collected", os.waitpid(grandchild, 0)[0] == grandchild)
        print("gone", grandchild not in host.children_of(os.getpid()))
        """
    )
    result = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        check=False,
        env={**os.environ, "PYTHONPATH": str(Path(host.__file__).resolve().parents[1])},
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.split() == ["adopted", "True", "collected", "True", "gone", "True"]


@pytest.mark.parametrize("whole", [True, False])
def test_a_worker_that_does_not_answer_for_stacks_is_read_from_outside_on_3_14(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, whole: bool
) -> None:
    """On 3.14 the worker is asked first, since reading from outside stops
    at the first call from C; one that does not answer is read from
    outside after all. Where the reading gives every frame it was read
    first, so a worker left unanswered stays so."""

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)
    signals: list[tuple[int, int]] = []
    answers: list[dict[str, Any]] = []
    frames = [{"file": "/srv/app.py", "line": 3, "function": "application"}]
    monkeypatch.setattr(_outside, "WHOLE", whole)
    instance.signal = lambda pid, signum: signals.append((pid, signum))  # type: ignore[method-assign]
    instance.read_outside = lambda pid: [(7, frames, None)]  # type: ignore[method-assign]
    instance.send_to_supervisor = answers.append  # type: ignore[method-assign]

    pending = host.Pending(1, "stacks", {4321: 0})
    instance.pending[1] = pending
    instance.finish_if_answered(pending, expired=True)

    assert signals == [(4321, signal.SIGQUIT)]
    (answer,) = answers[-1]["workers"]

    if whole:
        assert not answer["ok"] and "did not answer" in answer["error"]
    else:
        assert answer["ok"] and answer["result"]["outside"]
        assert answer["result"]["threads"][0]["frames"][0]["function"] == "application"


def test_a_shutdown_with_a_budget_of_its_own_leaves_the_workers_less_time(
    tmp_path: Path,
) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(
        release, str(tmp_path), workers=1, threads=1, shutdown_timeout=5.0, exit_grace=2.0
    )

    # No file: the worker's own stop budget.

    assert instance.time_to_stop() == 7.0

    # What the shutdown leaves is less than the budget and comes off it,
    # more than the budget is no reason to take longer.

    stop_by = tmp_path / host.STOP_BY_FILE
    stop_by.write_text(str(int((time.time() + 3.0) * 1000)))
    assert 2.5 < instance.time_to_stop() <= 3.0
    stop_by.write_text(str(int((time.time() + 60.0) * 1000)))
    assert instance.time_to_stop() == 7.0

    # Past already, or unreadable.

    stop_by.write_text(str(int((time.time() - 1.0) * 1000)))
    assert instance.time_to_stop() == 0.0
    stop_by.write_text("not a time")
    assert instance.time_to_stop() == 7.0
