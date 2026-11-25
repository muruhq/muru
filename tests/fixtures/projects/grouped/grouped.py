"""A project whose muru.toml declares worker groups (section 7.1).

The response says which group and worker served it, and the group the
environment names, so a test can see paths reach the right workers. A
query string of ms=N holds the request for that long, for the group
timeouts, in steps of 10 ms, so that a soft timeout's interrupt is
raised soon after it is set rather than when one long sleep ends.
"""

from __future__ import annotations

import json
import os
import time
from collections.abc import Callable, Iterable
from typing import Any

from muru import context


def application(environ: dict[str, Any], start_response: Callable[..., Any]) -> Iterable[bytes]:
    """Report the group, the worker and the path."""

    query = environ.get("QUERY_STRING", "")

    if query.startswith("ms="):
        until = time.monotonic() + int(query[3:]) / 1000

        while time.monotonic() < until:
            time.sleep(0.01)

    body = json.dumps(
        {
            "group": context().group,
            "env_group": os.environ.get("MURU_GROUP"),
            "worker": context().worker_id,
            "env_worker": os.environ.get("MURU_WORKER_ID"),
            "development": context().development,
            "env_development": os.environ.get("MURU_DEVELOPMENT"),
            "pid": os.getpid(),
            "path": environ["PATH_INFO"],
        }
    ).encode()
    start_response(
        "200 OK", [("Content-Type", "application/json"), ("Content-Length", str(len(body)))]
    )

    return [body]
