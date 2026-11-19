"""The import check: what preloading left behind that fork would break.

After the host has imported the application (design specification section
6.2), the process should still be single-threaded with nothing connected:
a socket or pipe opened at import time would be shared by every worker, and
a thread or child process does not survive fork at all. ``ImportRecorder``
compares the process before and after preload, and watches each first
import on the way so that a finding can name the module that caused it.
"""

from __future__ import annotations

import ctypes
import ctypes.util
import importlib
import os
import socket
import stat
import sys
import threading
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from functools import partial
from importlib.machinery import ModuleSpec
from types import ModuleType, TracebackType
from typing import Any

from muru._imports import find_after

__all__ = ["Finding", "ImportRecorder", "child_pids", "latest_findings", "thread_count"]

# Compared through a name rather than sys.platform itself, so that mypy on
# one platform does not report the other platform's branch as unreachable.

_PLATFORM = sys.platform


@dataclass(frozen=True)
class Finding:
    """One thing the import left behind that cannot cross fork safely."""

    kind: str
    """socket, pipe, thread or child."""

    detail: str
    """What was found, for example "a connected socket (fd 7, to 127.0.0.1:5432)"."""

    module: str | None = None
    """The module whose import created it, when that could be determined."""

    instrumentation: str | None = None
    """The instrumentation module whose code started it, for a thread that runs its code."""

    def describe(self) -> str:
        """One sentence naming the finding and, when known, its module."""

        if self.instrumentation is not None:
            during = f" during the import of {self.module}" if self.module else ""
            return f"instrumentation module {self.instrumentation} left {self.detail}{during}"

        origin = f"the import of {self.module}" if self.module else "the import"
        return f"{origin} left {self.detail}"

    @property
    def advice(self) -> str:
        """What to change so that the application preloads cleanly."""

        if self.kind in ("socket", "pipe"):
            return (
                "close it in a pre_fork hook and reopen it in post_fork, "
                "or open it lazily on first use"
            )

        if self.instrumentation is not None:
            return (
                "the instrumentation module has to stop it before the workers are forked, "
                "or start it from worker_started instead"
            )

        return "start it from a post_fork hook instead"


# --- threads and children -------------------------------------------------

_PROC_PIDTASKINFO = 4
_TASKINFO_SIZE = 96
_THREADNUM_OFFSET = 84
_libproc_handle: ctypes.CDLL | bool | None = False


def _libproc() -> ctypes.CDLL | None:
    """Load the macOS libproc library once; None when it is unavailable."""

    global _libproc_handle

    if _libproc_handle is False:
        _libproc_handle = None
        path = ctypes.util.find_library("proc")

        if path is not None:
            try:
                library = ctypes.CDLL(path)
                library.proc_pidinfo.restype = ctypes.c_int
                library.proc_pidinfo.argtypes = [
                    ctypes.c_int,
                    ctypes.c_int,
                    ctypes.c_uint64,
                    ctypes.c_void_p,
                    ctypes.c_int,
                ]
                library.proc_listchildpids.restype = ctypes.c_int
                library.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
                _libproc_handle = library
            except (OSError, AttributeError):
                _libproc_handle = None

    return _libproc_handle if isinstance(_libproc_handle, ctypes.CDLL) else None


def thread_count() -> int:
    """Count the threads of this process, including ones Python cannot see.

    Native threads started by C libraries do not appear in the threading
    module, so the count comes from the operating system where possible:
    /proc on Linux and the task information on macOS.
    """

    if _PLATFORM == "linux":
        try:
            return len(os.listdir("/proc/self/task"))
        except OSError:
            pass
    elif _PLATFORM == "darwin":
        library = _libproc()

        if library is not None:
            buffer = ctypes.create_string_buffer(_TASKINFO_SIZE)
            size = library.proc_pidinfo(os.getpid(), _PROC_PIDTASKINFO, 0, buffer, _TASKINFO_SIZE)

            if size >= _TASKINFO_SIZE:
                raw = buffer.raw[_THREADNUM_OFFSET : _THREADNUM_OFFSET + 4]
                return int.from_bytes(raw, sys.byteorder, signed=True)

    return threading.active_count()


