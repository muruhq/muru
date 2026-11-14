"""Tracing with wrapture (design specification section 11.6).

A project turns tracing on with ``tracing = true``, or a ``[tracing]``
table naming the wrapture configuration file, and the release's
instrumentation list then starts with this module (section 6.15), with
the file and the entry points to wrap in its settings. wrapture is the
application's dependency, not Muru's: it is imported only here, only
when tracing is on, from the project's environment.

The configuration is applied in two halves, so a preloading host holds
no sink, thread or open file of wrapture's when it forks:

- ``load_started`` applies it with the outputs held back, before the
  application is imported, so the patches are in place for its imports.
  Where the load runs in the process that serves, a worker with
  per-worker imports, the outputs start there and then, and the load is
  traced to the configuration's sinks.

- ``worker_started`` starts the outputs in each worker of a preloaded
  release, after the fork. What the host did while it loaded is not
  traced.

``wrap_application`` wraps the entry points the project asked for
(``tracing_middleware``) in wrapture's request middleware, so each
request records as one tree. The others are left to a framework's own
instrumentation, whose request boundary keeps its own settings.

Shutdown is wrapture's own: its atexit hook delivers what is owed, and
Muru runs the atexit callbacks in each worker as it stops.
"""

from __future__ import annotations

import importlib
import sys
from collections.abc import Mapping
from typing import Any

__all__ = ["configure", "load_started", "worker_started", "wrap_application"]

# The settings Muru gives the module: the configuration file, if named,
# and the entry points to wrap.

_config: str | None = None
_middleware_entrypoint = False
_middleware_mounts: frozenset[str] = frozenset()

# What load_started applied: wrapture's AppliedConfig, None until then
# or when it failed.

_applied: Any = None


def configure(settings: Mapping[str, Any]) -> None:
    """Take the settings Muru built from the project's tracing keys."""

    global _config, _middleware_entrypoint, _middleware_mounts

    config = settings.get("config")
    _config = config if isinstance(config, str) else None

    middleware = settings.get("middleware") or {}
    _middleware_entrypoint = bool(middleware.get("entrypoint"))
    _middleware_mounts = frozenset(middleware.get("mounts") or ())


def load_started(context: Any) -> None:
    """Apply the wrapture configuration before the application is imported.

    The outputs are held back in the host and started at once in a
    worker. Raises when wrapture is missing or no configuration is found
    or it cannot be applied; Muru logs that, and the release serves
    untraced.
    """

    global _applied

    wrapture = _wrapture()
    source = _config or wrapture.find_config()

    if source is None:
        raise RuntimeError(
            "tracing is on but no wrapture configuration was found: name the file with "
            "config in the [tracing] table, set WRAPTURE_CONFIG, or provide wrapture.toml "
            "or a [tool.wrapture] table in pyproject.toml in the project's directory"
        )

    _applied = wrapture.load_config(source).apply(start=False)

    if context.worker_id is not None:
        _applied.start()


def worker_started(context: Any) -> None:
    """Start the outputs in a worker, after the fork, if the load has not."""

    if _applied is not None:
        _applied.start()


def wrap_application(context: Any, target: Any, application: Any, interface: str) -> Any:
    """Wrap an entry point the project asked for in wrapture's request middleware.

    Returns None, serving the application unchanged, for the others, for
    gRPC, and when the configuration was not applied.
    """

    if _applied is None or interface not in ("wsgi", "asgi"):
        return None

    wanted = (target.kind == "entrypoint" and _middleware_entrypoint) or (
        target.kind == "mount" and target.name in _middleware_mounts
    )

    if not wanted:
        return None

    wrapture = _wrapture()

    # The request event is labelled with the entry point's spec, as the
    # project names it, rather than the application's type.

    if interface == "wsgi":
        return wrapture.WSGIMiddleware(application, label=target.spec)

    return wrapture.ASGIMiddleware(application, label=target.spec)


def _wrapture() -> Any:
    # wrapture from the project's environment, with what to do when it is
    # not there.

    # Muru runs on 3.11, which wrapture does not support. The version is
    # read as plain numbers so the check stands for every version Muru
    # is type-checked as.

    version = (sys.version_info.major, sys.version_info.minor)

    if version < (3, 12):
        raise RuntimeError("tracing needs wrapture, which needs Python 3.12 or later")

    try:
        module = importlib.import_module("wrapture")
    except ImportError as error:
        raise RuntimeError(
            "tracing needs the wrapture package in the project's environment; "
            f"add it to the project's dependencies ({error})"
        ) from error

    # apply(start=False) and AppliedConfig.start() arrived in 1.0.0b5.

    if not hasattr(module.AppliedConfig, "start"):
        raise RuntimeError(
            f"tracing needs wrapture 1.0.0b5 or later, and the environment has "
            f"{getattr(module, '__version__', 'an older one')}"
        )

    return module
