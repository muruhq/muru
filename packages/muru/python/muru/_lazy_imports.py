"""Deferring imports with unwrapt, and the import report (specification sections 6.17, 6.18).

A project turns deferral on with ``lazy_imports = true``, the import
report with ``import_report = true``, or both, and the release's
instrumentation list then starts with this module (section 6.15), ahead
of tracing's, so the imports of the modules after it can be deferred and
recorded too. Its settings say which: ``defer`` and ``report``, with
``workers``, how many workers the release starts with, for the report's
analysis. unwrapt is the application's dependency, not Muru's: it is
imported only here, only when a key is on, from the project's
environment.

- ``load_started`` installs unwrapt with the configuration it finds itself
  (``UNWRAPT_CONFIG``, else ``unwrapt.toml``, else a ``[tool.unwrapt]``
  table in ``pyproject.toml``, in the directory the host runs in), before
  anything of the application is imported: the shim when deferring, and
  the profile recorder for the report. Below Python 3.15 the shim
  honours each module's ``__lazy_modules__`` as well as the
  configuration's ``lazy_modules``; from 3.15 the interpreter honours
  ``__lazy_modules__`` itself, and unwrapt installs its shim only when
  ``lazy_modules`` has entries. Deferral stays installed through the
  fork, and each deferred module loads in a worker on first use.

- ``before_import`` and ``after_import`` label the imports of each entry
  point the release loads, for the report: ``hooks``, ``entrypoint``,
  ``mount:NAME`` and ``module:NAME`` for one of the ``preload_modules``,
  and time each one's import as a whole.
  Imports outside any pair, by an instrumentation module, ``on_load`` or
  ``pre_fork``, carry no target.

- ``load_finished`` stops the recorder, on every outcome, labels the
  report and has ``muru._import_report`` analyse it, keeping the result
  for the host to send to the controller; and when the host forks it
  writes one line to the release's log naming the deferred imports
  already used before the fork, which under preload save nothing.

- ``worker_started`` subscribes to unwrapt's resolutions, so a module a
  request loads on first use is marked as deferred in the request's
  record (``muru._imports``).

A missing or too old unwrapt, or a configuration file it cannot use,
raises from ``load_started``; Muru logs that with the reason, and the
release starts without deferral or a report.
"""

from __future__ import annotations

import importlib
import json
import os
import sys
import time
from collections.abc import Callable
from typing import Any

from muru import _imports
from muru._instrumentation import FORK

__all__ = [
    "after_import",
    "before_import",
    "configure",
    "load_finished",
    "load_started",
    "payload",
    "summary",
    "take_payload",
    "worker_answer",
    "worker_started",
]

# The most deferred imports the line at the fork names one by one.

NAMED = 10

# The oldest unwrapt the module works with, as its __version_info__.

UNWRAPT_LEAST = ("1", "0", "0", "a5")

# The settings Muru gives the module.

_defer = True
_report = False
_workers = 1

# unwrapt, once load_started installed it; None until then or when it
# failed, so the later callbacks do nothing.

_unwrapt: Any = None

# How many deferrals unwrapt had handed out before load_started, which
# lists every one in the process, so the line at the fork counts this
# load's alone.

_deferred_before = 0

# The scopes before_import pushed, which after_import pops, each with
# its target's label and when its import began; and each target's label
# and milliseconds once its import ended (issue 785).

_scopes: list[Any] = []
_started: list[tuple[str, float]] = []
_targets: list[dict[str, Any]] = []

# The load's import report and Muru's analysis of it, once load_finished
# has built them.

_payload: dict[str, Any] | None = None

# What unsubscribes from unwrapt's resolutions, once worker_started has
# subscribed.

_unsubscribe: Callable[[], None] | None = None


def configure(settings: dict[str, Any]) -> None:
    """Take the settings Muru built from the release's lazy_imports and import_report keys."""

    global _defer, _report, _workers

    _defer = bool(settings.get("defer", True))
    _report = bool(settings.get("report", False))
    _workers = max(1, int(settings.get("workers", 1) or 1))


def load_started(context: Any) -> None:
    """Install unwrapt, deferring and recording as asked, before the application is imported.

    Raises when unwrapt is missing or too old, or its configuration
    cannot be used; Muru logs that, and the release starts without
    deferral or a report.
    """

    global _deferred_before, _unwrapt

    unwrapt = _module()

    try:
        config = unwrapt.load_config()
    except unwrapt.ConfigError as error:
        raise RuntimeError(f"{_key()} cannot use the unwrapt configuration: {error}") from error

    # unwrapt installed already, by its own bootstrap, keeps the settings
    # it was installed with.

    if (
        not unwrapt.install(config, defer=_defer, profile=_report)
        and unwrapt.installed() is not None
    ):
        _write(
            f"{_key()}: unwrapt was already installed in this process, so its own settings apply"
        )

    _deferred_before = len(unwrapt.deferred())
    _unwrapt = unwrapt


def before_import(context: Any, target: Any) -> None:
    """Label the imports of the hooks module, the entrypoint, a mount or a listed module."""

    if _unwrapt is None or not _report:
        return

    if target.kind in ("mount", "module"):
        label = f"{target.kind}:{target.name or target.spec}"
    else:
        label = target.kind

    _scopes.append(_unwrapt.push_scope(target=label))
    _started.append((label, time.perf_counter()))


