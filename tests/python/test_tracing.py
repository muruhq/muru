"""Unit tests for tracing with wrapture (section 11.6).

wrapture is the application's dependency, not Muru's, so a stand-in module
records what ``muru._tracing`` asks of it: the configuration applied with its
outputs held back, the outputs started, and the request middleware.
"""

from __future__ import annotations

import sys
import types
from collections.abc import Iterator
from dataclasses import dataclass
from typing import Any

import pytest

from muru import _tracing
from muru._instrumentation import Target

# wrapture, and so tracing, needs Python 3.12 or later.

pytestmark = pytest.mark.skipif(
    sys.version_info < (3, 12), reason="tracing needs Python 3.12 or later"
)


@dataclass
class Context:
    worker_id: int | None = None


class Applied:
    def __init__(self) -> None:
        self.starts = 0

    def start(self) -> None:
        self.starts += 1


class Middleware:
    def __init__(self, application: Any, *, label: str) -> None:
        self.application = application
        self.label = label


@pytest.fixture
def wrapture(monkeypatch: pytest.MonkeyPatch) -> Iterator[types.SimpleNamespace]:
    applied = Applied()
    loaded: list[str] = []
    held: list[bool] = []

    class Config:
        instrument = (types.SimpleNamespace(name="flask"), types.SimpleNamespace(name="sqlite3"))
        observe = ()

        def apply(self, *, start: bool = True) -> Applied:
            held.append(not start)
            return applied

    def load_config(source: str) -> Config:
        loaded.append(source)
        return Config()

    module = types.ModuleType("wrapture")
    module.AppliedConfig = Applied  # type: ignore[attr-defined]
    module.find_config = lambda: "pyproject.toml"  # type: ignore[attr-defined]
    module.load_config = load_config  # type: ignore[attr-defined]
    module.WSGIMiddleware = Middleware  # type: ignore[attr-defined]
    module.ASGIMiddleware = Middleware  # type: ignore[attr-defined]
    module.Sink = type("Sink", (), {})  # type: ignore[attr-defined]
    module.add_sink = lambda sink: sink  # type: ignore[attr-defined]

    monkeypatch.setitem(sys.modules, "wrapture", module)
    monkeypatch.setattr(_tracing, "_applied", None)
    monkeypatch.setattr(_tracing, "_toolbar", False)

    yield types.SimpleNamespace(applied=applied, loaded=loaded, held=held, module=module)


def test_the_host_holds_the_outputs_back_and_each_worker_starts_them(
    wrapture: types.SimpleNamespace,
) -> None:
    _tracing.configure({"middleware": {"entrypoint": False, "mounts": []}})
    _tracing.load_started(Context(worker_id=None))

    assert wrapture.loaded == ["pyproject.toml"]
    assert wrapture.held == [True]
    assert wrapture.applied.starts == 0

    _tracing.worker_started(Context(worker_id=0))
    assert wrapture.applied.starts == 1


def test_a_worker_that_loads_starts_the_outputs_at_once(
    wrapture: types.SimpleNamespace,
) -> None:
    _tracing.configure({"config": "tracing/wrapture.toml"})
    _tracing.load_started(Context(worker_id=1))

    assert wrapture.loaded == ["tracing/wrapture.toml"]
    assert wrapture.applied.starts == 1


def test_only_the_entry_points_asked_for_are_wrapped(wrapture: types.SimpleNamespace) -> None:
    _tracing.configure({"middleware": {"entrypoint": False, "mounts": ["raw"]}})
    _tracing.load_started(Context())

    app = object()
    entrypoint = Target("entrypoint", "", "shop:app")
    raw = Target("mount", "raw", "raw:app")

    assert _tracing.wrap_application(Context(), entrypoint, app, "wsgi") is None
    assert _tracing.wrap_application(Context(), raw, app, "grpc") is None

    wrapped = _tracing.wrap_application(Context(), raw, app, "asgi")
    assert isinstance(wrapped, Middleware)
    assert wrapped.application is app
    assert wrapped.label == "raw:app"


def test_the_start_up_says_once_what_tracing_applies(
    wrapture: types.SimpleNamespace,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    # Issue 781: in the host, or the first worker when each loads.

    monkeypatch.delenv("WRAPTURE_CONFIG", raising=False)
    _tracing.configure({})
    _tracing.load_started(Context(worker_id=None))
    assert capsys.readouterr().err == (
        "tracing on: wrapture configuration from [tool.wrapture] in ./pyproject.toml, "
        "instrumenting flask, sqlite3\n"
    )

    _tracing.configure({"config": "tracing/wrapture.toml"})
    _tracing.load_started(Context(worker_id=1))
    assert capsys.readouterr().err == ""
    _tracing.load_started(Context(worker_id=0))
    assert "from ./tracing/wrapture.toml, instrumenting" in capsys.readouterr().err


def test_no_configuration_or_no_wrapture_is_said_plainly(
    wrapture: types.SimpleNamespace,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    _tracing.configure({})
    monkeypatch.setattr(wrapture.module, "find_config", lambda: None)

    with pytest.raises(RuntimeError, match="no wrapture configuration was found"):
        _tracing.load_started(Context())

    # Issue 781: a one-line warning, beside the traceback Muru logs.

    assert capsys.readouterr().err.startswith(
        "warning: the release is served untraced: tracing is on but no "
        "wrapture configuration was found"
    )

    monkeypatch.setitem(sys.modules, "wrapture", None)

    with pytest.raises(RuntimeError, match="wrapture package in the project's environment"):
        _tracing.load_started(Context())

    assert (
        _tracing.wrap_application(Context(), Target("entrypoint", "", "a:b"), object(), "wsgi")
        is None
    )
