"""The modules a worker loads for the first time after the fork (issue 738).

Under preload a module the host did not import costs every worker
separately, and adds its time to whichever request needs it first:
a framework importing a view module as it first resolves a URL, an
``import`` inside a function, code only a request reaches. In each
worker the runtime puts a finder first on ``sys.meta_path`` once the
worker has loaded what it serves, just before the instrumentation
modules' ``worker_started``. The finder sees only modules not yet in
``sys.modules``, so it costs nothing once the worker has settled. It
hands the search on to the finders after it and wraps the loader they
return, which times the module's creation and execution, however it was
imported: an ``import`` statement, ``importlib.import_module``, a direct
``__import__`` call or a plugin loader.

Each load goes to the record of the request it ran under, through the
request's id (section 6.9), when the edge asked for one, with its start,
length, nesting and the line that triggered it; a load outside any
request, or under one not recorded, is only counted for the worker. A
request's record keeps at most ``LOADS_KEPT`` loads, counting the rest.
The time of the outermost loads is also told to the runtime as each
ends, for the response head's ``Server-Timing``. A load that resolved an
import deferred by ``lazy_imports`` (section 6.17) is marked as deferred,
as unwrapt tells ``muru._lazy_imports`` of it.

A request can also wait on a load another thread has under way: the
import system holds the module's lock while it loads, and a thread that
asks for the module meanwhile blocks on the lock and then finds it in
``sys.modules``, never reaching the finder (issue 787). For that the
runtime wraps ``importlib._bootstrap._lock_unlock_module``, which the
import system calls only when the module asked for is still being
initialised, and keeps each wait of at least ``WAIT_MIN_MS`` in the
waiting request's record, naming the module and the request loading it,
its time told to the runtime apart from the loads' time.
"""

from __future__ import annotations

import importlib._bootstrap
import os
import sys
import threading
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from importlib.machinery import ModuleSpec
from types import FrameType, ModuleType
from typing import Any

from muru._context import request_id

__all__ = [
    "LOADS_KEPT",
    "WAITS_KEPT",
    "WAIT_MIN_MS",
    "WORKER_LOADS_KEPT",
    "find_after",
    "install",
    "loaded",
    "mark_deferred",
    "uninstall",
    "worker_loads",
]

LOADS_KEPT = 200
"""The most loads one request's record keeps one by one, the first to start.

The rest are counted.
"""

WORKER_LOADS_KEPT = 2000
"""The most loads a worker keeps for ``muructl debug imports``, the first to start.

The rest are counted.
"""

WAITS_KEPT = 50
"""The most waits on another thread's load one request's record keeps; the rest are dropped."""

WAIT_MIN_MS = 1.0
"""The least a wait on another thread's load must take to be kept."""

# The import system's own frames, and this module's, which are passed
# over to find the line that triggered a load, and the packages whose
# frames stand between an import statement and the import system when
# they wrap builtins.__import__: unwrapt's, with lazy_imports on (section
# 6.17), and wrapt's beneath it.

_PASSED_OVER = (
    "<frozen importlib._bootstrap>",
    "<frozen importlib._bootstrap_external>",
    __file__,
    os.path.join(os.path.dirname(os.__file__), "importlib", "__init__.py"),
)
_PASSED_OVER_PACKAGES = ("unwrapt", "wrapt")


@dataclass
class _Loads:
    # One recorded request's loads, on its clock, and its runtime's tally
    # of the outermost loads' time for the response head; and its waits
    # on other threads' loads, each its start, length, module and the
    # request loading it, with their own tally.

    started: float
    tally: Callable[[float], None] | None
    wait_tally: Callable[[float], None] | None = None
    lock: threading.Lock = field(default_factory=threading.Lock)
    kept: list[list[Any]] = field(default_factory=list)
    waits: list[tuple[float, float, str, str]] = field(default_factory=list)
    deferred: set[str] = field(default_factory=set)
    count: int = 0
    outermost_ms: float = 0.0
    taken: bool = False


_requests: dict[str, _Loads] = {}

# The modules this worker has loaded since the finder went in, and those
# of them that ran outside any request.

_counts = [0, 0]
_counting = threading.Lock()

# Each module this worker has loaded since the finder went in, for the
# import report's section on the workers (issue 741): the module, the
# request it ran under, its milliseconds, its depth, whether it resolved
# a deferred import, and when it ended, the first WORKER_LOADS_KEPT to
# start; one still under way has no end yet.

_worker: dict[str, list[Any]] = {}

# The loads under way, each module's with the request it runs under
# (empty outside one) and its thread, for a wait on it to name.

