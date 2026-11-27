"""Unit tests for the host process helpers."""

from __future__ import annotations

import argparse
import io
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
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


def test_a_named_groups_sockets_carry_its_place_and_never_its_name(tmp_path: Path) -> None:
    # No socket's path grows with a name (design specification section
    # 13.8): the first named group is 1, the second 2.

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    groups = [host.Group("api", 2, 1, 1, 1), host.Group("a-long-name-for-a-group", 1, 1, 1, 1)]
    instance = host.Host(release, str(tmp_path), workers=1, threads=1, groups=groups)

    assert instance.socket_path(0) == str(tmp_path / "worker-0.sock")
    assert instance.socket_path(2) == str(tmp_path / "worker-1-1.sock")
    assert instance.socket_path(3) == str(tmp_path / "worker-2-0.sock")
    instance.generation[3] = 4
    assert instance.socket_path(3) == str(tmp_path / "worker-2-0-g4.sock")
    assert instance.files_dir == str(tmp_path)


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


def test_each_mounts_path_hint_settings_are_read_from_one_argument() -> None:
    """Section 4.9: the roots a mount's workers open hinted files beneath,
    and its second header, by mount name, the default mount's under ""."""

    parsed = host.parse_sendfile(
        '{"": {"roots": ["/srv/media"], "owned": {"/srv/media": 1001}, "header": null},'
        ' "api": {"roots": [], "header": "X-Sendfile"}}'
    )
    assert parsed == [
        ("", ["/srv/media"], None, {"/srv/media": 1001}),
        ("api", [], "X-Sendfile", {}),
    ]
    assert host.parse_args(["--app", "a:b", "--socket-dir", "/tmp"]).sendfile == []

    for text in (
        "[]",
        "not json",
        '{"": {"roots": ["media"]}}',
        '{"": {"roots": [], "header": 1}}',
        '{"": {"roots": [], "owned": {"/srv/media": 1001}}}',
        '{"": {"roots": ["/srv/media"], "owned": {"/srv/media": "x"}}}',
    ):
        with pytest.raises(argparse.ArgumentTypeError):
            host.parse_sendfile(text)


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

    # Issue 706: the worker gave no stacks here, so the evidence names the
    # request alone.

    assert events[0]["evidence"] == {"worker": "0", "pid": 4242, "requests": ["r-1"], "threads": []}

    # Once the worker stops reporting it, it is forgotten, and its record
    # is told it ended.

    later = (
        time.monotonic() + (host.PAST_DEADLINE_FORGET_REPORTS + 1) * host.REPORT_INTERVAL_SECONDS
    )
    instance.raise_past_deadline(state, later)
    assert state.past_deadline == {}
    (ended,) = [message for message in sent if message.get("type") == "capture"]
    assert ended["kind"] == "request.ended"
    assert ended["evidence"] == {"worker": "0", "pid": 4242, "requests": ["r-1"]}
    assert "has ended in worker 0 (pid 4242)" in str(ended["message"])


def test_a_relays_flush_is_answered_after_the_pipes_queued_for_it(tmp_path: Path) -> None:
    """Issue 838: the relay asks, and the answer follows every offer the
    host has queued, so the relay has the pipes by the time it is told."""

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)
    ours, relay = socket.socketpair()
    read_end, write_end = os.pipe()

    try:
        ours.setblocking(False)
        instance.relay = ours
        instance.held[0] = host.HeldPipe(0, read_end, 0, "stderr")

        # An offer the socket has not taken yet waits ahead of the answer.

        offer = b'{"pipe":0,"stream":"stderr"}\n'
        instance.relay_outgoing.append((offer, 0))
        relay.sendall(b'{"flush":3}\n')
        instance.read_relay()

        data = b""
        descriptors: list[int] = []
        relay.settimeout(5)

        while not data.endswith(b"\n") or data.count(b"\n") < 2:
            more, fds, _flags, _address = socket.recv_fds(relay, 4096, 4)
            assert more, data
            data += more
            descriptors += fds

        assert data == offer + b'{"flushed":3}\n'
        assert len(descriptors) == 1

        for fd in descriptors:
            os.close(fd)
    finally:
        ours.close()
        relay.close()
        os.close(read_end)
        os.close(write_end)


def replacing_host(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, spawned: int = 5001, workers: int = 1
) -> tuple[host.Host, host.WorkerState, list[dict[str, Any]], list[host.Pending]]:
    """A host with its first worker, nothing forked, and every ask held for the test to answer."""

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=workers, threads=1)
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


