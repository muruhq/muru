"""Unit tests for muru._lazy_imports: deferring imports with unwrapt.

They use unwrapt itself, from the dev group, with a configuration's
``lazy_modules`` so that the imports are deferred by unwrapt on every
Python version, and uninstall it again after each test.
"""

from __future__ import annotations

import importlib
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

from muru import _imports, _instrumentation, _lazy_imports
from muru._context import Context, _request_id

unwrapt = pytest.importorskip("unwrapt")


@pytest.fixture
def project(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    # A project directory, the host's working directory, whose importer
    # module's import of heavy unwrapt's configuration defers.

    (tmp_path / "unwrapt.toml").write_text('[lazy_modules]\n"importer" = ["heavy"]\n')
    (tmp_path / "importer.py").write_text("import heavy\n\ndef use():\n    return heavy.VALUE\n")
    (tmp_path / "heavy.py").write_text("VALUE = 42\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.delenv("UNWRAPT_CONFIG", raising=False)
    before = set(sys.modules)

    try:
        yield tmp_path
    finally:
        if _lazy_imports._unsubscribe is not None:
            _lazy_imports._unsubscribe()

        unwrapt.uninstall()
        _imports.uninstall()
        _lazy_imports._unwrapt = None
        _lazy_imports._unsubscribe = None

        # unwrapt's own modules stay, holding its installed state.

        for name in set(sys.modules) - before:
            if not name.startswith(("unwrapt", "wrapt")):
                del sys.modules[name]


def test_the_configuration_unwrapt_finds_defers_and_the_fork_names_what_was_used(
    project: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    host = Context()
    _lazy_imports.load_started(host)

    import importer

    # The import is deferred until first use.

    assert "heavy" not in sys.modules
    _lazy_imports.load_finished(host, _instrumentation.FORK)
    assert capsys.readouterr().err == (
        "lazy_imports: 1 import deferred, none of them used before the fork\n"
    )

    # Used before the fork, it is named.

    assert importer.use() == 42
    _lazy_imports.load_finished(host, _instrumentation.FORK)
    assert capsys.readouterr().err == (
        "lazy_imports: 1 import deferred, 1 of them used before the fork, "
        "where deferring them saves nothing: heavy (by importer)\n"
    )

    # Nothing is said when nothing forks.

    _lazy_imports.load_finished(host, _instrumentation.SERVE)
    assert capsys.readouterr().err == ""


def test_a_deferred_module_a_request_loads_is_marked_in_its_record(project: Path) -> None:
    _lazy_imports.load_started(Context())

    import importer

    _imports.install()
    _lazy_imports.worker_started(Context(worker_id=0))
    _imports._start("R1")
    token = _request_id.set("R1")

    try:
        importer.use()
        importlib.import_module("json.tool")
    finally:
        _request_id.reset(token)

    loads, _count, _ms, _waits = _imports._take("R1")
    marked = {name: (trigger, deferred) for _at, _ms, name, trigger, depth, deferred in loads}

    # The line that used the deferred import is its trigger, past
    # unwrapt's frames, and a module imported plainly is not marked.

    assert marked["heavy"] == ("importer.py:4", True)
    assert marked["json.tool"][1] is False


def test_a_configuration_unwrapt_cannot_use_is_an_error_with_the_reason(project: Path) -> None:
    (project / "unwrapt.toml").write_text("lazy_modules = 3\n")

    with pytest.raises(RuntimeError, match="cannot use the unwrapt configuration"):
        _lazy_imports.load_started(Context())

    # Nothing was installed, so the later callbacks do nothing.

    assert unwrapt.installed() is None
    _lazy_imports.worker_started(Context(worker_id=0))
    assert _lazy_imports._unsubscribe is None


def test_without_unwrapt_the_error_says_to_add_it(
    project: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    with monkeypatch.context() as patched:
        patched.setitem(sys.modules, "unwrapt", None)

        with pytest.raises(RuntimeError, match="needs the unwrapt package"):
            _lazy_imports.load_started(Context())


def test_an_unwrapt_too_old_is_an_error_naming_both_versions(
    project: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(unwrapt, "__version_info__", ("1", "0", "0", "a4"))
    monkeypatch.setattr(unwrapt, "__version__", "1.0.0a4")

    with pytest.raises(RuntimeError, match=r"needs unwrapt 1\.0\.0a5 or later.*has 1\.0\.0a4"):
        _lazy_imports.load_started(Context())

    # A later release, and a release candidate of the next, are new enough.

    assert _lazy_imports._version(("1", "0", "0", "a10")) > _lazy_imports._version(
        ("1", "0", "0", "a5")
    )
    assert _lazy_imports._version(("1", "0", "0")) > _lazy_imports._version(("1", "0", "0", "rc1"))
