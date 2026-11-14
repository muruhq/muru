"""Unit tests for the signals Muru keeps for itself (issue 673)."""

from __future__ import annotations

import asyncio
import logging
import signal

import pytest

from muru import _signals


def handler(signum: int, frame: object) -> None:
    pass


def test_a_kept_signal_is_refused_with_a_warning_and_others_pass(
    caplog: pytest.LogCaptureFixture,
) -> None:
    before = signal.getsignal(signal.SIGUSR1)
    _signals.keep(frozenset({signal.SIGUSR1}))

    with caplog.at_level(logging.WARNING, logger="muru"):
        returned = signal.signal(signal.SIGUSR1, handler)

    assert returned == before
    assert signal.getsignal(signal.SIGUSR1) == before
    assert "signal.signal(SIGUSR1) from test_signals:" in caplog.text
    assert "Muru handles SIGUSR1 in this process" in caplog.text

    previous = signal.signal(signal.SIGUSR2, handler)

    try:
        assert signal.getsignal(signal.SIGUSR2) is handler
    finally:
        _signals.install(signal.SIGUSR2, previous)


def test_muru_installs_its_own_past_the_guard() -> None:
    before = signal.getsignal(signal.SIGUSR1)
    _signals.keep(frozenset({signal.SIGUSR1}))

    try:
        _signals.install(signal.SIGUSR1, handler)
        assert signal.getsignal(signal.SIGUSR1) is handler
    finally:
        _signals.install(signal.SIGUSR1, before)


def test_an_event_loops_handler_is_refused_too(caplog: pytest.LogCaptureFixture) -> None:
    _signals.keep(frozenset({signal.SIGUSR1}))
    loop = asyncio.new_event_loop()

    try:
        with caplog.at_level(logging.WARNING, logger="muru"):
            loop.add_signal_handler(signal.SIGUSR1, lambda: None)

        assert "signal.signal(SIGUSR1) from test_signals:" in caplog.text
    finally:
        loop.remove_signal_handler(signal.SIGUSR1)
        loop.close()
