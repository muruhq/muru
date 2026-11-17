"""Deferring imports with unwrapt (design specification section 6.17).

A project turns deferral on with ``lazy_imports = true``, and the
release's instrumentation list then starts with this module (section
6.15), ahead of tracing's, so the imports of the modules after it can be
deferred too. unwrapt is the application's dependency, not Muru's: it is
imported only here, only when the key is on, from the project's
environment.

- ``load_started`` installs unwrapt's deferral with the configuration
  unwrapt finds itself (``UNWRAPT_CONFIG``, else ``unwrapt.toml``, else a
  ``[tool.unwrapt]`` table in ``pyproject.toml``, in the directory the
  host runs in), before anything of the application is imported. Below
  Python 3.15 each module's ``__lazy_modules__`` is honoured as well as
  the configuration's ``lazy_modules``; from 3.15 the interpreter honours
  ``__lazy_modules__`` itself, and unwrapt is installed only when its
  ``lazy_modules`` has entries. Deferral stays installed through the
  fork, and each deferred module loads in a worker on first use.

- ``load_finished`` writes one line to the release's log when the host
  forks, naming the deferred imports already used before the fork, which
  under preload save nothing.

- ``worker_started`` subscribes to unwrapt's resolutions, so a module a
  request loads on first use is marked as deferred in the request's
  record (``muru._imports``).

A missing or too old unwrapt, or a configuration file it cannot use,
raises from ``load_started``; Muru logs that with the reason, and the
release starts without deferral.
"""

from __future__ import annotations

import importlib
import sys
from collections.abc import Callable
from typing import Any

from muru import _imports
from muru._instrumentation import FORK

__all__ = ["load_finished", "load_started", "worker_started"]

# The most deferred imports the line at the fork names one by one.

NAMED = 10

# unwrapt, once load_started installed it; None until then or when it
# failed, so the later callbacks do nothing.

_unwrapt: Any = None

# What unsubscribes from unwrapt's resolutions, once worker_started has
# subscribed.

_unsubscribe: Callable[[], None] | None = None


def load_started(context: Any) -> None:
    """Install unwrapt's deferral before the application is imported.

    Raises when unwrapt is missing or too old, or its configuration
    cannot be used; Muru logs that, and the release starts without
    deferral.
    """

    global _unwrapt

    unwrapt = _module()

    try:
        config = unwrapt.load_config()
    except unwrapt.ConfigError as error:
        raise RuntimeError(f"lazy_imports cannot use the unwrapt configuration: {error}") from error

    # unwrapt installed already, by its own bootstrap, keeps the settings
    # it was installed with.

    if not unwrapt.install(config, defer=True) and unwrapt.installed() is not None:
        _write(
            "lazy_imports: unwrapt was already installed in this process, so its own settings apply"
        )

    _unwrapt = unwrapt


def load_finished(context: Any, outcome: str) -> None:
    """Say at the fork which deferred imports were used before it."""

    if _unwrapt is None or outcome != FORK:
        return

    deferrals = _unwrapt.deferred()

    if not deferrals:
        return

    used = [deferral for deferral in deferrals if deferral.resolved]
    imports = "import" if len(deferrals) == 1 else "imports"

    if not used:
        _write(
            f"lazy_imports: {len(deferrals)} {imports} deferred, none of them used before the fork"
        )
        return

    named = ", ".join(
        f"{deferral.module} (by {deferral.importer})" if deferral.importer else deferral.module
        for deferral in used[:NAMED]
    )
    more = f", and {len(used) - NAMED} more" if len(used) > NAMED else ""

    _write(
        f"lazy_imports: {len(deferrals)} {imports} deferred, {len(used)} of them used before "
        f"the fork, where deferring them saves nothing: {named}{more}"
    )


def worker_started(context: Any) -> None:
    """Mark a deferred module a request loads in the worker as deferred, in its record."""

    global _unsubscribe

    if _unwrapt is None or _unsubscribe is not None:
        return

    _unsubscribe = _unwrapt.on_resolve(_resolved)


def _resolved(resolution: Any) -> None:
    # A deferred import resolving, in the thread that used it, once the
    # module is imported: it was loaded then unless something else had
    # loaded it first.

    if resolution.loaded:
        _imports.mark_deferred(resolution.module)


def _write(line: str) -> None:
    # One line of the host's own, on standard error, which the host tags.

    try:
        sys.stderr.write(line + "\n")
        sys.stderr.flush()
    except (OSError, ValueError):
        pass


def _module() -> Any:
    # unwrapt from the project's environment, with what to do when it is
    # not there or is too old.

    try:
        module = importlib.import_module("unwrapt")
    except ImportError as error:
        raise RuntimeError(
            "lazy_imports needs the unwrapt package in the project's environment; "
            f"add it to the project's dependencies ({error})"
        ) from error

    # install() taking a configuration, deferred() and on_resolve()
    # arrived in 1.0.0a2.

    if not (hasattr(module, "on_resolve") and hasattr(module, "deferred")):
        raise RuntimeError(
            "lazy_imports needs unwrapt 1.0.0a2 or later, and the environment has "
            f"{getattr(module, '__version__', 'an older one')}"
        )

    return module