def child_pids() -> set[int]:
    """Return the pids of this process's live children."""

    pid = os.getpid()

    if _PLATFORM == "linux":
        children: set[int] = set()

        try:
            for task in os.listdir("/proc/self/task"):
                with open(f"/proc/self/task/{task}/children", encoding="ascii") as handle:
                    children.update(int(child) for child in handle.read().split())

            return children
        except OSError:
            pass

        try:
            for entry in os.listdir("/proc"):
                if not entry.isdigit():
                    continue
                with open(f"/proc/{entry}/stat", encoding="ascii", errors="replace") as handle:
                    fields = handle.read().rpartition(")")[2].split()
                if len(fields) > 1 and int(fields[1]) == pid:
                    children.add(int(entry))
        except OSError:
            pass

        return children

    if _PLATFORM == "darwin":
        library = _libproc()

        if library is not None:
            buffer = (ctypes.c_int * 4096)()
            count = library.proc_listchildpids(pid, buffer, ctypes.sizeof(buffer))
            if count >= 0:
                return {int(buffer[index]) for index in range(min(count, len(buffer)))}

    return set()


# --- descriptors ------------------------------------------------------------

Identity = tuple[int, int, int]


def _fd_directory() -> str:
    return "/proc/self/fd" if _PLATFORM == "linux" else "/dev/fd"


def open_fds() -> dict[int, Identity]:
    """Every open descriptor with its (mode, device, inode) identity."""

    found: dict[int, Identity] = {}

    try:
        entries = os.listdir(_fd_directory())
    except OSError:
        return found

    for entry in entries:
        if not entry.isdigit():
            continue
        fd = int(entry)

        try:
            info = os.fstat(fd)
        except OSError:
            # The descriptor used to list the directory, already closed.

            continue

        found[fd] = (stat.S_IFMT(info.st_mode), info.st_dev, info.st_ino)

    return found


def _format_peer(peer: Any) -> str:
    if isinstance(peer, tuple) and len(peer) >= 2:
        host, port = peer[0], peer[1]
        return f"[{host}]:{port}" if ":" in str(host) else f"{host}:{port}"

    if isinstance(peer, bytes):
        return repr(peer)
    return str(peer) or "an unnamed peer"


def _peer_name(fd: int) -> str | None:
    """Return the peer of a connected socket, or None when it is not connected."""

    try:
        duplicate = os.dup(fd)
    except OSError:
        return None

    try:
        sock = socket.socket(fileno=duplicate)
    except OSError:
        os.close(duplicate)
        return None

    try:
        peer = sock.getpeername()
    except OSError:
        return None
    finally:
        sock.close()

    return _format_peer(peer)


def _classify(fd: int, mode: int, children_appeared: bool) -> tuple[str, str] | None:
    """Kind and detail for a descriptor that workers must not inherit, else None.

    Regular files, unconnected sockets and pipes with no child process on
    the other end are safe to share and are not reported.
    """

    if stat.S_ISSOCK(mode):
        peer = _peer_name(fd)
        if peer is None:
            return None
        return "socket", f"a connected socket (fd {fd}, to {peer})"

    if stat.S_ISFIFO(mode) and children_appeared:
        return "pipe", f"a pipe (fd {fd}) to a child process"
    return None


# --- the recorder -----------------------------------------------------------


