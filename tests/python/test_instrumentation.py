"""Unit tests for instrumentation modules (section 6.15)."""

from __future__ import annotations

import json
import os
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

from muru import check
from muru._instrumentation import ENVIRONMENT, Entry, InstrumentationError, Instruments

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
