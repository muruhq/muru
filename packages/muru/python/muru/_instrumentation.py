"""Instrumentation modules (design specification section 6.15, ADR 0064).

A release lists instrumentation modules apart from its hooks module: tools
such as an import timer or an APM agent, set up by someone who need not
change the application. They are imported wherever the application is, in
the host under preload and in each worker with per-worker imports, and
before anything of it. Each is imported, configured and started before the
next is imported, so an earlier one can measure a later one's import.

A module defines any of the callbacks in ``CALLBACKS`` at module level,
found by name. A module that cannot be imported stops the release; an
exception from a callback is logged and the release carries on.

The host is given the list as JSON in ``MURU_INSTRUMENTATION``, which it
takes out of its environment before importing anything, so neither the
application nor its command line carries the settings.
"""

from __future__ import annotations

import importlib
import json
import logging
import os
from collections.abc import Callable, Iterable, Iterator, MutableMapping
from contextlib import contextmanager
from dataclasses import dataclass, field, replace
from types import ModuleType
from typing import Any, ClassVar

from muru import logging as muru_logging

__all__ = [
    "CALLBACKS",
    "ENVIRONMENT",
    "Entry",
    "Instrument",
    "InstrumentationError",
    "Instruments",
    "Target",
]

ENVIRONMENT = "MURU_INSTRUMENTATION"

# The callbacks a module may define, in the order of a release's life.

CALLBACKS = (
    "configure",
    "load_started",
    "before_import",
    "after_import",
    "load_finished",
    "worker_started",
    "wrap_application",
    "worker_stopped",
)

# How a load ended, as load_finished is told (section 6.15): the host
# forks the workers from what it loaded; this process serves what it
# loaded (a worker with per-worker imports, or --single); the host starts
# again with per-worker imports; preload = true refused what the import
# check found; something failed; a signal stopped the process while it
# loaded; or muructl check is done.

FORK = "fork"
SERVE = "serve"
FALLBACK = "fallback"
REFUSED = "refused"
FAILED = "failed"
STOPPED = "stopped"
CHECK = "check"


class InstrumentationError(Exception):
    """An instrumentation module could not be imported, which stops the release."""

    def __init__(self, module: str, error: BaseException) -> None:
        super().__init__(
            f"cannot import instrumentation module {module}: {error.__class__.__name__}: {error}"
        )
        self.module = module


@dataclass(frozen=True)
class Entry:
    """One entry of the list: a module and the settings only it is given."""

    ENVIRONMENT: ClassVar[str] = ENVIRONMENT

    module: str
    settings: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def parse(cls, text: str) -> list[Entry]:
        """Read the list as the host is given it, a JSON array of entries."""

        entries = json.loads(text)

        if not isinstance(entries, list):
            raise ValueError("the instrumentation list is not a JSON array")

        parsed = []

        for entry in entries:
            if not isinstance(entry, dict) or not isinstance(entry.get("module"), str):
                raise ValueError(f"{entry!r} is not an instrumentation entry with a module")

            settings = entry.get("settings") or {}

            if not isinstance(settings, dict):
                raise ValueError(f"the settings of {entry['module']} are not a table")

            parsed.append(cls(entry["module"], settings))

        return parsed

    @classmethod
    def take(cls, environ: MutableMapping[str, str] = os.environ) -> list[Entry]:
        """Take the list out of the environment, so nothing started later sees it."""

        text = environ.pop(ENVIRONMENT, "")
        return cls.parse(text) if text else []

    @staticmethod
    def encode(entries: Iterable[Entry]) -> str:
        """Write the list as the host is given it, for a process started again."""

        return json.dumps(
            [{"module": entry.module, "settings": entry.settings} for entry in entries]
        )


@dataclass(frozen=True)
class Target:
    """What a callback is about: the hooks module, the entrypoint or a mount.

    `kind` is ``hooks``, ``entrypoint`` or ``mount``; `name` is a mount's
    name, empty for the others; `spec` is the module, or ``module:callable``
    for an entry point. `error` is what an import raised, given to
    after_import when it failed.
    """

    kind: str
    name: str
    spec: str
    error: BaseException | None = None


