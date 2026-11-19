"""Unit tests for muru._imports: the modules a worker loads for the first time."""

from __future__ import annotations

import importlib
import sys
import threading
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

from muru import _imports
from muru._context import _request_id


@pytest.fixture
def source(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    # A directory on sys.path for the test's own modules, with the finder
    # in, and everything taken out again after.

    monkeypatch.syspath_prepend(str(tmp_path))
    before = set(sys.modules)
    _imports.install()

    try:
        yield tmp_path
    finally:
        _imports.uninstall()

        for name in set(sys.modules) - before:
            del sys.modules[name]


def test_a_requests_loads_are_kept_nested_with_what_triggered_them(source: Path) -> None:
    (source / "shop").mkdir()
    (source / "shop" / "__init__.py").write_text("")
    (source / "shop" / "reports.py").write_text("import shop.cells\n")
    (source / "shop" / "cells.py").write_text("")
    (source / "plain.py").write_text("")

    tallied: list[float] = []
    _imports._start("R1", tallied.append)
    token = _request_id.set("R1")

    try:
        import shop.reports  # noqa: F401

        importlib.import_module("plain")
        __import__("plain")
    finally:
        _request_id.reset(token)

    loads, count, outermost_ms, _waits = _imports._take("R1")
    names = [(name, depth) for _, _, name, _, depth, _ in loads]

    # The package, the module and the one it imports, nested; the second
    # import of plain is no load. Each trigger is the line that asked,
    # relative to its entry of sys.path.

    assert names == [("shop", 0), ("shop.reports", 0), ("shop.cells", 1), ("plain", 0)]
    assert count == 4
    assert loads[2][3] == "shop/reports.py:1"
    assert all(load[3].split(":")[0].endswith("test_imports.py") for load in (loads[1], loads[3]))

    # Only the outermost loads are summed and told to the runtime, and
    # each module has its own loader, not the timing one.

    assert len(tallied) == 3
    assert outermost_ms == pytest.approx(sum(tallied))
    assert type(sys.modules["shop.cells"].__loader__).__name__ == "SourceFileLoader"
    assert sys.modules["plain"].__spec__.loader is sys.modules["plain"].__loader__

    # A load outside any request is only counted, for the worker.

    (source / "later.py").write_text("")
    importlib.invalidate_caches()
    import later  # noqa: F401

    assert _imports.loaded() == (5, 1)
    assert _imports._take("R1") == ([], 0, 0.0, [])


def test_a_record_keeps_up_to_its_limit_and_a_missing_module_is_searched_once(
    source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_imports, "LOADS_KEPT", 1)

    for name in ("one", "two"):
        (source / f"{name}.py").write_text("")

    _imports._start("R1")
    token = _request_id.set("R1")

    try:
        import one  # noqa: F401
        import two  # noqa: F401
    finally:
        _request_id.reset(token)

    loads, count, _, _ = _imports._take("R1")
    assert ([load[2] for load in loads], count) == (["one"], 2)

    # A module that is not there is left to the import system from then
    # on, which still finds it once it appears.

    with pytest.raises(ImportError):
        import absent

    assert "absent" in _imports._missing
    (source / "absent.py").write_text("")
    importlib.invalidate_caches()
    import absent  # noqa: F401


def test_a_record_past_its_limit_keeps_the_first_loads_to_start(
    source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Issue 782: loads end innermost first, so keeping the first to end
    # would drop the outer import; the first to start keep it, with the
    # loads nested in it beneath. The worker's list is kept the same way.

    monkeypatch.setattr(_imports, "LOADS_KEPT", 3)
    monkeypatch.setattr(_imports, "WORKER_LOADS_KEPT", 3)
    (source / "report.py").write_text("".join(f"import part{index}\n" for index in range(5)))

    for index in range(5):
        (source / f"part{index}.py").write_text("")

    _imports._start("R1")
    token = _request_id.set("R1")

    try:
        import report  # noqa: F401
    finally:
        _request_id.reset(token)

    loads, count, outermost_ms, _waits = _imports._take("R1")
    kept = [(name, depth) for _, _, name, _, depth, _ in loads]
    assert kept == [("report", 0), ("part0", 1), ("part1", 1)]
    assert count == 6
    assert loads[0][1] == pytest.approx(outermost_ms)
    assert loads[0][1] >= sum(load[1] for load in loads[1:])

    worker = _imports.worker_loads()
    assert [load["module"] for load in worker["loads"]] == ["report", "part0", "part1"]
    assert worker["loads"][0]["depth"] == 0 and worker["loaded"] == 6

    # Issue 763: each load names the one it was nested in.

    assert [load["parent"] for load in worker["loads"]] == ["", "report", "report"]


def test_the_runtime_times_the_outermost_loads_alone(
    source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (source / "outer.py").write_text("import inner\n")
    (source / "inner.py").write_text("")
    noted: list[float] = []
    monkeypatch.setattr(_imports, "_note_loaded", noted.append)

    import outer  # noqa: F401

    # Every load is counted, and only the outermost is timed for the
    # worker's metrics, since it holds the one nested in it.

    assert len(noted) == 2
    assert noted[0] == 0.0 and noted[1] > 0.0


def test_a_request_waiting_on_another_threads_load_is_kept_with_who_loads_it(
    source: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Issue 787: the waiting thread never reaches the finder; it waits on
    # the module's lock, whose class the runtime replaces (issue 800).
    # That is private API, so this fails if it moves.

    bootstrap: Any = importlib._bootstrap
    assert _imports._module_lock is not None
    assert issubclass(bootstrap._ModuleLock, _imports._module_lock)
    monkeypatch.setattr(_imports, "WAIT_MIN_MS", 0.0)

    (source / "gate.py").write_text(
        "import threading\nstarted = threading.Event()\nrelease = threading.Event()\n"
    )
    (source / "slow.py").write_text("import gate\ngate.started.set()\ngate.release.wait(10)\n")
    (source / "circle_a.py").write_text("import circle_b\n")
    (source / "circle_b.py").write_text("import circle_a\n")
    import gate

    # The module's lock, as the waiting thread is about to block on it,
    # lets the load go.

    waiting = threading.Event()
    get_module_lock = bootstrap._get_module_lock

    def lock_for(module: str) -> Any:
        if module == "slow" and _request_id.get() == "R2":
            waiting.set()

        return get_module_lock(module)

    monkeypatch.setattr(bootstrap, "_get_module_lock", lock_for)
    waits: list[float] = []
    _imports._start("R1")
    _imports._start("R2", wait_tally=waits.append)

    def request(name: str) -> None:
        token = _request_id.set(name)

        try:
            import slow  # noqa: F401
        finally:
            _request_id.reset(token)

    loading = threading.Thread(target=request, args=("R1",))
    loading.start()
    assert gate.started.wait(10)
    waiter = threading.Thread(target=request, args=("R2",))
    waiter.start()
    assert waiting.wait(10)
    gate.release.set()
    loading.join()
    waiter.join()

    # A circular import on the request's own thread is no wait.

    token = _request_id.set("R2")

    try:
        import circle_a  # noqa: F401
    finally:
        _request_id.reset(token)

    loaded = _imports._take("R1")
    assert [load[2] for load in loaded[0]] == ["slow"] and loaded[3] == []

    loads, _count, _outermost_ms, kept = _imports._take("R2")
    assert [load[2] for load in loads] == ["circle_a", "circle_b"]
    assert [(module, loader) for _, _, module, loader in kept] == [("slow", "R1")]
    assert waits == [kept[0][1]]


@pytest.mark.parametrize("name", ["slow_m", "pkg.slow_m"])
def test_a_wait_through_import_module_is_kept_once(
    source: Path, monkeypatch: pytest.MonkeyPatch, name: str
) -> None:
    # Issue 794: importlib.import_module blocks on the module's lock in
    # _find_and_load, through a lock manager on Python 3.11 to 3.15.1 (a
    # hierarchical one for a dotted name on 3.15) and directly from 3.15.2
    # (issue 800), and on 3.15.1 and before then takes the lock again
    # through _lock_unlock_module, which must not count the wait twice.

    bootstrap: Any = importlib._bootstrap
    monkeypatch.setattr(_imports, "WAIT_MIN_MS", 0.0)

    (source / "gate.py").write_text(
        "import threading\nstarted = threading.Event()\nrelease = threading.Event()\n"
    )
    (source / "pkg").mkdir()
    (source / "pkg" / "__init__.py").write_text("")
    slow = "import gate\ngate.started.set()\ngate.release.wait(10)\n"
    (source / "slow_m.py").write_text(slow)
    (source / "pkg" / "slow_m.py").write_text(slow)
    import gate

    # The module's lock, as the waiting thread is about to block on it,
    # lets the load go.

    waiting = threading.Event()
    get_module_lock = bootstrap._get_module_lock

    def lock_for(module: str) -> Any:
        if module == name and _request_id.get() == "R2":
            waiting.set()

        return get_module_lock(module)

    monkeypatch.setattr(bootstrap, "_get_module_lock", lock_for)
    waits: list[float] = []
    _imports._start("R1")
    _imports._start("R2", wait_tally=waits.append)

    def request(request: str) -> None:
        token = _request_id.set(request)

        try:
            importlib.import_module(name)
        finally:
            _request_id.reset(token)

    loading = threading.Thread(target=request, args=("R1",))
    loading.start()
    assert gate.started.wait(10)
    waiter = threading.Thread(target=request, args=("R2",))
    waiter.start()
    assert waiting.wait(10)
    gate.release.set()
    loading.join()
    waiter.join()

    loaded = _imports._take("R1")
    assert name in [load[2] for load in loaded[0]] and loaded[3] == []

    loads, _count, _outermost_ms, kept = _imports._take("R2")
    assert loads == []
    assert [(module, loader) for _, _, module, loader in kept] == [(name, "R1")]
    assert waits == [kept[0][1]]