_loading: dict[str, tuple[str, int]] = {}

# The import system's function a thread waits in for another's load,
# while it is wrapped.

_lock_unlock: Callable[[str], None] | None = None

# Each thread's depth of loads under way, and the names it is searching
# for, so a finder after this one that imports while it searches does not
# bring the same search back here.

_local = threading.local()

_finder: _Finder | None = None

# The names the finders after this one did not find. A module that is not
# there never reaches sys.modules, so code that tries an optional import
# on every call would otherwise be searched for twice each time, here and
# by the import system after this finder returns nothing; such a name is
# left to the import system alone, which finds it if it has since
# appeared, untimed.

_MISSING_KEPT = 1024
_missing: set[str] = set()

# The runtime's counter of the worker's loads, for its status reports,
# when the extension is there.

_note_loaded: Callable[[float], None] | None = None


def loaded() -> tuple[int, int]:
    """Return the modules this worker has loaded since the fork, and those outside a request."""

    return _counts[0], _counts[1]


def install() -> None:
    """Put the finder first on ``sys.meta_path``, once per process."""

    global _finder, _note_loaded

    if _finder is not None:
        return

    try:
        from muru import _native

        _note_loaded = _native.module_loaded
    except (ImportError, AttributeError):
        _note_loaded = None

    _finder = _Finder()
    sys.meta_path.insert(0, _finder)
    _wrap_lock_unlock()


def uninstall() -> None:
    """Take the finder off ``sys.meta_path``, for tests."""

    global _finder

    if _finder is not None and _finder in sys.meta_path:
        sys.meta_path.remove(_finder)

    _finder = None
    _counts[:] = [0, 0]
    _requests.clear()
    _missing.clear()
    _worker.clear()
    _loading.clear()
    _unwrap_lock_unlock()


def _wrap_lock_unlock() -> None:
    # The import system calls _lock_unlock_module by its name in
    # importlib._bootstrap (the C import code included), and only for a
    # module still being initialised, so the common path pays nothing.

    global _lock_unlock

    original = getattr(importlib._bootstrap, "_lock_unlock_module", None)

    if original is None or _lock_unlock is not None:
        return

    _lock_unlock = original
    importlib._bootstrap._lock_unlock_module = _waiting  # type: ignore[attr-defined]


def _unwrap_lock_unlock() -> None:
    global _lock_unlock

    if _lock_unlock is not None:
        importlib._bootstrap._lock_unlock_module = _lock_unlock  # type: ignore[attr-defined]

    _lock_unlock = None


def _waiting(name: str) -> None:
    # The import system's wait for a module another thread is loading,
    # timed. A wait on the thread's own load, as in a circular import,
    # does not block and is left alone.

    original = _lock_unlock

    if original is None:
        return

    loading = _loading.get(name)

    if loading is not None and loading[1] == threading.get_ident():
        original(name)
        return

    started = time.perf_counter()

    try:
        original(name)
    finally:
        _waited(name, loading[0] if loading is not None else "", started, time.perf_counter())


def _waited(module: str, loader: str, started: float, ended: float) -> None:
    # A wait that ended, kept for the waiting request when that is
    # recorded. One inside a load of the request's own is part of that
    # load's time already.

    ms = (ended - started) * 1000.0
    current = request_id()
    depth: int = getattr(_local, "depth", 0)

    if ms < WAIT_MIN_MS or current is None or depth > 0:
        return

    kept = _requests.get(current)

    if kept is None:
        return

    with kept.lock:
        if kept.taken:
            return

        if len(kept.waits) < WAITS_KEPT:
            kept.waits.append(((started - kept.started) * 1000.0, ms, module, loader))

        tally = kept.wait_tally

    if tally is not None:
        tally(ms)


def worker_loads() -> dict[str, Any]:
    """Return the modules this worker has loaded since the fork, for ``muructl debug imports``.

    Each is its module, the request it ran under (empty outside one),
    its milliseconds, its depth beneath other loads, whether it resolved
    a deferred import, and when it ended in Unix milliseconds, the first
    ``WORKER_LOADS_KEPT`` to start, leaving out one still under way; with
    how many loads there were in all.
    """

    with _counting:
        loads = [
            {
                "module": module,
                "request": request,
                "ms": round(ms, 3),
                "depth": depth,
                "deferred": deferred,
                "at_unix_ms": at,
            }
            for module, (request, ms, depth, deferred, at) in _worker.items()
            if at
        ]
        count = _counts[0]

    return {"loads": loads, "loaded": count}


