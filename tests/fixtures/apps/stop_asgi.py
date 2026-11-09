"""An ASGI application with a stuck route and a lifespan shutdown that takes a moment."""

from __future__ import annotations

import asyncio
import sys
from collections.abc import Awaitable, Callable, MutableMapping
from typing import Any

Scope = MutableMapping[str, Any]
Receive = Callable[[], Awaitable[dict[str, Any]]]
Send = Callable[[dict[str, Any]], Awaitable[None]]


def say(text: str) -> None:
    print(f"STOP {text}", file=sys.stderr, flush=True)


async def application(scope: Scope, receive: Receive, send: Send) -> None:
    if scope["type"] == "lifespan":
        while True:
            message = await receive()

            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                say("lifespan shutdown began")
                await asyncio.sleep(0.2)
                say("lifespan shutdown finished")
                await send({"type": "lifespan.shutdown.complete"})
                return

    say("request began")
    await asyncio.sleep(60)
    await send({"type": "http.response.start", "status": 200, "headers": []})
    await send({"type": "http.response.body", "body": b"late\n"})
