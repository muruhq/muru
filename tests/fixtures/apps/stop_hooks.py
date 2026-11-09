"""Hooks that say when a worker's shutdown work runs, for the stopping tests."""

from __future__ import annotations

import asyncio
import atexit
import os
import sys

import muru
from muru import Context


def say(text: str) -> None:
    print(f"STOP {text}", file=sys.stderr, flush=True)


def at_exit() -> None:
    say(f"atexit ran pid={os.getpid()}")
    loop = muru.loop()

    if loop is not None:
        asyncio.run_coroutine_threadsafe(asyncio.sleep(0), loop).result(1)
        say("atexit used the loop")


def post_fork(context: Context) -> None:
    atexit.register(at_exit)


def on_ready(context: Context) -> None:
    if os.environ.get("MURU_TEST_FAIL_ON_READY"):
        raise RuntimeError("this worker will not start")


def on_exit(context: Context) -> None:
    say("on_exit ran")