def test_a_worker_that_exits_badly_while_stopping_is_logged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capfd: pytest.CaptureFixture[str]
) -> None:
    """Issue 734: a worker that fails as the host stops says how; one
    that stops as it should says nothing."""

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=2, threads=1)
    instance.workers.update({4242: 0, 4243: 1})
    monkeypatch.setattr(os, "kill", lambda _pid, _signal: None)
    monkeypatch.setattr(instance, "stop_orphans", lambda: None)
    exits(monkeypatch, (4242, 0), (4243, signal.SIGKILL))
    instance.terminate_all()

    logged = capfd.readouterr().err
    assert "worker 1 (pid 4243) exited with signal SIGKILL" in logged
    assert "pid 4242" not in logged


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
                    "request": {"request_id": "R1", "elapsed_ms": 9},
                    "frames": [{"file": "/srv/app.py", "line": 13, "function": "application"}],
                }
            ],
            "active": [{"request_id": "R1"}, {"request_id": "R2"}],
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

    # Issue 706: the evidence names every request in flight, with the
    # stack of each thread serving one, for a development run's records.

    assert event["evidence"] == {
        "worker": "0",
        "pid": 4242,
        "reason": "a request ran too long",
        "requests": ["R1", "R2"],
        "threads": [
            {
                "name": "t",
                "request_id": "R1",
                "stack": '  File "/srv/app.py", line 13, in application',
            }
        ],
    }

    # The old worker going is a line in the log and no second event.

    exits(monkeypatch, (4242, 0))
    instance.reap()
    assert len([message for message in sent if message.get("type") == "event"]) == 1
    assert "worker 0 (pid 4242) exited after its drain grace" in capfd.readouterr().err


