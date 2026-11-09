"""Hooks that say when a worker's shutdown work runs, for the stopping tests."""

from __future__ import annotations

import sys

from muru import Context


def on_exit(context: Context) -> None:
    print("STOP on_exit ran", file=sys.stderr, flush=True)
