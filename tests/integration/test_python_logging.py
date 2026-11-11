"""Python logging defaults (design specification section 11.4).

The host sets the root logger's level and replaces logging.lastResort with
a handler in the platform format before the application is imported, from
the project's [tool.muru.logging.python] table or a declared release's
logging settings, and publishes what it chose as MURU_LOG_LEVEL and
MURU_LOG_FORMAT.
"""

from __future__ import annotations

import json
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import Daemon, daemon

__all__ = ["daemon"]

LOGGER = support.ROOT / "tests" / "fixtures" / "projects" / "logger"


def wait_for(predicate: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.05)


def worker_records(output: str) -> list[dict[str, Any]]:
    """The JSON records a worker wrote, from the relayed output."""

    records = []

    for line in support.untimed(output).splitlines():
        prefix, separator, rest = line.partition(": ")

        if separator and prefix.startswith("muru.worker[") and rest.startswith("{"):
            records.append(json.loads(rest))

    return records


def test_without_settings_python_defaults_hold_in_the_platform_format() -> None:
    server = support.start("logging_wsgi:application", workers=1, threads=1)

    try:
        server.mark_log()
        report = json.loads(server.get("/plain").body)
        assert report == {
            "MURU_LOG_LEVEL": "WARNING",
            "MURU_LOG_FORMAT": "standard",
            "root_level": "WARNING",
            "noisy_level": "NOTSET",
            "request_id": report["request_id"],
        }
        assert server.wait_for_log("WARNING noisy: noise for /plain")
        output = server.log_since_mark()
        assert "muru.worker[0]: WARNING demo: warned for /plain" in output
        assert "served /plain" not in output
        assert "debug for" not in output
    finally:
        server.stop()


def test_every_record_carries_the_request_beside_what_the_application_passes() -> None:
    server = support.start("logging_wsgi:application", workers=1, threads=1)

    try:
        server.mark_log()
        report = json.loads(server.get("/tagged").body)
        line = f"tagged {report['request_id']} 0 mine served /tagged"
        assert server.wait_for_log(line), server.log_since_mark()
    finally:
        server.stop()


def test_a_project_table_sets_level_format_loggers_and_warnings() -> None:
    server = support.start(str(LOGGER), workers=None, threads=None, chdir=None)

    try:
        server.mark_log()
        report = json.loads(server.get("/warn").body)
        assert report["MURU_LOG_LEVEL"] == "INFO"
        assert report["MURU_LOG_FORMAT"] == "json"
        assert server.wait_for_log('"logger": "py.warnings"')
        records = worker_records(server.log_since_mark())
        served = next(record for record in records if record["message"] == "served /warn")
        assert served["level"] == "INFO"
        assert served["logger"] == "demo"
        assert (served["app"], served["group"], served["worker"]) == ("logger", "default", 0)
        assert served["time"].endswith("Z")
        assert served["request_id"] == report["request_id"]

        # The noisy logger is raised to ERROR, and warnings are captured.

        assert not any(record["logger"] == "noisy" for record in records)
        warned = next(record for record in records if record["logger"] == "py.warnings")
        assert "deprecated path" in warned["message"]
    finally:
        server.stop()


def test_a_deployed_release_uses_the_project_settings(daemon: Daemon) -> None:
    assert daemon.deploy("logger", LOGGER) == "muru: release logger@r1 is ready and serving"
    report = json.loads(daemon.request("/one").body)
    assert (report["MURU_LOG_LEVEL"], report["MURU_LOG_FORMAT"]) == ("INFO", "json")
    wait_for(lambda: '"message": "served /one"' in daemon.muructl("logs", "logger").stdout)
    records = worker_records(daemon.muructl("logs", "logger").stdout)
    served = next(record for record in records if record["message"] == "served /one")
    assert served["app"] == "logger"
    assert served["level"] == "INFO"

    # The settings are part of the release, so an export carries them.

    exported = daemon.muructl("config", "export", "--app", "logger").stdout
    assert "logging.python" in exported, exported
    assert 'level = "INFO"' in exported, exported
    assert 'noisy = "ERROR"' in exported, exported


DAEMON_LOGGING = """
[logging.python]
level = "DEBUG"
loggers = { noisy = "WARNING", urllib3 = "WARNING" }
warnings = ["error::UserWarning"]
"""


def test_the_daemons_defaults_lie_under_the_applications(tmp_path: Path) -> None:
    """The daemon's [logging.python] applies key by key under the
    application's table: the project's own keys win, the daemon's fill in
    the rest, and its warnings filter is in force."""

    from support.daemon import start_daemon

    server = start_daemon(tmp_path, config_extra=DAEMON_LOGGING)

    try:
        assert server.deploy("logger", LOGGER) == "muru: release logger@r1 is ready and serving"
        report = json.loads(server.request("/one").body)
        assert (report["MURU_LOG_LEVEL"], report["MURU_LOG_FORMAT"]) == ("INFO", "json")

        # The daemon's filter turns the application's UserWarning into an
        # error, so the request fails.

        assert server.request("/warn").status == 500

        shown = server.muructl("config", "show", "logger", "--effective").stdout

        for line in [
            "logger@r1: logging.python.level = INFO ([tool.muru])",
            "logger@r1: logging.python.loggers.noisy = ERROR ([tool.muru])",
            "logger@r1: logging.python.loggers.urllib3 = WARNING (daemon configuration)",
            "logger@r1: logging.python.warnings.0 = error::UserWarning (daemon configuration)",
        ]:
            assert line in shown, shown

        # A reload takes the new table for the next host, and says so.

        (tmp_path / "muru.toml").write_text(
            (tmp_path / "muru.toml").read_text().replace('"DEBUG"', '"ERROR"')
        )
        reloaded = server.muructl("daemon", "reload").stdout
        assert "each application takes the new defaults at its next restart or deploy" in reloaded
    finally:
        server.stop()


def test_a_bad_warnings_filter_is_refused_when_the_file_is_read(tmp_path: Path) -> None:
    import pytest
    from support.daemon import start_daemon

    with pytest.raises(RuntimeError, match="is not a warnings filter"):
        start_daemon(tmp_path, config_extra='[logging.python]\nwarnings = ["shout"]\n')
