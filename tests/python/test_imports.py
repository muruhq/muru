"""Unit tests for muru._imports: the modules a worker loads for the first time."""

from __future__ import annotations

import importlib
import sys
from collections.abc import Iterator
from pathlib import Path

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

    loads, count, outermost_ms = _imports._take("R1")
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
    import later  # noqa: F401

    assert _imports.loaded() == (5, 1)
    assert _imports._take("R1") == ([], 0, 0.0)


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

    loads, count, _ = _imports._take("R1")
    assert ([load[2] for load in loads], count) == (["one"], 2)

    # A module that is not there is left to the import system from then
    # on, which still finds it once it appears.

    with pytest.raises(ImportError):
        import absent

    assert "absent" in _imports._missing
    (source / "absent.py").write_text("")
    importlib.invalidate_caches()
    import absent  # noqa: F401