def test_a_worker_that_cannot_answer_for_its_stacks_still_names_its_requests(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, state, sent, asked = replacing_host(tmp_path, monkeypatch)
    instance.replace(
        state,
        "no thread of it ran Python for the GIL stall timeout of 5 s",
        "worker.timeout_killed",
        "gil_stalled",
        abandon={"which": "all", "reason": "worker_stalled", "error": "stalled"},
    )

    # Issue 715: the worker gives up its requests from its native thread,
    # but its stacks need the interpreter, and no answer comes.

    abandon = next(pending for pending in asked if pending.op == "abandon")
    abandon.answers[4242] = {"ok": True, "result": {"abandoned": ["R1", "R2"]}}
    assert abandon.then is not None
    abandon.then(abandon)
    stacks = next(pending for pending in asked if pending.op == "stacks")
    assert stacks.then is not None
    stacks.then(stacks)
    (event,) = [message for message in sent if message.get("type") == "event"]
    assert event["message"].endswith("its stacks were written to the log by faulthandler")
    assert event["evidence"] == {
        "worker": "0",
        "pid": 4242,
        "reason": "no thread of it ran Python for the GIL stall timeout of 5 s",
        "requests": ["R1", "R2"],
        "threads": [],
    }


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


def test_replacements_for_memory_never_leave_a_group_with_no_worker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Issue 662: the memory guard and a worker's hard limit act at the
    # same check, with 120 left under the limit and workers of 300.

    container_memory(tmp_path, used=880, limit=1000)
    instance, first, sent, _asked = replacing_host(tmp_path, monkeypatch, workers=2)
    second = host.WorkerState(1, 4243, 0, "w1.sock", time.monotonic(), -1)
    instance.states[second.pid] = second
    instance.workers[second.pid] = second.index
    instance.cgroup = str(tmp_path)
    instance.own_memory = dict  # type: ignore[method-assign]

    for state in (first, second):
        state.ready = True
        state.memory = {"uss": 300}
        instance.routed[state.index] = state.path

    # The guard's choice stops first, and is listed for the guard.

    reason = {"reason": "short", "cause": "container_memory"}
    instance.recycle_worker(8, 0, "0", {"pid": 4242, **reason})
    assert instance.replacing[0].stop_first and instance.routed[0] is None
    instance.send_status()
    assert sent[-1]["memory_replacements"] == [{"index": 0, "group": "default", "pid": 4242}]

    # Worker 1, over its hard limit, is the last in routing: its
    # replacement waits, and it keeps serving.

    over = "uss 300B is over the hard limit of 250B"
    assert not instance.replace(second, over, "worker.recycled", "memory", hard=True)
    assert 1 not in instance.replacing and instance.routed[1] == second.path

    # With room for its successor, it is replaced, but stays in routing
    # until the successor is ready.

    container_memory(tmp_path, used=200, limit=1000)
    assert instance.replace(second, over, "worker.recycled", "memory", hard=True)
    assert not instance.replacing[1].stop_first and instance.routed[1] == second.path

    # The guard's next ask, while a replacement for memory is under way,
    # is refused: that replacement is its one at a time.

    instance.replacing.pop(1)
    second.replaced = False
    instance.recycle_worker(9, 1, "1", {"pid": 4243, **reason})
    assert sent[-1]["error"] == (
        "worker 0 is being replaced for memory, and workers are replaced for memory one at a time"
    )


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


def test_a_worker_raises_its_oom_score_and_keeps_a_higher_one(tmp_path: Path) -> None:
    score = tmp_path / "oom_score_adj"
    score.write_text("0\n")
    host.raise_oom_score(str(score))
    assert score.read_text() == "500"

    score.write_text("800\n")
    host.raise_oom_score(str(score))
    assert score.read_text() == "800\n"

    host.raise_oom_score(str(tmp_path / "missing"))


def test_the_hosts_streams_tag_each_line_they_start(monkeypatch: pytest.MonkeyPatch) -> None:
    written = io.StringIO()
    stream = host.TaggedStream(written, ".stdout")
    tag = f"muru.host[{os.getpid()}].stdout: "

    # A line written in pieces is tagged once, and an empty line too.

    stream.write("one, ")
    stream.write("two\n\nthree\n")
    assert written.getvalue() == f"{tag}one, two\n{tag}\n{tag}three\n"

    # In a worker the relay tags the line, so the stream writes it as it is.

    monkeypatch.setattr(host, "_in_worker", True)
    stream.write("four\n")
    assert written.getvalue().endswith(f"{tag}three\nfour\n")


def test_a_worker_is_named_by_its_group_and_index(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("MURU_GROUP", raising=False)
    monkeypatch.delenv("MURU_WORKER_ID", raising=False)
    host.worker_environment("reports", 2)
    assert os.environ["MURU_GROUP"] == "reports"
    assert os.environ["MURU_WORKER_ID"] == "2"


def test_development_is_what_the_environment_says(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("MURU_DEVELOPMENT", raising=False)
    assert not host.is_development()
    monkeypatch.setenv("MURU_DEVELOPMENT", "1")
    assert host.is_development()
    monkeypatch.setenv("MURU_DEVELOPMENT", "0")
    assert not host.is_development()


# Section 7.8: swapping workers within max_surge and max_unavailable.


def swapping_host(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, workers: int = 2, **settings: int
) -> tuple[host.Host, list[host.WorkerState], list[dict[str, Any]], list[tuple[int, int]]]:
    """A host whose workers are ready and routed to, with forks, signals and exits stood in for.

    It returns the host, its workers' states in index order, what it sent
    its supervisor, and each fork as (index, pid). A forked worker has a
    state as a real one has, and is ready once `ready()` says so.
    """

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=workers, threads=1, **settings)
    sent: list[dict[str, Any]] = []
    forks: list[tuple[int, int]] = []
    states = []

    for index in range(workers):
        state = host.WorkerState(index, 4000 + index, 0, f"w{index}.sock", time.monotonic(), -1)
        state.ready = True
        state.last_report_at = time.monotonic()
        instance.states[state.pid] = state
        instance.workers[state.pid] = index
        instance.routed[index] = state.path
        states.append(state)

    def spawn(index: int, replacing: bool = False) -> int:
        pid = 5000 + len(forks)
        forks.append((index, pid))
        generation = instance.generation.get(index, 0)
        path = f"w{index}-g{generation}.sock"
        fresh = host.WorkerState(index, pid, generation, path, time.monotonic(), -1)
        fresh.last_report_at = time.monotonic()
        instance.states[pid] = fresh
        instance.workers[pid] = index

        if not replacing:
            instance.routed[index] = path

        return pid

    monkeypatch.setattr(_outside, "WHOLE", False)
    monkeypatch.setattr(os, "kill", lambda _pid, _signal: None)
    instance.control_listener = object()  # type: ignore[assignment]
    instance.send_to_supervisor = sent.append  # type: ignore[method-assign]
    instance.ask = lambda *_args, **_kwargs: None  # type: ignore[method-assign]
    instance.spawn = spawn  # type: ignore[method-assign]
    instance.own_memory = lambda: {}  # type: ignore[method-assign]
    return instance, states, sent, forks


def ready(instance: host.Host, pid: int) -> None:
    """Have the worker with this pid report for the first time."""

    state = instance.states[pid]
    state.ready = True
    instance.worker_ready(state)


def waiting(sent: list[dict[str, Any]]) -> list[tuple[object, object]]:
    return [
        (message["group"], message["reason"])
        for message in sent
        if message.get("kind") == "worker.swap_waiting"
    ]


def test_voluntary_swaps_of_a_group_take_turns_within_max_surge(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, states, sent, forks = swapping_host(tmp_path, monkeypatch)

    # Both workers are due at once. The first is replaced surge first: its
    # successor is forked while it serves. The second waits for the slot.

    instance.swap(states[0], "served 10 requests", "worker.recycled", "max_requests")
    instance.swap(states[1], "served 11 requests", "worker.recycled", "max_requests")
    instance.swap(states[1], "served 12 requests", "worker.recycled", "max_requests")
    assert forks == [(0, 5000)]
    assert instance.routed[0] == "w0.sock"
    assert waiting(sent) == [("default", "surge")]
    assert len(instance.swaps["default"]) == 1

    instance.send_status()
    assert sent[-1]["swaps_waiting"] == {"default": 1}

    # Nothing changes until the old worker has gone, however often asked.

    ready(instance, 5000)
    instance.run_swaps()
    assert forks == [(0, 5000)]
    assert waiting(sent) == [("default", "surge")]

    exits(monkeypatch, (4000, 0))
    instance.reap()
    instance.run_swaps()
    assert forks == [(0, 5000), (1, 5001)]
    assert not instance.swaps["default"]
    recycled = [message for message in sent if message.get("kind") == "worker.recycled"]
    assert recycled[0]["message"] == (
        "worker 0 (pid 4000) replaced by pid 5000: served 10 requests"
    )


def test_a_group_with_no_surge_swaps_drain_first_within_max_unavailable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, states, sent, forks = swapping_host(
        tmp_path, monkeypatch, max_surge=0, max_unavailable=1
    )
    instance.swap(states[0], "lived 60 s", "worker.recycled", "max_lifetime")
    instance.swap(states[1], "lived 61 s", "worker.recycled", "max_lifetime")

    # The first worker leaves routing and nothing is forked; the second
    # waits, since one worker of the group is out of routing already.

    assert forks == []
    assert instance.routed[0] is None
    assert instance.replacing[0].stop_first
    assert waiting(sent) == [("default", "unavailable")]

    # Its successor is forked once it has exited, and the second swap
    # still waits until that successor is in routing.

    exits(monkeypatch, (4000, 0))
    instance.reap()
    instance.run_swaps()
    assert forks == [(0, 5000)]
    assert instance.routed[1] == "w1.sock"
    recycled = [message for message in sent if message.get("kind") == "worker.recycled"]
    assert recycled[0]["message"] == (
        "worker 0 (pid 4000) replaced by pid 5000: lived 60 s; "
        "it stopped before its replacement started"
    )

    ready(instance, 5000)
    instance.run_swaps()
    assert instance.routed[1] is None
    assert forks == [(0, 5000)]


def test_a_group_of_one_with_no_surge_has_no_floor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, states, _sent, forks = swapping_host(
        tmp_path, monkeypatch, workers=1, max_surge=0, max_unavailable=1
    )
    instance.swap(states[0], "lived 60 s", "worker.recycled", "max_lifetime")
    assert instance.routed[0] is None
    assert forks == []


def test_an_involuntary_replacement_skips_the_queue_and_keeps_the_order(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # With a surge allowance: a hard timeout is not held back by the swap
    # under way, and its successor is forked at once.

    instance, states, _sent, forks = swapping_host(tmp_path, monkeypatch)
    instance.swap(states[0], "served 10 requests", "worker.recycled", "max_requests")
    assert instance.replace(states[1], "ran too long", "worker.timeout_killed", "hard_timeout")
    assert forks == [(0, 5000), (1, 5001)]

    # With none: it is not held back by the worker already out of
    # routing either, but its successor waits for it to exit, and the
    # fault is reported at the decision all the same.

    instance, states, sent, forks = swapping_host(
        tmp_path / "none", monkeypatch, max_surge=0, max_unavailable=1
    )
    instance.swap(states[0], "served 10 requests", "worker.recycled", "max_requests")
    assert instance.replace(states[1], "ran too long", "worker.timeout_killed", "hard_timeout")
    assert forks == []
    assert instance.routed == {0: None, 1: None}
    replacement = instance.replacing[1]
    assert replacement.stop_first and replacement.terminate_at is not None

    replacement.captured = True
    instance.report_timeout_kill(replacement)
    killed = [message for message in sent if message.get("kind") == "worker.timeout_killed"]
    assert (
        killed[0]["message"] == "worker 1 (pid 4001) is replaced once it has stopped: ran too long"
    )

    exits(monkeypatch, (4001, 0))
    instance.reap()
    assert forks == [(1, 5000)]


def test_a_queued_swap_is_dropped_when_its_worker_goes_another_way(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, states, _sent, forks = swapping_host(tmp_path, monkeypatch, workers=3)
    instance.swap(states[0], "served 10 requests", "worker.recycled", "max_requests")
    instance.swap(states[1], "served 11 requests", "worker.recycled", "max_requests")
    instance.swap(states[2], "served 12 requests", "worker.recycled", "max_requests")

    # The second worker is scaled in while it waits, and a worker being
    # debugged is passed over and keeps its place.

    instance.retire(states[1], "scaled in")
    states[2].debug_sessions = 1
    ready(instance, 5000)
    exits(monkeypatch, (4000, 0))
    instance.reap()
    instance.run_swaps()
    assert forks == [(0, 5000)]
    assert [swap.pid for swap in instance.swaps["default"]] == [4002]

    states[2].debug_sessions = 0
    instance.run_swaps()
    assert forks == [(0, 5000), (2, 5001)]


def test_a_groups_swap_settings_are_its_own_or_the_releases(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    reports = host.Group("reports", 1, 1, 1, 1, max_surge=0, max_unavailable=1)
    plain = host.Group("plain", 1, 1, 1, 1)
    instance = host.Host(
        release, str(tmp_path), workers=1, threads=1, groups=[reports, plain], max_surge=2
    )
    settings = {group.name: (group.max_surge, group.max_unavailable) for group in instance.groups}
    assert settings == {"default": (2, 0), "reports": (0, 1), "plain": (2, 0)}

    assert host.parse_group_swap("reports=0,1") == (
        "reports",
        {"max_surge": 0, "max_unavailable": 1},
    )

    for refused in ("reports=0,0", "reports=1", "=1,0", "reports=-1,2", "reports=a,b"):
        with pytest.raises(argparse.ArgumentTypeError):
            host.parse_group_swap(refused)


def test_swaps_are_surge_first_only_within_the_allowance_the_daemon_grants(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Section 7.9: the host keeps the allowance it was last granted."""

    instance, states, sent, forks = swapping_host(tmp_path, monkeypatch)
    default = instance.groups[0]
    assert instance.surge_allowance(default) == 1

    # No room in the budget and nothing allowed under the count: the swap
    # waits, and says which limit it waits on.

    instance.take_request(
        {
            "type": "request",
            "id": 7,
            "op": "surge",
            "args": {"groups": {"default": {"allowance": 0, "short": "memory"}}},
        }
    )
    assert sent[-1] == {"type": "response", "id": 7, "workers": []}
    assert instance.surge_allowance(default) == 0
    instance.swap(states[0], "served 10 requests", "worker.recycled", "max_requests")
    assert forks == []
    assert waiting(sent) == [("default", "budget")]
    event = next(message for message in sent if message.get("kind") == "worker.swap_waiting")
    assert event["message"].endswith(
        "the node's memory budget has no room for another worker of the group default over "
        "its count"
    )

    # An involuntary replacement is not held back, and keeps the order:
    # its successor is forked once it has exited.

    assert instance.replace(states[1], "ran too long", "worker.timeout_killed", "hard_timeout")
    assert forks == []
    assert instance.replacing[1].stop_first

    # The swap that waited starts as soon as an allowance is granted, and
    # never more is taken than the group's max_surge.

    surge = {"groups": {"default": {"allowance": 3, "short": ""}}}
    instance.take_request({"type": "request", "id": 8, "op": "surge", "args": surge})
    assert instance.surge_allowance(default) == 1
    assert forks == [(0, 5000)]


def test_a_release_with_no_traffic_swaps_drain_first_whatever_its_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, states, sent, forks = swapping_host(tmp_path, monkeypatch, workers=1)
    surge = {"groups": {"default": {"allowance": 0, "short": "traffic"}}}
    instance.take_request({"type": "request", "id": 3, "op": "surge", "args": surge})
    instance.swap(states[0], "lived 60 s", "worker.recycled", "max_lifetime")
    assert forks == []
    assert instance.routed[0] is None
    assert instance.replacing[0].stop_first
    assert waiting(sent) == []


# Section 10.14: every worker restarted on request, through the queue.


def ask(instance: host.Host, op: str, **args: object) -> None:
    instance.take_request({"type": "request", "id": 9, "op": op, "args": args})


def restart_progress(sent: list[dict[str, Any]]) -> list[tuple[object, object]]:
    return [
        (message["total"], message["left"]) for message in sent if message.get("type") == "restart"
    ]


def test_every_worker_is_restarted_in_turn_through_the_queue(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, _states, sent, forks = swapping_host(tmp_path, monkeypatch)
    ask(instance, "restart_all")

    # Both workers are marked and the answer names them; the first is
    # replaced at once and the second waits its turn.

    response = next(message for message in sent if message.get("type") == "response")
    assert [worker["pid"] for worker in response["workers"]] == [4000, 4001]
    assert kinds(sent)[0] == "restart.started"
    assert forks == [(0, 5000)]
    assert len(instance.swaps["default"]) == 1
    assert restart_progress(sent) == [(2, 2)]

    # A second ask marks only what has started since: the replacement.

    ask(instance, "restart_all")
    assert instance.restart is not None and instance.restart.total == 3

    ready(instance, 5000)
    exits(monkeypatch, (4000, 0))
    instance.reap()
    instance.tick()
    assert forks == [(0, 5000), (1, 5001)]
    assert restart_progress(sent)[-1] == (3, 2)

    ready(instance, 5001)
    exits(monkeypatch, (4001, 0))
    instance.reap()
    instance.tick()
    assert forks == [(0, 5000), (1, 5001), (0, 5002)]

    ready(instance, 5002)
    exits(monkeypatch, (5000, 0))
    instance.reap()
    instance.tick()
    assert instance.restart is None
    assert restart_progress(sent)[-1] == (3, 0)
    finished = [message for message in sent if message.get("kind") == "restart.finished"]
    assert finished[0]["message"] == "the 3 workers marked have each been replaced or have gone"
    restarted = [message for message in sent if message.get("kind") == "worker.restarted"]
    assert len(restarted) == 3 and restarted[0]["reason"] == "restart"
    assert restarted[0]["message"] == (
        "worker 0 (pid 4000) replaced by pid 5000: restarted on request"
    )


def test_a_cancelled_restart_leaves_the_workers_not_yet_replaced(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, states, sent, forks = swapping_host(tmp_path, monkeypatch, workers=3)
    ask(instance, "restart_cancel")
    assert sent[-1]["error"] == "no restart is under way"

    ask(instance, "restart_all")
    assert forks == [(0, 5000)]
    ask(instance, "restart_cancel")

    # The replacement under way is finished; the two that waited stay,
    # and are no longer queued.

    assert [worker["pid"] for worker in sent[-1]["workers"]] == [4001, 4002]
    assert not instance.swaps["default"]
    assert not states[1].queued and not states[2].queued
    assert instance.restart is not None

    ready(instance, 5000)
    exits(monkeypatch, (4000, 0))
    instance.reap()
    instance.tick()
    assert instance.restart is None
    assert forks == [(0, 5000)]
    finished = next(message for message in sent if message.get("kind") == "restart.finished")
    assert finished["reason"] == "cancelled"
    assert finished["message"] == (
        "cancelled: 1 of the 3 marked workers were replaced or have gone, and 2 stay as they were"
    )


def test_a_restart_of_one_group_marks_only_its_workers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    reports = host.Group("reports", 1, 1, 1, 1)
    instance = host.Host(release, str(tmp_path), workers=1, threads=1, groups=[reports])
    sent: list[dict[str, Any]] = []
    instance.control_listener = object()  # type: ignore[assignment]
    instance.send_to_supervisor = sent.append  # type: ignore[method-assign]
    instance.spawn = lambda index, replacing=False: 5000 + index  # type: ignore[method-assign]

    for index in range(2):
        state = host.WorkerState(index, 4000 + index, 0, f"w{index}.sock", time.monotonic(), -1)
        state.ready = True
        instance.states[state.pid] = state
        instance.workers[state.pid] = index
        instance.routed[index] = state.path

    ask(instance, "restart_all", group="nosuch")
    assert sent[-1]["error"] == "no worker group nosuch"

    ask(instance, "restart_all", group="reports")
    assert sent[-1]["workers"] == [{"index": 0, "group": "reports", "pid": 4001, "ok": True}]
    assert list(instance.replacing) == [1]
    started = next(message for message in sent if message.get("kind") == "restart.started")
    assert started["group"] == "reports"
    assert started["message"] == (
        "1 worker of the group reports started before now is to be replaced, each in its turn"
    )

    # A supervisor that connects meanwhile is told how far it has got.

    assert instance.restart is not None
    instance.restart.told = None
    instance.tell_restart()
    assert sent[-1]["type"] == "restart" and (sent[-1]["total"], sent[-1]["left"]) == (1, 1)


# Section 11.5: events raised with no supervisor connected are held.


def unsupervised_host(tmp_path: Path) -> host.Host:
    """A host with a control socket and no supervisor connected to it."""

    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)
    instance.control_listener = object()  # type: ignore[assignment]
    return instance


def connect(instance: host.Host) -> socket.socket:
    """Stand a supervisor's connection in, and return the supervisor's end."""

    ours, theirs = socket.socketpair()
    instance.supervisor = ours
    return theirs


def received(connection: socket.socket) -> list[dict[str, Any]]:
    """Read every line the host has sent on a connection."""

    connection.setblocking(False)
    data = b""

    while True:
        try:
            chunk = connection.recv(65536)
        except BlockingIOError:
            break

        if not chunk:
            break

        data += chunk

    return [json.loads(line) for line in data.splitlines()]


def test_events_raised_with_no_supervisor_are_held_and_sent_with_their_times(
    tmp_path: Path,
) -> None:
    instance = unsupervised_host(tmp_path)
    before = int(time.time() * 1000)

    instance.emit("worker.recycled", "info", "one", group="default", reason="max_requests")
    instance.emit(
        "worker.timeout_killed",
        "warning",
        "two",
        group="default",
        reason="hard_timeout",
        evidence={"worker": "0", "pid": 1, "requests": ["r-1"], "threads": []},
    )
    instance.emit("restart.finished", "notice", "three")

    # State is not held: a supervisor is sent it whole when it connects.

    instance.announce()
    after = int(time.time() * 1000)
    assert [event["kind"] for event in instance.held_events] == [
        "worker.recycled",
        "worker.timeout_killed",
        "restart.finished",
    ]

    # A supervisor that connects is sent them oldest first, each with the
    # time it happened and none with its evidence.

    supervisor = connect(instance)
    instance.send_held_events()
    events = received(supervisor)
    assert [(event["type"], event["kind"], event["message"]) for event in events] == [
        ("event", "worker.recycled", "one"),
        ("event", "worker.timeout_killed", "two"),
        ("event", "restart.finished", "three"),
    ]
    assert (events[0]["group"], events[0]["reason"]) == ("default", "max_requests")
    assert all(before <= event["time_unix_ms"] <= after for event in events)
    assert all("evidence" not in event for event in events)
    assert not instance.held_events

    # With one connected, an event is sent as it happens, with no time of
    # its own, and nothing is held.

    instance.emit("worker.recycled", "info", "four")
    assert received(supervisor) == [
        {"type": "event", "kind": "worker.recycled", "level": "info", "message": "four"}
    ]
    assert not instance.held_events

    # One whose send fails is held for the next supervisor.

    supervisor.close()
    instance.emit("worker.exited", "error", "five")
    assert instance.supervisor is None
    assert [event["message"] for event in instance.held_events] == ["five"]

    later = connect(instance)
    instance.send_held_events()
    assert [event["message"] for event in received(later)] == ["five"]


def test_past_the_bound_the_oldest_held_events_go_and_the_supervisor_is_told(
    tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    instance = unsupervised_host(tmp_path)

    for number in range(host.HELD_EVENTS + 3):
        instance.emit("worker.recycled", "info", f"event {number}")

    assert len(instance.held_events) == host.HELD_EVENTS
    assert instance.events_dropped == 3

    # The log says so once, when the first is dropped.

    said = "events are held with nothing connected to record them: the oldest are dropped"
    assert capfd.readouterr().err.count(said) == 1

    # The supervisor hears how many were dropped ahead of the ones kept.

    # They are more than a socket's buffer takes, so the supervisor reads
    # as the host sends, as a real one does.

    supervisor = connect(instance)
    events: list[dict[str, Any]] = []

    def read() -> None:
        lines = supervisor.makefile("rb")

        while len(events) < host.HELD_EVENTS + 1:
            events.append(json.loads(lines.readline()))

    reader = threading.Thread(target=read)
    reader.start()
    instance.send_held_events()
    reader.join(timeout=10)
    assert len(events) == host.HELD_EVENTS + 1
    assert (events[0]["kind"], events[0]["level"], events[0]["dropped"]) == (
        "host.events_dropped",
        "warning",
        3,
    )
    assert events[0]["message"] == (
        "3 events raised while nothing was connected to record them were dropped, the "
        f"oldest: {host.HELD_EVENTS} are held at most, and the release's log has a line for each"
    )
    assert events[1]["message"] == "event 3"
    assert events[-1]["message"] == f"event {host.HELD_EVENTS + 2}"
    assert instance.events_dropped == 0

    # The next supervisor is not told again.

    instance.emit("worker.recycled", "info", "sent")
    instance.send_held_events()
    assert [event["kind"] for event in received(supervisor)] == ["worker.recycled"]


def test_a_host_with_no_control_socket_holds_no_events(tmp_path: Path) -> None:
    release = host.Release("plain_wsgi:application", None, host.Context(app="plain_wsgi"))
    instance = host.Host(release, str(tmp_path), workers=1, threads=1)

    instance.emit("worker.recycled", "info", "nobody is coming")
    assert not instance.held_events


# Issue 981: with no supervisor connected, a worker that enters routing
# is reachable at the path the edge has.


def listener(path: str) -> socket.socket:
    """A Unix socket listening at a path, as a worker's is."""

    listening = socket.socket(socket.AF_UNIX)
    listening.bind(path)
    listening.listen(4)
    listening.setblocking(False)
    return listening


def reached(path: str, listening: socket.socket) -> bool:
    """Say whether a connection to the path arrives at this listener."""

    client = socket.socket(socket.AF_UNIX)

    try:
        client.connect(path)
    except OSError:
        return False
    finally:
        client.close()

    try:
        accepted, _ = listening.accept()
    except BlockingIOError:
        return False

    accepted.close()
    return True


def acknowledge(instance: host.Host, supervisor: socket.socket) -> None:
    """Have the supervisor acknowledge the host's latest announcement."""

    assert instance.supervisor is not None
    instance.supervisor.setblocking(False)
    supervisor.sendall(
        json.dumps({"type": "applied", "version": instance.version}).encode() + b"\n"
    )
    instance.read_supervisor()


def test_a_worker_that_enters_routing_with_no_supervisor_takes_the_path_the_edge_has() -> None:
    # A directory of its own with a short name: a Unix socket's path is
    # limited to about a hundred characters.

    with tempfile.TemporaryDirectory(prefix="muru-", dir="/tmp") as directory:
        instance = unsupervised_host(Path(directory))
        first = os.path.join(directory, "worker-0.sock")
        second = os.path.join(directory, "worker-0-g1.sock")
        old = listener(first)
        instance.routed[0] = first

        # The supervisor acknowledges the first socket, so the edge has it.

        supervisor = connect(instance)
        instance.send_workers()
        acknowledge(instance, supervisor)
        assert instance.edge_routes == {0: first}
        assert not instance.edge_reaches(0)

        # It goes away, and a replacement becomes ready on a socket of
        # its own, which nobody can tell the edge of: the edge's path
        # reaches the new worker, and the old one's socket stays open.

        supervisor.close()
        instance.drop_supervisor()
        new = listener(second)
        instance.routed[0] = second
        instance.alias_routes()
        assert reached(first, new) and not reached(first, old)
        assert reached(second, new)
        assert instance.edge_reaches(0)
        assert sorted(os.listdir(directory)) == ["worker-0-g1.sock", "worker-0.sock"]

        # A worker that crashed is started again on the path it had,
        # which is a new socket: the edge's path follows it.

        new.close()
        os.unlink(second)
        again = listener(second)
        assert not reached(first, again)
        instance.alias_routes()
        assert reached(first, again)

        # The next supervisor is told the socket's own path, and the
        # second name goes once it has acknowledged it.

        later = connect(instance)
        instance.version += 1
        instance.send_workers()
        assert received(later)[-1]["sockets"] == [second]
        assert os.path.exists(first)
        acknowledge(instance, later)
        assert instance.edge_routes == {0: second}
        assert not instance.aliases
        assert os.listdir(directory) == ["worker-0-g1.sock"]

        for held in (old, again, later):
            held.close()

        instance.drop_supervisor()


def test_an_announcement_never_acknowledged_is_not_what_the_edge_has(tmp_path: Path) -> None:
    instance = unsupervised_host(tmp_path)
    instance.routed[0] = "worker-0.sock"
    supervisor = connect(instance)
    instance.send_workers()
    acknowledge(instance, supervisor)

    # The supervisor goes before it acknowledges the next announcement,
    # so the edge is taken to have the sockets of the last it did.

    instance.routed[0] = "worker-0-g1.sock"
    instance.announce()
    assert instance.announcements == {1: {0: "worker-0-g1.sock"}}
    supervisor.close()
    instance.drop_supervisor()
    assert instance.edge_routes == {0: "worker-0.sock"}
    assert not instance.announcements


# Issue 987: a worker's socket file goes when the worker has.


def test_a_replaced_workers_socket_file_is_removed_once_it_has_exited(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    instance, states, _sent, forks = swapping_host(tmp_path, monkeypatch)
    monkeypatch.chdir(tmp_path)

    for name in ("w0.sock", "w1.sock", "w0-g1.sock"):
        Path(name).touch()

    # The old worker's file stays while it finishes what it holds, and
    # goes when it is reaped; the others are untouched.

    instance.swap(states[0], "served 10 requests", "worker.recycled", "max_requests")
    ready(instance, forks[0][1])
    assert sorted(os.listdir()) == ["w0-g1.sock", "w0.sock", "w1.sock"]
    exits(monkeypatch, (states[0].pid, 0))
    instance.reap()
    assert sorted(os.listdir()) == ["w0-g1.sock", "w1.sock"]

    # A path that is the second name of another worker's socket, which
    # the edge connects to with no supervisor, is left.

    instance.aliases[1] = ("w1.sock", "w1-g1.sock")
    exits(monkeypatch, (states[1].pid, 0))
    instance.reap()
    assert sorted(os.listdir()) == ["w0-g1.sock", "w1.sock"]


# Issue 983: a supervisor that connects is told where a restart stands,
# and one that does not answer is treated as one that is not there.


def test_a_supervisor_that_connects_is_told_no_restart_is_under_way() -> None:
    with tempfile.TemporaryDirectory(prefix="muru-", dir="/tmp") as directory:
        instance = unsupervised_host(Path(directory))
        instance.control_listener = listener(os.path.join(directory, "control.sock"))
        supervisor = socket.socket(socket.AF_UNIX)
        supervisor.connect(os.path.join(directory, "control.sock"))
        instance.accept_supervisor()
        assert [message["type"] for message in received(supervisor)] == ["workers", "restart"]

        # A second connection takes the first one's place, and is told
        # the same: the first may have been left believing in a restart
        # that has finished since.

        again = socket.socket(socket.AF_UNIX)
        again.connect(os.path.join(directory, "control.sock"))
        instance.accept_supervisor()
        assert received(again)[1] == {
            "type": "restart",
            "total": 0,
            "left": 0,
            "started_unix_ms": 0,
            "group": "",
        }
        assert supervisor.recv(1) == b""

        for held in (supervisor, again, instance.control_listener):
            held.close()

        instance.drop_supervisor()


def test_a_replacement_nobody_acknowledges_takes_the_path_the_edge_has(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    with tempfile.TemporaryDirectory(prefix="muru-", dir="/tmp") as directory:
        instance = unsupervised_host(Path(directory))
        first = os.path.join(directory, "worker-0.sock")
        second = os.path.join(directory, "worker-0-g1.sock")
        old = listener(first)
        instance.routed[0] = first
        supervisor = connect(instance)
        instance.send_workers()
        acknowledge(instance, supervisor)

        # The supervisor stays connected and says nothing of the next
        # announcement: when its time is up the old worker is told to
        # stop, and the edge's path reaches the new one first.

        monkeypatch.setattr(os, "kill", lambda _pid, _signal: None)
        new = listener(second)
        instance.routed[0] = second
        replacement = host.Replacement(0, 4000, "served 10 requests", "worker.recycled")
        replacement.announced = instance.announce()
        replacement.announced_at = time.monotonic()
        instance.advance(replacement, time.monotonic())
        assert replacement.terminate_at is None and not reached(first, new)

        instance.advance(replacement, time.monotonic() + host.ACK_TIMEOUT_SECONDS)
        assert replacement.terminate_at is not None
        assert reached(first, new) and reached(second, new)

        # An acknowledgement that comes after all takes the second name
        # away: the edge has the socket's own.

        acknowledge(instance, supervisor)
        assert os.listdir(directory) == ["worker-0-g1.sock"]

        for held in (old, new, supervisor):
            held.close()

        instance.drop_supervisor()