def mark_deferred(module: str) -> None:
    """Mark the current request's load of `module` as the resolution of a deferred import.

    Called once the module is imported, in the thread whose use of the
    deferred import loaded it, so the load is in the record already.
    """

    with _counting:
        load = _worker.get(module)

        if load is not None:
            load[3] = True

    current = request_id()
    kept = _requests.get(current) if current is not None else None

    if kept is None:
        return

    with kept.lock:
        kept.deferred.add(module)


def _trigger() -> str:
    # The line that asked for the load: the first frame that is neither
    # the import system's nor this module's, as its file and line.

    frame: FrameType | None = sys._getframe(2)

    while frame is not None and (
        frame.f_code.co_filename in _PASSED_OVER
        or str(frame.f_globals.get("__name__", "")).partition(".")[0] in _PASSED_OVER_PACKAGES
    ):
        frame = frame.f_back

    if frame is None:
        return ""

    return f"{frame.f_code.co_filename}:{frame.f_lineno}"


def find_after(
    finder: object, name: str, path: Sequence[str] | None, target: ModuleType | None
) -> ModuleSpec | None:
    """Find a module through the finders after `finder` on ``sys.meta_path``.

    The finders are asked as the import system asks them, in order, and
    the first spec found is returned; None when none of them finds it.
    The import check's finder (``muru.preload``) uses this too.
    """

    found = False

    for other in list(sys.meta_path):
        if other is finder:
            found = True
            continue

        if not found:
            continue

        find = getattr(other, "find_spec", None)

        if find is None:
            continue

        spec: ModuleSpec | None = find(name, path, target)

        if spec is not None:
            return spec

    return None


class _Finder:
    """The finder first on ``sys.meta_path``, which times the loads of the others."""

    def find_spec(
        self, name: str, path: Sequence[str] | None, target: ModuleType | None = None
    ) -> ModuleSpec | None:
        """Find the module through the finders after this one, with its loader timed."""

        # A reload, or a search this thread is already making, is left to
        # the other finders as if this one were not there.

        if name in sys.modules:
            return None

        searching: set[str] | None = getattr(_local, "searching", None)

        if searching is None:
            searching = _local.searching = set()

        if name in searching or name in _missing:
            return None

        searching.add(name)

        try:
            spec = self._search(name, path, target)
        finally:
            searching.discard(name)

        if spec is None:
            if len(_missing) < _MISSING_KEPT:
                _missing.add(name)

            return None

        # Only a loader that creates and executes a module can be timed;
        # one of the old protocol, or a namespace package, is left alone.

        loader = spec.loader

        if (
            loader is None
            or not hasattr(loader, "exec_module")
            or not hasattr(loader, "create_module")
        ):
            return spec

        current = request_id()
        kept = _requests.get(current) if current is not None and _requests else None
        spec.loader = _Timed(spec, loader, kept, _trigger() if kept is not None else "")  # type: ignore[assignment]
        return spec

    def _search(
        self, name: str, path: Sequence[str] | None, target: ModuleType | None
    ) -> ModuleSpec | None:
        return find_after(self, name, path, target)

    def invalidate_caches(self) -> None:
        """Nothing is cached here."""


