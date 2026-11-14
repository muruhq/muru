"""Unit tests for instrumentation modules (section 6.15)."""

from __future__ import annotations

import gc
import json
import os
import sys
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest

from muru import check, host
from muru._instrumentation import ENVIRONMENT, Entry, InstrumentationError, Instruments, Target

FIXTURE_APPS = Path(__file__).resolve().parents[1] / "fixtures" / "apps"

# Each module appends to one shared list as it is imported and as its
# callbacks run, so the test sees the order across modules.

RECORDING = """
import instrumented_order as order

order.CALLS.append(("import", __name__))


def configure(settings):
    order.CALLS.append(("configure", __name__, dict(settings)))
    settings["changed"] = True
"""


@pytest.fixture
def modules(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    monkeypatch.syspath_prepend(str(tmp_path))
    (tmp_path / "instrumented_order.py").write_text("CALLS = []\n")
    names = ("instrumented_order", "first_tool", "second_tool", "broken_tool", "odd_tool")

    for name in names:
        sys.modules.pop(name, None)

    yield tmp_path

    for name in names:
        sys.modules.pop(name, None)


def calls() -> list[tuple[object, ...]]:
    return list(sys.modules["instrumented_order"].CALLS)


def test_each_module_is_configured_before_the_next_is_imported(modules: Path) -> None:
    (modules / "first_tool.py").write_text(RECORDING)
    (modules / "second_tool.py").write_text(RECORDING)
    first = Entry("first_tool", {"output": "/tmp/times.json"})
    instruments = Instruments([first, Entry("second_tool")])
    instruments.load()

    # Each sees only its own settings, as a copy it may change.

    assert calls() == [
        ("import", "first_tool"),
        ("configure", "first_tool", {"output": "/tmp/times.json"}),
        ("import", "second_tool"),
        ("configure", "second_tool", {}),
    ]
    assert first.settings == {"output": "/tmp/times.json"}
    assert instruments.defined() == {"first_tool": ["configure"], "second_tool": ["configure"]}


def test_a_module_that_cannot_be_imported_stops_the_load(modules: Path) -> None:
    (modules / "first_tool.py").write_text(RECORDING)
    instruments = Instruments([Entry("first_tool"), Entry("missing_tool"), Entry("second_tool")])

    with pytest.raises(InstrumentationError, match="cannot import instrumentation module missing"):
        instruments.load()

    assert [call[1] for call in calls()] == ["first_tool", "first_tool"]


def test_a_failing_callback_is_logged_and_the_rest_carry_on(
    modules: Path, caplog: pytest.LogCaptureFixture
) -> None:
    (modules / "broken_tool.py").write_text(
        "def configure(settings):\n    raise RuntimeError('no licence key')\n"
    )
    (modules / "odd_tool.py").write_text("configure = 3\n")
    (modules / "second_tool.py").write_text(RECORDING)
    instruments = Instruments([Entry("broken_tool"), Entry("odd_tool"), Entry("second_tool")])
    instruments.load()

    assert calls()[-1] == ("configure", "second_tool", {})
    assert instruments.defined() == {
        "broken_tool": ["configure"],
        "odd_tool": [],
        "second_tool": ["configure"],
    }
    error = caplog.text
    assert "the configure callback of instrumentation module broken_tool failed" in error
    assert "RuntimeError: no licence key" in error
    assert "odd_tool: configure is not callable" in error


def test_the_list_leaves_the_environment_it_came_in() -> None:
    entries = [Entry("first_tool", {"output": "x.json"}), Entry("second_tool")]
    environ = {ENVIRONMENT: Entry.encode(entries), "PATH": "/bin"}

    assert Entry.take(environ) == entries
    assert environ == {"PATH": "/bin"}
    assert Entry.take(environ) == []

    with pytest.raises(ValueError, match="not an instrumentation entry"):
        Entry.parse(json.dumps([{"settings": {}}]))


def test_check_imports_the_modules_before_the_application(
    modules: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    (modules / "first_tool.py").write_text(RECORDING)
    monkeypatch.setenv(ENVIRONMENT, Entry.encode([Entry("first_tool")]))
    monkeypatch.chdir(FIXTURE_APPS)

    status = check.main(["--app", "plain_wsgi:application", "--chdir", str(FIXTURE_APPS)])
    report = json.loads(capsys.readouterr().out)

    assert status == 0, report
    assert report["instrumentation"] == [{"module": "first_tool", "callbacks": ["configure"]}]
    assert ENVIRONMENT not in os.environ

    # One that cannot be imported fails the check before the application
    # is loaded.

    monkeypatch.setenv(ENVIRONMENT, Entry.encode([Entry("missing_tool")]))
    status = check.main(["--app", "plain_wsgi:application", "--chdir", str(FIXTURE_APPS)])
    report = json.loads(capsys.readouterr().out)

    assert status == 1
    assert report["entrypoints"] == []
    assert "cannot import instrumentation module missing_tool" in report["errors"][0]["error"]


WRAPPING = """
import instrumented_order as order


def wrap_application(context, target, application, interface):
    order.CALLS.append(("wrap", __name__, target.kind, target.name, interface))

    if application == "server":
        return "another server"

    return f"{__name__}({application})"
"""


def test_the_first_module_listed_wraps_outermost(modules: Path) -> None:
    (modules / "first_tool.py").write_text(WRAPPING)
    (modules / "second_tool.py").write_text(WRAPPING)
    (modules / "odd_tool.py").write_text("def wrap_application(*args):\n    return None\n")
    instruments = Instruments([Entry("first_tool"), Entry("odd_tool"), Entry("second_tool")])
    instruments.load()
    target = Target("mount", "api", "shop.api:app")

    assert instruments.wrap(None, target, "app", "asgi") == "first_tool(second_tool(app))"
    assert calls() == [
        ("wrap", "second_tool", "mount", "api", "asgi"),
        ("wrap", "first_tool", "mount", "api", "asgi"),
    ]


def test_a_failing_wrapper_or_a_grpc_server_leaves_the_application_as_it_was(
    modules: Path, caplog: pytest.LogCaptureFixture
) -> None:
    (modules / "broken_tool.py").write_text(
        "def wrap_application(context, target, application, interface):\n"
        "    raise RuntimeError('cannot wrap')\n"
    )
    (modules / "second_tool.py").write_text(WRAPPING)
    instruments = Instruments([Entry("second_tool"), Entry("broken_tool")])
    instruments.load()
    entrypoint = Target("entrypoint", "", "shop.wsgi:application")

    assert instruments.wrap(None, entrypoint, "app", "wsgi") == "second_tool(app)"
    assert "so shop.wsgi:application is served as it was" in caplog.text

    # A gRPC server is still handed over, but what comes back is not served.

    assert instruments.wrap(None, entrypoint, "server", "grpc") == "server"
    assert "returned another object for the gRPC entry point" in caplog.text


LIFECYCLE = """
import instrumented_order as order


def load_started(context):
    order.CALLS.append(("load_started", __name__))


def before_import(context, target):
    order.CALLS.append(("before", __name__, target.kind, target.spec))


def after_import(context, target):
    error = type(target.error).__name__ if target.error is not None else None
    order.CALLS.append(("after", __name__, target.kind, error))


def load_finished(context, outcome):
    order.CALLS.append(("finished", __name__, outcome))
"""


@pytest.fixture
def host_main(
    modules: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> Iterator[Callable[..., int]]:
    """Run the host's main up to the fork, with two recording modules listed."""

    (modules / "first_tool.py").write_text(LIFECYCLE)
    (modules / "second_tool.py").write_text(LIFECYCLE)
    (modules / "plain_app.py").write_text("def application(environ, start_response): ...\n")
    (modules / "leaky_app.py").write_text(
        "import socket\nLEFT_OPEN = socket.socketpair()\n"
        "def application(environ, start_response): ...\n"
    )
    (modules / "failing_hooks.py").write_text(
        "def on_load(context):\n    raise RuntimeError('load-boom')\n"
    )

    def run(app: str, *args: str) -> int:
        monkeypatch.setenv(ENVIRONMENT, Entry.encode([Entry("first_tool"), Entry("second_tool")]))
        socket_dir = str(tmp_path / "sockets")
        return host.main(["--app", app, "--socket-dir", socket_dir, *args])

    yield run

    gc.enable()
    leaky = sys.modules.pop("leaky_app", None)

    if leaky is not None:
        for end in leaky.LEFT_OPEN:
            end.close()

    for name in ("plain_app", "failing_hooks", "missing_app"):
        sys.modules.pop(name, None)


def bracket(kind: str, spec: str, error: str | None = None) -> list[tuple[object, ...]]:
    return [
        ("before", "first_tool", kind, spec),
        ("before", "second_tool", kind, spec),
        ("after", "second_tool", kind, error),
        ("after", "first_tool", kind, error),
    ]


def finished(outcome: str) -> list[tuple[object, ...]]:
    return [("finished", "second_tool", outcome), ("finished", "first_tool", outcome)]


STARTED = [("load_started", "first_tool"), ("load_started", "second_tool")]


def test_a_failed_import_or_on_load_ends_the_load_as_failed(
    host_main: Callable[..., int],
) -> None:
    assert host_main("missing_app:application") == 1
    assert calls() == [
        *STARTED,
        *bracket("entrypoint", "missing_app:application", "ModuleNotFoundError"),
        *finished("failed"),
    ]

    sys.modules["instrumented_order"].CALLS.clear()
    sys.modules.pop("first_tool")
    sys.modules.pop("second_tool")
    assert host_main("plain_app:application", "--hooks", "failing_hooks") == 1
    assert calls() == [
        *STARTED,
        *bracket("hooks", "failing_hooks"),
        *bracket("entrypoint", "plain_app:application"),
        *finished("failed"),
    ]


def test_what_the_import_check_finds_is_refused_or_falls_back(
    host_main: Callable[..., int], monkeypatch: pytest.MonkeyPatch
) -> None:
    assert host_main("leaky_app:application", "--preload", "true") == 1
    assert calls() == [
        *STARTED,
        *bracket("entrypoint", "leaky_app:application"),
        *finished("refused"),
    ]

    # The fallback hears of it before the host starts again, and the list
    # goes back in the environment for the host started again.

    class Restarted(Exception):
        pass

    def execv(path: str, argv: list[str]) -> None:
        raise Restarted(os.environ.get(ENVIRONMENT))

    monkeypatch.setattr(os, "execv", execv)

    for name in ("first_tool", "second_tool", "leaky_app"):
        leaky = sys.modules.pop(name)

    for end in leaky.LEFT_OPEN:
        end.close()

    sys.modules["instrumented_order"].CALLS.clear()

    with pytest.raises(Restarted) as restarted:
        host_main("leaky_app:application")

    assert calls()[-2:] == finished("fallback")
    assert [entry.module for entry in Entry.parse(str(restarted.value))] == [
        "first_tool",
        "second_tool",
    ]


def test_check_runs_the_load_callbacks_as_the_host_does(
    modules: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    (modules / "first_tool.py").write_text(LIFECYCLE)
    monkeypatch.setenv(ENVIRONMENT, Entry.encode([Entry("first_tool")]))
    monkeypatch.chdir(FIXTURE_APPS)

    status = check.main(["--app", "plain_wsgi:application", "--chdir", str(FIXTURE_APPS)])
    capsys.readouterr()

    assert status == 0
    assert calls() == [
        ("load_started", "first_tool"),
        ("before", "first_tool", "entrypoint", "plain_wsgi:application"),
        ("after", "first_tool", "entrypoint", None),
        ("finished", "first_tool", "check"),
    ]
