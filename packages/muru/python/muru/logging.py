"""Python logging defaults (design specification section 11.4).

Before the application is imported, the host sets the root logger's level
and replaces ``logging.lastResort`` with a handler that writes to standard
error in the configured format, so a logger with no handler of its own is
shown the platform way. ``basicConfig`` and ``dictConfig`` are untouched:
the last-resort handler is used only when no handler exists in a logger's
hierarchy, so an application that configures logging behaves exactly as
it does without Muru. ``default_config()`` is the same defaults as a
dictConfig dictionary, for applications that want to adopt them
explicitly.

Every record carries the request it was logged in and where it was
logged, as ``muru_request_id``, ``muru_app``, ``muru_release``,
``muru_group`` and ``muru_worker``, so an application's own format can
use them (``%(muru_request_id)s``). The prefix keeps them apart from what
an application passes in ``extra``, which ``logging`` refuses to let
overwrite an attribute the record already has.
"""

from __future__ import annotations

import builtins
import datetime
import importlib
import json
import logging
import os
import re
import sys
import traceback
import warnings
from collections.abc import Callable
from typing import Any

from muru._context import context, request_id

__all__ = ["STANDARD_FORMAT", "JsonFormatter", "default_config", "formatter"]

STANDARD_FORMAT = "%(levelname)s %(name)s: %(message)s"
"""The ``standard`` format: the level, the logger and the message."""


class JsonFormatter(logging.Formatter):
    """One JSON object per record, with the Muru tags of the process."""

    def format(self, record: logging.LogRecord) -> str:
        """Format the record as one JSON object."""

        where = context()
        line: dict[str, Any] = {
            "time": datetime.datetime.fromtimestamp(record.created, datetime.UTC).isoformat(
                timespec="milliseconds"
            )[:-6]
            + "Z",
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
            "app": where.app,
            "release": where.release,
            "group": where.group,
            "worker": where.worker_id,
        }

        current = getattr(record, "muru_request_id", None) or request_id()

        if current is not None:
            line["request_id"] = current

        if record.exc_info and record.exc_info[0] is not None:
            line["exception"] = self.formatException(record.exc_info)

        if record.stack_info:
            line["stack"] = self.formatStack(record.stack_info)

        return json.dumps(line, default=str)


def formatter(format: str = "standard") -> logging.Formatter:
    """Return the formatter for a format name.

    ``standard`` and ``json`` are the platform formats; anything else is
    a ``logging`` format string.
    """

    if format == "standard":
        return logging.Formatter(STANDARD_FORMAT)

    if format == "json":
        return JsonFormatter()

    return logging.Formatter(format)


class _FallbackHandler(logging.StreamHandler):  # type: ignore[type-arg]
    """A handler that writes only when nothing else in the hierarchy would.

    ``logging.captureWarnings`` gives the ``py.warnings`` logger a
    ``NullHandler``, which stops the last-resort handler from ever seeing
    a warning; this handler stands in for it there, and steps aside once
    the application configures handlers of its own.
    """

    def emit(self, record: logging.LogRecord) -> None:
        """Write the record unless another handler would."""

        logger: logging.Logger | None = logging.getLogger(record.name)

        while logger is not None:
            for handler in logger.handlers:
                if handler is not self and not isinstance(handler, logging.NullHandler):
                    return

            if not logger.propagate:
                break

            logger = logger.parent

        super().emit(record)


CAPTURE_LIMIT = 200
"""The most log records a development run's request record keeps."""

# Section 11.7: the log records of the requests a development run keeps
# records of, by request id, from the worker taking a request until it
# sends the worker's part of its record. Each entry is the records kept,
# as (created, level, logger, message), and how many were past the limit.

_captures: dict[str, tuple[list[tuple[float, str, str, str]], list[int]]] = {}


def _capture_start(request: str) -> None:
    """Start keeping the log records of a request, for its record."""

    _captures[request] = ([], [0])


def _capture_take(request: str) -> tuple[list[tuple[float, str, str, str]], int]:
    """Stop keeping a request's log records, and return them and how many were dropped."""

    kept, dropped = _captures.pop(request, ([], [0]))
    return kept, dropped[0]


def _capture(record: logging.LogRecord, request: str) -> None:
    """Keep a record for its request's record, with its traceback, up to the limit."""

    captured = _captures.get(request)

    if captured is None:
        return

    kept, dropped = captured

    if len(kept) >= CAPTURE_LIMIT:
        dropped[0] += 1
        return

    try:
        message = record.getMessage()
    except Exception:
        message = str(record.msg)

    if record.exc_info:
        message = f"{message}\n{''.join(traceback.format_exception(*record.exc_info)).rstrip()}"

    kept.append((record.created, record.levelname, record.name, message))


def _tagging(factory: Callable[..., logging.LogRecord]) -> Callable[..., logging.LogRecord]:
    """Wrap a record factory so every record carries the Muru tags."""

    def tagged(*args: Any, **kwargs: Any) -> logging.LogRecord:
        record = factory(*args, **kwargs)
        where = context()
        current = request_id()
        record.muru_request_id = current
        record.muru_app = where.app
        record.muru_release = where.release
        record.muru_group = where.group
        record.muru_worker = where.worker_id

        if _captures and current is not None and current in _captures:
            _capture(record, current)

        return record

    tagged.muru_tagging = True  # type: ignore[attr-defined]
    return tagged


