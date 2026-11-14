"""Fixtures shared by the runtime library's unit tests."""

from __future__ import annotations

import signal
import sys
import types
from collections.abc import Iterator
from typing import Any

import pytest

import muru
from muru import _signals


@pytest.fixture
def native(monkeypatch: pytest.MonkeyPatch) -> Iterator[Any]:
    """The runtime's extension, or a stand-in where it is not built.

    The unit tests run without the extension in CI, so code that tells
    the runtime something, such as a thread's name, calls a stand-in
    whose functions do nothing and which a test may replace.
    """

    try:
        from muru import _native
    except ImportError:
        stand_in: Any = types.ModuleType("muru._native")
        stand_in.name_thread = lambda name: None
        stand_in.loop_beat = lambda: None
        stand_in.sync_pool = lambda *arguments: None
        monkeypatch.setitem(sys.modules, "muru._native", stand_in)
        monkeypatch.setattr(muru, "_native", stand_in, raising=False)
        yield stand_in
    else:
        yield _native


@pytest.fixture(autouse=True)
def _signals_restored(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """Put signal.signal back after a test that ran the host in this process.

    The host and its workers replace it to keep their own signals, which a
    test calling host.main() would otherwise leave in place for the tests
    after it.
    """

    monkeypatch.setattr(signal, "signal", signal.signal)
    monkeypatch.setattr(_signals, "_kept", _signals._kept)
    yield
