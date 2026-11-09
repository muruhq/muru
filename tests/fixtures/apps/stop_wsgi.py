"""A WSGI application with a stuck route."""

from __future__ import annotations

import sys
import time
from collections.abc import Callable, Iterable
from typing import Any

StartResponse = Callable[[str, list[tuple[str, str]]], Any]


def application(environ: dict[str, Any], start_response: StartResponse) -> Iterable[bytes]:
    print("STOP request began", file=sys.stderr, flush=True)
    time.sleep(60)
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"late\n"]