class _Timed:
    # A loader that times another's creation and execution of one module.
    # The module is given its own loader back before it executes, so its
    # code, and anything reading __loader__ or __spec__.loader later, sees
    # the real one.
    #
    # A load takes its place among those kept as it starts, the request's
    # and the worker's, and fills in its length as it ends. An outer load
    # starts before the loads nested in it and ends after them, so keeping
    # the first to start keeps every kept load's ancestors (issue 782).

    __slots__ = (
        "_depth",
        "_entry",
        "_kept",
        "_loader",
        "_spec",
        "_started",
        "_trigger",
        "_worker_entry",
    )

    def __init__(self, spec: ModuleSpec, loader: Any, kept: _Loads | None, trigger: str) -> None:
        self._spec = spec
        self._loader = loader
        self._kept = kept
        self._trigger = trigger
        self._started = 0.0
        self._depth = 0
        self._entry: list[Any] | None = None
        self._worker_entry: list[Any] | None = None

    def __getattr__(self, name: str) -> Any:
        return getattr(self._loader, name)

    def create_module(self, spec: ModuleSpec) -> ModuleType | None:
        """Start the clock, and have the real loader create the module."""

        self._begin()

        try:
            created: ModuleType | None = self._loader.create_module(spec)
        except BaseException:
            self._note(time.perf_counter())
            raise

        return created

    def exec_module(self, module: ModuleType) -> None:
        """Have the real loader execute the module, and note the load."""

        loader = self._loader
        self._spec.loader = loader

        try:
            module.__loader__ = loader
        except AttributeError:
            pass

        # A loader whose create_module was not called through this one
        # starts the clock now. The loads already executing on this
        # thread are those this one is nested in.

        if not self._started:
            self._begin()

        _local.depth = self._depth + 1

        try:
            loader.exec_module(module)
        finally:
            _local.depth = self._depth
            self._note(time.perf_counter())

    def _begin(self) -> None:
        # A load that starts: its place among those kept, while there is
        # room, for its length to be filled in as it ends.

        self._started = time.perf_counter()
        self._depth = depth = getattr(_local, "depth", 0)
        current = request_id()
        name = self._spec.name

        _loading[name] = (current or "", threading.get_ident())

        with _counting:
            if len(_worker) < WORKER_LOADS_KEPT and name not in _worker:
                self._worker_entry = _worker[name] = [current or "", 0.0, depth, False, 0]

        kept = self._kept

        if kept is None:
            return

        with kept.lock:
            if not kept.taken and len(kept.kept) < LOADS_KEPT:
                at = (self._started - kept.started) * 1000.0
                self._entry = [at, None, name, self._trigger, depth]
                kept.kept.append(self._entry)

    def _note(self, ended: float) -> None:
        # A load that ended: counted for the worker, and kept for its
        # request when that is recorded.

        current = request_id()
        ms = (ended - self._started) * 1000.0
        depth = self._depth
        _loading.pop(self._spec.name, None)

        with _counting:
            _counts[0] += 1
            _counts[1] += current is None

            if self._worker_entry is not None:
                self._worker_entry[1] = ms
                self._worker_entry[4] = int(time.time() * 1000)

        # The runtime counts every load and times the outermost, which
        # hold the nested ones, for the worker's metrics (issue 753).

        if _note_loaded is not None:
            _note_loaded(ms if depth == 0 else 0.0)

        kept = self._kept

        if kept is None:
            return

        tally = None

        with kept.lock:
            if kept.taken:
                return

            kept.count += 1

            if self._entry is not None:
                self._entry[1] = ms

            if depth == 0:
                kept.outermost_ms += ms
                tally = kept.tally

        if tally is not None:
            tally(ms)


# The worker's side: called by the runtime as it takes a request it
# records and as it builds the worker's part of the record.


def _start(
    request: str,
    tally: Callable[[float], None] | None = None,
    since: float = 0.0,
    wait_tally: Callable[[float], None] | None = None,
) -> None:
    """Start keeping a request's loads, on its clock from `since` seconds ago.

    `tally` is told each outermost load's milliseconds as it ends, and
    `wait_tally` each wait on another thread's load.
    """

    if _finder is None:
        return

    _requests[request] = _Loads(time.perf_counter() - since, tally, wait_tally)


def _take(
    request: str,
) -> tuple[
    list[tuple[float, float, str, str, int, bool]],
    int,
    float,
    list[tuple[float, float, str, str]],
]:
    """Stop keeping a request's loads, and return them for the runtime to send.

    The loads kept, each its start on the request's clock, its length in
    milliseconds, the module, the line that triggered it, its depth
    beneath other loads and whether it resolved a deferred import, in the
    order they began; how many loads there were in all; the outermost
    loads' milliseconds summed; and the waits on other threads' loads,
    each its start, length, module and the request loading it, in order.
    """

    kept = _requests.pop(request, None)

    if kept is None:
        return [], 0, 0.0, []

    # The loads were kept in the order they began; one still under way,
    # on a thread the request started, has no length and is left out.

    with kept.lock:
        kept.taken = True
        loads = [tuple(load) for load in kept.kept if load[1] is not None]
        deferred = set(kept.deferred)
        count, outermost_ms = kept.count, kept.outermost_ms
        waits = list(kept.waits)

    return (
        [
            (at, ms, name, _short(trigger), depth, name in deferred)
            for at, ms, name, trigger, depth in loads
        ],
        count,
        outermost_ms,
        waits,
    )


def _short(trigger: str) -> str:
    # A trigger's file relative to the entry of sys.path it is under, the
    # longest that holds it, as it would be imported from.

    longest = ""

    for entry in sys.path:
        if (
            entry
            and len(entry) > len(longest)
            and trigger.startswith(entry.rstrip(os.sep) + os.sep)
        ):
            longest = entry.rstrip(os.sep) + os.sep

    return trigger[len(longest) :]