class ImportRecorder:
    """Watch what preload leaves behind, attributing it to the imports that did it.

    Use as a context manager around the import of the application and its
    on_load hook, then read ``findings()``. While active, a finder first
    on ``sys.meta_path`` wraps the loader of each module loaded for the
    first time, so that what the module's creation and execution leave
    (new descriptors and threads) is put down to it, however it was
    imported: an ``import`` statement, a from-import's submodules or
    ``importlib.import_module``. The check never loads a module itself
    and stays out of ``builtins.__import__``, which unwrapt's profile
    recorder hooks for the import report (issue 788).

    ``instrumentation`` names the release's instrumentation modules. A
    Python thread whose code is in one of them, or in the package one of
    them belongs to, is put down to that module rather than to the import
    that was running when it started, since a tool may start a thread
    lazily, on the first call it traces.
    """

    def __init__(self, instrumentation: Iterable[str] = ()) -> None:
        self._instrumentation = list(instrumentation)
        self._fds_before: dict[int, Identity] = {}
        self._threads_before = 0
        self._python_threads_before: set[threading.Thread] = set()
        self._children_before: set[int] = set()
        self._fd_owner: dict[tuple[int, Identity], str] = {}
        self._thread_owner: dict[threading.Thread, str] = {}
        self._thread_owners: list[tuple[str, int]] = []
        self._finder: _RecordingFinder | None = None

    def __enter__(self) -> ImportRecorder:
        self._fds_before = open_fds()
        self._threads_before = thread_count()
        self._python_threads_before = set(threading.enumerate())
        self._children_before = child_pids()
        self._finder = _RecordingFinder(self)
        sys.meta_path.insert(0, self._finder)
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        if self._finder is not None and self._finder in sys.meta_path:
            sys.meta_path.remove(self._finder)

        self._finder = None

    def import_module(self, name: str) -> ModuleType:
        """Import a module through importlib; each module it loads is attributed as it loads."""

        return importlib.import_module(name)

    def _snapshot(self) -> _Snapshot:
        # The process as a load begins, to compare with as it ends.

        return _Snapshot(open_fds(), thread_count(), set(threading.enumerate()))

    def _claim(self, module: str, before: _Snapshot) -> None:
        # What a load left, compared with as it began. A load nested in
        # another ends first, so it claims what it left before the outer
        # load can.
        #
        # A descriptor is the load's when its number is new or now refers
        # to something else, as when the load closed one and its number
        # was taken again; an owner is kept by identity, so a number
        # reused by a later load is that load's (issue 685). Listing the
        # descriptors opens one of its own, which is closed again by the
        # time it is examined and so is left out.

        for fd, identity in open_fds().items():
            if before.fds.get(fd) != identity:
                self._fd_owner.setdefault((fd, identity), module)

        # A Python thread is charged by name; what grew besides is native
        # threads, charged by count.

        started = set(threading.enumerate()) - before.python_threads

        for thread in started:
            self._thread_owner.setdefault(thread, module)

        grown = thread_count() - before.threads - len(started)

        if grown > 0:
            self._thread_owners.append((module, grown))

    def _instrumentation_of(self, code: str | None, module: str | None) -> str | None:
        """Return the instrumentation module that the module ``code`` is part of, if any.

        ``code`` is part of an instrumentation module when it is that
        module or one of its submodules, or when it is in the same
        top-level package as long as that is not the package of the
        import it was charged to: wrapture's writer thread runs code in
        wrapture.sinks, listed as wrapture.muru.
        """

        if code is None:
            return None

        for entry in self._instrumentation:
            if code == entry or code.startswith(f"{entry}."):
                return entry

        package = code.partition(".")[0]

        if module is not None and module.partition(".")[0] == package:
            return None

        for entry in self._instrumentation:
            if entry.partition(".")[0] == package:
                return entry

        return None

    def findings(self) -> list[Finding]:
        """Return what is still left behind now, compared with before the imports."""

        findings: list[Finding] = []
        children = child_pids() - self._children_before

        for fd, identity in sorted(open_fds().items()):
            if self._fds_before.get(fd) == identity:
                continue
            classified = _classify(fd, identity[0], bool(children))

            if classified is not None:
                kind, detail = classified
                findings.append(Finding(kind, detail, self._fd_owner.get((fd, identity))))

        # The Python threads still running are each named, with the module
        # whose code they run; what is left over is native threads.

        started = [
            thread
            for thread in threading.enumerate()
            if thread not in self._python_threads_before and thread.is_alive()
        ]

        for thread in started:
            module = self._thread_owner.get(thread)
            code = _thread_code(thread)
            instrumentation = self._instrumentation_of(code, module)
            detail = f"a thread running ({thread.name}" + (f", from {code})" if code else ")")
            findings.append(Finding("thread", detail, module, instrumentation))

        extra_threads = thread_count() - self._threads_before - len(started)

        if extra_threads > 0:
            attributed = 0

            for module, grown in self._thread_owners:
                count = min(grown, extra_threads - attributed)
                if count <= 0:
                    break
                attributed += count
                findings.append(Finding("thread", _threads(count), module))

            if attributed < extra_threads:
                findings.append(Finding("thread", _threads(extra_threads - attributed)))

        for pid in sorted(children):
            findings.append(Finding("child", f"a child process running (pid {pid})"))

        # Kept for the import report (section 6.17), which the release's
        # load_finished builds from them.

        _latest[:] = findings
        return findings