def after_import(context: Any, target: Any) -> None:
    """End the label before_import began, and keep how long the target's import took."""

    if _unwrapt is None or not _scopes:
        return

    _unwrapt.pop_scope(_scopes.pop())

    if _started:
        label, started = _started.pop()
        _targets.append({"target": label, "ms": (time.perf_counter() - started) * 1000.0})


def load_finished(context: Any, outcome: str) -> None:
    """Build the import report, and say at the fork which deferred imports were used before it."""

    if _unwrapt is None:
        return

    if _report:
        _record(context, outcome)

    if _defer and outcome == FORK:
        _say_deferred()


def worker_started(context: Any) -> None:
    """Mark a deferred module a request loads in the worker as deferred, in its record."""

    global _unsubscribe

    if _unwrapt is None or not _defer or _unsubscribe is not None:
        return

    _unsubscribe = _unwrapt.on_resolve(_resolved)


def payload() -> dict[str, Any] | None:
    """Return the load's import report with Muru's analysis, once load_finished built it."""

    return _payload


def summary() -> dict[str, Any] | None:
    """Return the headline figures of the host's import report, for the event of its load."""

    if _payload is None or _payload["labels"].get("role") == "worker":
        return None

    figures = _payload["summary"]

    return {
        "import_ms": round(figures["total_import_ms"], 1),
        "modules": figures["modules_loaded"],
        "saving_ms": round(figures["estimated_saving_ms"], 1),
        "deferred_used": len(_payload["deferred_used"]),
    }


def take_payload() -> str | None:
    """Take the host's import report as JSON, leaving none behind as objects.

    A worker's own report, made when it loaded the application itself, is
    kept for ``worker_answer``.
    """

    global _payload

    if _payload is None or _payload["labels"].get("role") == "worker":
        return None

    text = json.dumps(_payload)
    _payload = None
    return text


def worker_answer() -> dict[str, Any]:
    """Answer ``muructl debug imports`` in a worker.

    The modules the worker has loaded since the fork, and the report of
    its own load when it loaded the application itself, with per-worker
    imports; a forked worker's copy of the host's report is the host's,
    and is left out.
    """

    own = _payload if _payload and _payload["labels"].get("role") == "worker" else None
    return {"modules": _imports.worker_loads(), "report": own}


def _record(context: Any, outcome: str) -> None:
    # The recorder's report, labelled with what it is of, and Muru's
    # analysis of it.

    global _payload

    from muru import _import_report, preload

    # A scope a failed import left open is closed first.

    while _scopes:
        _unwrapt.pop_scope(_scopes.pop())

    _started.clear()
    targets = list(_targets)
    _targets.clear()

    report = _unwrapt.stop_profile(write=False)

    if report is None:
        return

    worker = context.worker_id
    labels = {
        "app": context.app,
        "release": context.release,
        "role": "host" if worker is None else "worker",
        "outcome": outcome,
        "python": sys.version.split()[0],
    }

    if worker is not None:
        labels["worker"] = (
            str(worker) if context.group == "default" else f"{context.group}/{worker}"
        )

    report.labels.update(labels)
    _payload = _import_report.analyse(
        report,
        outcome=outcome,
        workers=_workers,
        found=preload.latest_findings() if worker is None else (),
        root=os.getcwd(),
        targets=targets,
    )


def _say_deferred() -> None:
    # One line at the fork: what was deferred, and what of it the host's
    # load used anyway.

    deferrals = _unwrapt.deferred()[_deferred_before:]

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


def _resolved(resolution: Any) -> None:
    # A deferred import resolving, in the thread that used it, once the
    # module is imported: it was loaded then unless something else had
    # loaded it first.

    if resolution.loaded:
        _imports.mark_deferred(resolution.module)


def _key() -> str:
    # The key that turned the module on, for its messages.

    return "lazy_imports" if _defer else "import_report"


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
            f"{_key()} needs the unwrapt package in the project's environment; "
            f"add it to the project's dependencies ({error})"
        ) from error

    # The import report reads what 1.0.0a5 added: the statement's own
    # place under a wrapping hook, extension modules' imports, declared
    # and deferred laziness apart, and use at load (section 6.18).

    if _version(getattr(module, "__version_info__", ())) < _version(UNWRAPT_LEAST):
        raise RuntimeError(
            f"{_key()} needs unwrapt {'.'.join(UNWRAPT_LEAST[:3])}{UNWRAPT_LEAST[3]} or later, "
            f"and the environment has {getattr(module, '__version__', 'an older one')}"
        )

    return module


def _version(info: tuple[str, ...]) -> tuple[int, int, int, int, int]:
    # unwrapt's __version_info__, such as ("1", "0", "0", "a5"), as a key
    # that sorts as the releases do: an alpha before a beta before a
    # release candidate before the release. Anything unreadable sorts
    # first, as too old.

    try:
        major, minor, micro = (int(part) for part in info[:3])
    except ValueError:
        return (0, 0, 0, 0, 0)

    suffix = info[3] if len(info) > 3 else ""

    for rank, tag in enumerate(("a", "b", "rc")):
        number = suffix[len(tag) :]

        if suffix.startswith(tag) and number.isdigit():
            return (major, minor, micro, rank, int(number))

    return (major, minor, micro, 3, 0)
