"""Unit tests for the import report: muru._lazy_imports recording and muru._import_report.

They record real loads with unwrapt's profile recorder, from the dev
group, and uninstall it again after each test.
"""

from __future__ import annotations

import json
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

from muru import _import_report, _imports, _instrumentation, _lazy_imports, preload
from muru._context import Context, _request_id
from muru._instrumentation import Target

unwrapt = pytest.importorskip("unwrapt")


@pytest.fixture
def project(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    # A project whose entrypoint imports a view module of its own, which
    # imports a package from outside the project, and whose hooks import
    # a module of their own.

    root = tmp_path / "project"
    vendor = tmp_path / "vendor"
    root.mkdir()
    vendor.mkdir()
    (root / "shop_app.py").write_text("import shop_views\n")
    (root / "shop_views.py").write_text("import vendored\nimport shop_slow\n")
    (root / "shop_slow.py").write_text("import time\ntime.sleep(0.005)\n")
    (root / "shop_hooks.py").write_text("import shop_cheap\n")
    (root / "shop_cheap.py").write_text("import time\ntime.sleep(0.002)\n")
    (vendor / "vendored.py").write_text("import vendored_part\n")
    (vendor / "vendored_part.py").write_text("import time\ntime.sleep(0.003)\n")
    monkeypatch.chdir(root)
    monkeypatch.syspath_prepend(str(vendor))
    monkeypatch.syspath_prepend(str(root))
    monkeypatch.delenv("UNWRAPT_CONFIG", raising=False)
    before = set(sys.modules)

    try:
        yield root
    finally:
        unwrapt.uninstall()
        _imports.uninstall()
        _lazy_imports._unwrapt = None
        _lazy_imports._payload = None
        _lazy_imports._scopes.clear()
        _lazy_imports._started.clear()
        _lazy_imports._targets.clear()
        _lazy_imports.configure({})

        for name in set(sys.modules) - before:
            if not name.startswith(("unwrapt", "wrapt")):
                del sys.modules[name]


def load(context: Context, outcome: str, modules: tuple[str, ...] = ()) -> None:
    """Load the project as a host does, under the import check, each entry point bracketed.

    `modules` are the release's preload_modules, loaded after the entrypoint.
    """

    _lazy_imports.load_started(context)

    with preload.ImportRecorder() as recorder:
        for target, module in (
            (Target("hooks", "", "shop_hooks"), "shop_hooks"),
            (Target("entrypoint", "", "shop_app:app"), "shop_app"),
            *((Target("module", "", name), name) for name in modules),
        ):
            _lazy_imports.before_import(context, target)
            recorder.import_module(module)
            _lazy_imports.after_import(context, target)

    _lazy_imports.load_finished(context, outcome)


def test_the_hosts_load_is_labelled_by_target_and_the_advice_is_configuration(
    project: Path,
) -> None:
    _lazy_imports.configure({"defer": False, "report": True, "workers": 3})
    load(Context(app="shop", release="r2"), _instrumentation.FORK)
    payload = _lazy_imports.payload()

    assert payload is not None
    assert payload["labels"] == {
        "app": "shop",
        "release": "r2",
        "role": "host",
        "outcome": "fork",
        "python": sys.version.split()[0],
    }

    # Each import carries the target that loaded it.

    targets = {row["imported"]: row["target"] for row in payload["candidates"]}
    assert targets["shop_cheap"] == "hooks"
    assert targets["shop_views"] == "entrypoint"

    # An import in the project's own module is advised to use its
    # __lazy_modules__, one in a package from outside the project
    # unwrapt's table, and who pays is the host's preload, shared by the
    # workers.

    advice = {row["module"]: row for row in payload["findings"] if row["kind"] == "defer"}
    assert advice["shop_slow"]["config"] == "__lazy_modules__ = ['shop_slow']"
    assert advice["vendored_part"]["config"] == (
        '[tool.unwrapt.lazy_modules]\n"vendored" = ["vendored_part"]'
    )
    assert "shared by the 3 workers" in advice["shop_slow"]["pays"]

    # A forked worker's copy of the host's report is not its own, and the
    # host takes its report as one string, keeping no parsed copy.

    assert _lazy_imports.worker_answer()["report"] is None
    taken = _lazy_imports.take_payload()
    assert taken is not None and json.loads(taken)["labels"]["role"] == "host"
    assert _lazy_imports.payload() is None


def test_a_module_loaded_by_a_from_import_is_counted_and_advised_only_in_own_code(
    project: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Issue 788: the import check no longer hides what a from-import
    # loads. A package from outside the project, imported only by from
    # statements, is counted; a from-import of the project's own is
    # advised once it takes long enough, a library's never.

    package = project.parent / "vendor" / "vendored_pkg"
    package.mkdir()
    (package / "__init__.py").write_text("from vendored_pkg import part\n")
    (package / "part.py").write_text("import time\ntime.sleep(0.004)\n")
    (project / "shop_app.py").write_text(
        "from vendored_pkg import part\nfrom shop_slow import time\n"
    )
    monkeypatch.setattr(_import_report, "REWRITE_MIN_MS", 4.0)
    _lazy_imports.configure({"defer": False, "report": True, "workers": 2})
    load(Context(), _instrumentation.FORK)
    payload = _lazy_imports.payload()

    assert payload is not None
    edges = [edge for edge in payload["report"]["edges"] if edge["importer"] == "shop_app"]
    loaded = {edge["imported"]: edge for edge in edges if edge["loaded"]}
    assert loaded["vendored_pkg"]["kind"] == "from"
    assert loaded["vendored_pkg"]["file"].endswith("shop_app.py")
    assert loaded["vendored_pkg"]["line"] == 1
    assert loaded["vendored_pkg"]["cum_ms"] >= 4.0
    assert payload["summary"]["total_import_ms"] >= 4.0 + 5.0

    advice = {row["module"]: row for row in payload["findings"] if row["kind"] == "defer"}
    assert "vendored_pkg" not in advice
    assert "below it only once rewritten as import shop_slow" in advice["shop_slow"]["advice"]


def test_nothing_a_preload_modules_entry_loads_is_advised_and_its_cost_is_kept(
    project: Path,
) -> None:
    # Issue 785: the entry says the project wants the module loaded before
    # the fork, so deferring what it imports would undo it.

    (project / "shop_app.py").write_text("")
    (project / "shop_reports.py").write_text("import vendored\nimport shop_slow\n")
    _lazy_imports.configure({"defer": False, "report": True, "workers": 2})
    load(Context(), _instrumentation.FORK, modules=("shop_reports",))
    payload = _lazy_imports.payload()

    assert payload is not None
    targets = {row["imported"]: row["target"] for row in payload["candidates"]}
    assert targets["shop_slow"] == "module:shop_reports"
    assert not [row for row in payload["findings"] if row["target"].startswith("module:")]

    costs = {row["target"]: row["ms"] for row in payload["targets"]}
    assert set(costs) == {"hooks", "entrypoint", "module:shop_reports"}
    assert costs["module:shop_reports"] >= 5.0 + 3.0


def test_a_deferral_used_before_the_fork_gains_nothing(project: Path) -> None:
    (project / "unwrapt.toml").write_text('[lazy_modules]\n"shop_views" = ["vendored"]\n')
    (project / "shop_app.py").write_text("import shop_views\nshop_views.vendored.__name__\n")
    _lazy_imports.configure({"defer": True, "report": True, "workers": 2})
    load(Context(), _instrumentation.FORK)
    payload = _lazy_imports.payload()

    assert payload is not None
    used = [row for row in payload["findings"] if row["kind"] == "used_before_fork"]
    assert [(row["module"], row["importer"]) for row in used] == [("vendored", "shop_views")]


def test_a_worker_reports_its_own_load_and_the_modules_it_loaded_since(project: Path) -> None:
    _lazy_imports.configure({"defer": False, "report": True, "workers": 2})
    load(Context(worker_id=1, group="reports"), _instrumentation.SERVE)
    _imports.install()
    _imports._start("R7")
    token = _request_id.set("R7")

    try:
        __import__("json.tool")
    finally:
        _request_id.reset(token)

    answer = _lazy_imports.worker_answer()

    assert answer["report"]["labels"]["worker"] == "reports/1"
    loads = {load["module"]: load for load in answer["modules"]["loads"]}
    assert loads["json.tool"]["request"] == "R7"
    assert loads["json.tool"]["deferred"] is False