_latest: list[Finding] = []


def latest_findings() -> list[Finding]:
    """Return what the import check last found, for the import report."""

    return list(_latest)


def _threads(count: int) -> str:
    return "a thread running" if count == 1 else f"{count} threads running"


def _thread_code(thread: threading.Thread) -> str | None:
    """Return the module whose code a Python thread runs: its target's or its class's."""

    target = getattr(thread, "_target", None)

    if target is not None:
        module = getattr(target, "__module__", None)

        # A functools.partial has no module of its own, its function has.

        if module is None and isinstance(target, partial):
            module = getattr(target.func, "__module__", None)

    else:
        module = type(thread).__module__

    # The threading module's own code, as for Event.wait, says nothing.

    return module if isinstance(module, str) and module != "threading" else None


@dataclass(frozen=True)
class _Snapshot:
    # The descriptors and threads as a load began.

    fds: dict[int, Identity]
    threads: int
    python_threads: set[threading.Thread]


class _RecordingFinder:
    """The import check's finder, first on ``sys.meta_path`` while it is active."""

    def __init__(self, recorder: ImportRecorder) -> None:
        self._recorder = recorder
        self._local = threading.local()

    def find_spec(
        self, name: str, path: Sequence[str] | None, target: ModuleType | None = None
    ) -> ModuleSpec | None:
        """Find the module through the finders after this one, with its loader wrapped."""

        # A reload, or a search this thread is already making, is left to
        # the other finders as if this one were not there.

        if name in sys.modules:
            return None

        searching: set[str] | None = getattr(self._local, "searching", None)

        if searching is None:
            searching = self._local.searching = set()

        if name in searching:
            return None

        searching.add(name)

        try:
            spec = find_after(self, name, path, target)
        finally:
            searching.discard(name)

        # Only a loader that creates and executes a module can be wrapped;
        # what a loader of the old protocol leaves goes to the load it is
        # nested in.

        loader = spec.loader if spec is not None else None

        if (
            spec is None
            or loader is None
            or not hasattr(loader, "exec_module")
            or not hasattr(loader, "create_module")
        ):
            return spec

        spec.loader = _Recorded(spec, loader, self._recorder)  # type: ignore[assignment]
        return spec

    def invalidate_caches(self) -> None:
        """Nothing is cached here."""


class _Recorded:
    # A loader that has the recorder compare the process before and after
    # another's creation and execution of one module. The module is given
    # its own loader back before it executes, so its code, and anything
    # reading __loader__ or __spec__.loader later, sees the real one.

    __slots__ = ("_before", "_loader", "_recorder", "_spec")

    def __init__(self, spec: ModuleSpec, loader: Any, recorder: ImportRecorder) -> None:
        self._spec = spec
        self._loader = loader
        self._recorder = recorder
        self._before: _Snapshot | None = None

    def __getattr__(self, name: str) -> Any:
        return getattr(self._loader, name)

    def create_module(self, spec: ModuleSpec) -> ModuleType | None:
        """Take the baseline, and have the real loader create the module."""

        # An extension module runs its initialisation as it is created,
        # so the baseline comes first; a creation that fails ends the
        # load, and what it left is the module's still.

        self._before = self._recorder._snapshot()

        try:
            created: ModuleType | None = self._loader.create_module(spec)
        except BaseException:
            self._recorder._claim(self._spec.name, self._before)
            raise

        return created

    def exec_module(self, module: ModuleType) -> None:
        """Have the real loader execute the module, and claim what the load left."""

        loader = self._loader
        self._spec.loader = loader

        try:
            module.__loader__ = loader
        except AttributeError:
            pass

        before = self._before or self._recorder._snapshot()
        self._before = None

        try:
            loader.exec_module(module)
        finally:
            self._recorder._claim(self._spec.name, before)
