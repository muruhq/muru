"""The signals Muru keeps for itself in the host and its workers (issue 673).

Each process takes its signals as it starts, before anything of the
instrumentation modules or the application is imported, so a signal that
arrives while the application is loading is handled rather than killing
the process with the default action:

- **The host**, at the top of ``main``: SIGTERM and SIGINT raise ``Stopped``,
  which unwinds the load, runs ``load_finished`` with ``stopped`` and ends
  the host cleanly; SIGUSR1 and SIGUSR2, which would otherwise end it, are
  taken and do nothing until the host serves. ``Host.run`` then puts its
  own handlers in their place.

- **A worker**, right after the fork: SIGTERM and SIGINT raise ``Stopped``
  while the worker starts (its imports, ``post_fork``, a factory,
  ``wrap_application``, lifespan startup and ``on_ready``), which unwinds
  it through its lifespan shutdown, ``on_exit``, the ``atexit`` callbacks
  and ``worker_stopped``. SIGUSR1 and SIGUSR2 are taken. The native
  server notes every one of them as well, from the same moment, chained
  over these handlers, and once it serves with its own handling it acts on
  any that arrived before. From ``started()`` on a stop no longer raises
  but is left to the server, so it drains as any stop does. A worker's
  handlers are never replaced, since that would replace the native ones
  chained over them.

Python code cannot take these signals: ``signal.signal`` is replaced in
each process with a function that refuses a signal Muru keeps there, with
a warning naming the signal and where the call came from, and returns the
handler in force, so the caller carries on. Other signals pass through,
and there is no setting to turn this off. ``loop.add_signal_handler()``
calls ``signal.signal()`` and is covered. ``faulthandler.register()`` and
C extensions that call ``sigaction`` themselves are not.

Python runs signal handlers on the main thread, and only the main thread
can install one, so code on Muru's event loop thread or a request thread
cannot install one at all.
"""

from __future__ import annotations

import logging
import signal
import sys
from collections.abc import Callable
from types import FrameType
from typing import Any

__all__ = [
    "HOST",
    "WORKER",
    "Stopped",
    "hold",
    "install",
    "keep",
    "release",
    "start_host",
    "start_worker",
    "started",
]

HOST = frozenset({signal.SIGTERM, signal.SIGINT, signal.SIGCHLD, signal.SIGUSR1, signal.SIGUSR2})
"""The signals the host keeps."""

WORKER = frozenset({signal.SIGTERM, signal.SIGINT, signal.SIGUSR1, signal.SIGUSR2, signal.SIGQUIT})
"""The signals a worker keeps."""

# The signals held across a fork, so a worker gets them only once its own
# handlers are in place.

_HELD = frozenset({signal.SIGTERM, signal.SIGINT, signal.SIGUSR1, signal.SIGUSR2})

# The real signal.signal, the signals refused in this process, and whether
# a stop raises, which it does while the process is starting.

_original: Callable[..., Any] = signal.signal
_kept: frozenset[int] = frozenset()
_raising = False


class Stopped(BaseException):
    """A stop asked for by a signal while the process was starting.

    A BaseException, as KeyboardInterrupt is, so the handling of an
    application's or a hook's own errors does not swallow it.
    """

    def __init__(self, signum: int) -> None:
        super().__init__(signal.Signals(signum).name)
        self.signum = signum

    @property
    def name(self) -> str:
        """The signal's name, as SIGTERM."""

        return signal.Signals(self.signum).name


def start_host() -> None:
    """Take the host's signals as it starts, before anything is imported."""

    global _raising

    _raising = True
    install(signal.SIGTERM, _stop)
    install(signal.SIGINT, _stop)
    install(signal.SIGUSR1, _taken)
    install(signal.SIGUSR2, _taken)
    keep(HOST)


def hold() -> None:
    """Hold the stop, reopen and reclaim signals back, just before a fork.

    The worker forked meanwhile has them held too, until ``start_worker``
    has its own handlers in place, so none reaches the host's handlers in
    the worker; ``release`` lets the host have them again.
    """

    signal.pthread_sigmask(signal.SIG_BLOCK, _HELD)


def release() -> None:
    """Let the held signals through again, delivering any that arrived."""

    signal.pthread_sigmask(signal.SIG_UNBLOCK, _HELD)


def start_worker() -> None:
    """Take a worker's signals right after the fork, before it starts.

    The host's handlers and wake-up pipe are the host's. The native server
    notes the signals too, from here, so it acts on any that arrive before
    it serves. The signals the host held across the fork are let through
    last, so one that arrived meanwhile reaches these handlers.
    """

    global _raising

    from muru import _native

    signal.set_wakeup_fd(-1)
    _raising = True
    install(signal.SIGTERM, _stop)
    install(signal.SIGINT, _stop)
    install(signal.SIGUSR1, _taken)
    install(signal.SIGUSR2, _taken)
    install(signal.SIGCHLD, signal.SIG_DFL)
    _native.claim_signals()
    keep(WORKER)
    release()


def started() -> None:
    """Mark the process started, so a stop no longer raises.

    From here a stop is the server's own handling, or the host's.
    """

    global _raising

    _raising = False


def keep(signals: frozenset[signal.Signals]) -> None:
    """Refuse other code's handlers for these signals in this process from now on."""

    global _kept

    _kept = frozenset(int(signum) for signum in signals)
    signal.signal = _guarded  # type: ignore[assignment]


def install(signum: int, handler: Any) -> Any:
    """Install one of Muru's own handlers, past the guard."""

    return _original(signum, handler)


def _stop(signum: int, frame: FrameType | None) -> None:
    # SIGTERM and SIGINT: unwind the start while there is one to unwind.

    if _raising:
        raise Stopped(signum)


def _taken(signum: int, frame: FrameType | None) -> None:
    # SIGUSR1 and SIGUSR2 before the process serves: taken, so the default
    # action does not end it. A worker's server acts on them once it
    # serves; the host's handlers replace these.

    pass


def _guarded(signum: int, handler: Any) -> Any:
    # signal.signal as other code sees it: a signal Muru keeps is refused
    # and the handler in force returned, as signal.signal returns the one
    # it replaced.

    if int(signum) not in _kept:
        return _original(signum, handler)

    name = signal.Signals(signum).name
    logging.getLogger("muru").warning(
        "signal.signal(%s) from %s was ignored: Muru handles %s in this process",
        name,
        _caller(sys._getframe(1)),
        name,
    )

    return signal.getsignal(signum)


def _caller(frame: FrameType | None) -> str:
    # Where the call came from: the first frame outside asyncio, whose
    # add_signal_handler calls signal.signal on its caller's behalf.

    while frame is not None and frame.f_globals.get("__name__", "").startswith("asyncio"):
        frame = frame.f_back

    if frame is None:
        return "an unknown place"

    return f"{frame.f_globals.get('__name__', '?')}:{frame.f_lineno}"