def _shown(logger: logging.Logger, level: int) -> bool:
    """Say whether a record at `level` on `logger` would reach a handler that writes it.

    As ``Logger.callHandlers`` decides: the logger must be enabled for the
    level, and some handler on the way up its hierarchy, other than a
    ``NullHandler``, must take the level; with no handler at all the
    last-resort handler does. A filter is not consulted, so a record a
    filter drops counts as shown.
    """

    if logger.disabled or not logger.isEnabledFor(level):
        return False

    current: logging.Logger | None = logger
    found = False

    while current is not None:
        for handler in current.handlers:
            found = True

            if not isinstance(handler, logging.NullHandler) and level >= handler.level:
                return True

        if not current.propagate:
            break

        current = current.parent

    resort = logging.lastResort
    return not found and resort is not None and level >= resort.level


def _report(error: BaseException, message: str) -> None:
    """Report an exception the application or a hook did not catch.

    It is logged on the ``muru`` logger at ERROR with its traceback, so it
    reaches the application's own handlers when it has configured some,
    Muru's format when it has not, and carries the request's id. When the
    application's configuration would show nothing at ERROR there (the
    logger disabled by a ``dictConfig``, a level above ERROR, or only
    handlers that take no ERROR), the traceback is written to standard
    error instead, so it is never lost to a logging setting.
    """

    logger = logging.getLogger("muru")

    if _shown(logger, logging.ERROR):
        logger.error(message, exc_info=(type(error), error, error.__traceback__))
        return

    try:
        sys.stderr.write(f"muru: {message}\n" + "".join(traceback.format_exception(error)))
        sys.stderr.flush()
    except (OSError, ValueError):
        pass


def default_config() -> dict[str, Any]:
    """Return the platform defaults as a ``logging.config.dictConfig`` dictionary.

    The root level and the format come from MURU_LOG_LEVEL and
    MURU_LOG_FORMAT, which the host sets from the release's settings;
    outside Muru they are WARNING and ``standard``.
    """

    return {
        "version": 1,
        "disable_existing_loggers": False,
        "formatters": {
            "muru": {
                "()": "muru.logging.formatter",
                "format": os.environ.get("MURU_LOG_FORMAT", "standard"),
            }
        },
        "handlers": {
            "muru": {
                "class": "logging.StreamHandler",
                "stream": "ext://sys.stderr",
                "formatter": "muru",
            }
        },
        "root": {
            "level": os.environ.get("MURU_LOG_LEVEL", "WARNING"),
            "handlers": ["muru"],
        },
    }


_installed: dict[str, int] = {}
"""The levels `_install` set, by logger name, the root's under "", so a
later look can tell Muru's defaults from what changed them since."""

_refused_filters: list[str] = []
"""The warnings filters `_install` could not apply, each with why."""


def _filter_warnings(filters: list[str]) -> None:
    """Apply warnings filters in the syntax of Python's -W option, in order.

    Each is ``action:message:category:module:lineno``, read as -W reads
    it: the message and module match literally, from the start, and a
    later filter takes precedence. A filter whose category cannot be
    imported is not applied and is noted in ``_refused_filters``; the
    rest of its syntax was checked when the configuration was read.
    """

    actions = ("default", "always", "all", "ignore", "module", "once", "error")

    for text in filters:
        fields = [field.strip() for field in text.split(":")]
        action, message, category_name, module, lineno = (fields + [""] * 5)[:5]
        action = next((known for known in actions if known.startswith(action)), "default")
        action = "always" if action == "all" else action

        try:
            category = _warning_category(category_name)
        except (ImportError, AttributeError, TypeError) as error:
            _refused_filters.append(f"{text!r} ({error})")
            continue

        warnings.filterwarnings(
            action,  # type: ignore[arg-type]
            re.escape(message),
            category,
            re.escape(module) + r"\Z" if module else "",
            int(lineno or 0),
        )


def _warning_category(name: str) -> type[Warning]:
    """Return the warning category a filter names: a builtin or a dotted path."""

    if not name:
        return Warning

    if "." in name:
        module, _, attribute = name.rpartition(".")
        found = getattr(importlib.import_module(module), attribute)
    else:
        found = getattr(builtins, name)

    if not (isinstance(found, type) and issubclass(found, Warning)):
        raise TypeError(f"{name} is not a Warning subclass")

    return found


def _install(
    level: str | None,
    format: str | None,
    capture_warnings: bool,
    loggers: dict[str, str],
    warnings_filters: list[str] | None = None,
) -> None:
    """Apply the release's logging settings before the application is imported.

    The host calls this once; the workers inherit the result, and the
    environment variables tell the application what was chosen.
    """

    os.environ["MURU_LOG_LEVEL"] = level or "WARNING"
    os.environ["MURU_LOG_FORMAT"] = format or "standard"

    factory = logging.getLogRecordFactory()

    if not getattr(factory, "muru_tagging", False):
        logging.setLogRecordFactory(_tagging(factory))

    handler = logging.StreamHandler(sys.stderr)
    handler.setFormatter(formatter(format or "standard"))
    logging.lastResort = handler

    if level:
        logging.getLogger().setLevel(level)
        _installed[""] = logging.getLogger().level

    for name, named_level in loggers.items():
        logging.getLogger(name).setLevel(named_level)
        _installed[name] = logging.getLogger(name).level

    _filter_warnings(warnings_filters or [])

    if capture_warnings:
        logging.captureWarnings(True)
        fallback = _FallbackHandler(sys.stderr)
        fallback.setFormatter(formatter(format or "standard"))
        logging.getLogger("py.warnings").addHandler(fallback)