@dataclass
class Instrument:
    """One imported instrumentation module and the callbacks it defines."""

    name: str
    module: ModuleType
    functions: dict[str, Callable[..., Any]] = field(default_factory=dict)
    failures: list[tuple[str, BaseException]] = field(default_factory=list)
    """The callbacks that raised, with what they raised, for a check's report."""

    @classmethod
    def found(cls, name: str, module: ModuleType) -> Instrument:
        """Collect the callbacks a module defines, leaving out and logging one not callable."""

        functions: dict[str, Callable[..., Any]] = {}

        for callback in CALLBACKS:
            function = getattr(module, callback, None)

            if function is None:
                continue

            if not callable(function):
                logging.getLogger("muru").warning(
                    "instrumentation module %s: %s is not callable, so it is not called",
                    name,
                    callback,
                )
                continue

            functions[callback] = function

        return cls(name, module, functions)

    def call(self, callback: str, *args: Any) -> bool:
        """Run a callback if the module defines it, logging an exception rather than raising it.

        Returns whether it completed, or True when there is none.
        """

        function = self.functions.get(callback)

        if function is None:
            return True

        try:
            function(*args)
        except Exception as error:
            self.failures.append((callback, error))
            muru_logging._report(
                error, f"the {callback} callback of instrumentation module {self.name} failed"
            )
            return False

        return True


class Instruments:
    """A release's instrumentation modules, imported and configured in order."""

    def __init__(self, entries: Iterable[Entry] = ()) -> None:
        self.entries = list(entries)
        self.loaded: list[Instrument] = []
        self.outcome: str | None = None
        self.worker_started = False

    def load(
        self,
        context: Any = None,
        import_module: Callable[[str], ModuleType] = importlib.import_module,
    ) -> None:
        """Import each module, configure it and start it before the next is imported.

        A module that cannot be imported raises InstrumentationError,
        since that stops the release.
        """

        for entry in self.entries:
            try:
                module = import_module(entry.module)
            except Exception as error:
                raise InstrumentationError(entry.module, error) from error

            instrument = Instrument.found(entry.module, module)
            self.loaded.append(instrument)
            instrument.call("configure", dict(entry.settings))
            instrument.call("load_started", context)

    @contextmanager
    def importing(self, context: Any, target: Target) -> Iterator[None]:
        """Bracket the import of the hooks module, the entrypoint or a mount.

        before_import runs in listed order and after_import in reverse,
        given what the import raised when it failed; a host-phase factory
        called inside falls inside the pair.
        """

        self.call("before_import", context, target)

        try:
            yield
        except BaseException as error:
            self.call_reversed("after_import", context, replace(target, error=error))
            raise

        self.call_reversed("after_import", context, target)

    def finished(self, context: Any, outcome: str) -> None:
        """Run load_finished in reverse order, once, saying how the load ended."""

        if self.outcome is not None:
            return

        self.outcome = outcome
        self.call_reversed("load_finished", context, outcome)

    def call(self, callback: str, *args: Any) -> None:
        """Run a callback of every module that defines it, in listed order."""

        for instrument in self.loaded:
            instrument.call(callback, *args)

    def call_reversed(self, callback: str, *args: Any) -> None:
        """Run a callback of every module that defines it, in reverse order."""

        for instrument in reversed(self.loaded):
            instrument.call(callback, *args)

    def start_worker(self, context: Any) -> None:
        """Run worker_started in listed order, in a worker before its post_fork hook."""

        self.worker_started = True
        self.call("worker_started", context)

    def stop_worker(self, context: Any) -> None:
        """Run worker_stopped in reverse order, once, if worker_started ran."""

        if not self.worker_started:
            return

        self.worker_started = False
        self.call_reversed("worker_stopped", context)

    def wrap(self, context: Any, target: Target, application: Any, interface: str) -> Any:
        """Give an entry point's application to each module's wrap_application.

        The last listed wraps first, so the first listed is outermost. A
        callback that returns None, or raises, leaves the application as
        it was. A gRPC entry point's server is not a callable, so a
        different object returned for it is logged and not served.
        """

        for instrument in reversed(self.loaded):
            function = instrument.functions.get("wrap_application")

            if function is None:
                continue

            try:
                wrapped = function(context, target, application, interface)
            except Exception as error:
                muru_logging._report(
                    error,
                    f"the wrap_application callback of instrumentation module "
                    f"{instrument.name} failed, so {target.spec} is served as it was",
                )
                continue

            if wrapped is None or wrapped is application:
                continue

            if interface == "grpc":
                logging.getLogger("muru").warning(
                    "instrumentation module %s: wrap_application returned another object for "
                    "the gRPC entry point %s, which is served unchanged",
                    instrument.name,
                    target.spec,
                )
                continue

            application = wrapped

        return application

    def defined(self) -> dict[str, list[str]]:
        """Return each loaded module's callbacks, by module, for a check's report."""

        return {
            instrument.name: [
                callback for callback in CALLBACKS if callback in instrument.functions
            ]
            for instrument in self.loaded
        }
