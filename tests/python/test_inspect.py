"""What debug inspect shows of the environment (design specification section 11.7)."""

from __future__ import annotations

import pytest

from muru import _inspect


@pytest.mark.parametrize(
    ("key", "value", "shown"),
    [
        ("DATABASE_URL", "postgres://shop:hunter2@db/shop", "postgres://shop:***@db/shop"),
        (
            "REDIS_URL",
            "rediss://default:s3cret@cache.example:6380/0?ssl_cert_reqs=none",
            "rediss://default:***@cache.example:6380/0?ssl_cert_reqs=none",
        ),
        ("AMQP_URL", "amqp://guest:gu@st@mq:5672/", "amqp://guest:***@mq:5672/"),
        (
            "CLUSTER",
            "redis://a:one@h1:6379,redis://b:two@h2:6379 http://c:three@h3",
            "redis://a:***@h1:6379,redis://b:***@h2:6379 http://c:***@h3",
        ),
        ("DATABASE_URL", "postgres://shop@db/shop", "postgres://shop@db/shop"),
        ("DATABASE_URL", "postgres://db/shop", "postgres://db/shop"),
        ("SENTRY_DSN", "https://abc123@o1.ingest.sentry.io/1", "***"),
        ("sentry_dsn", "https://abc123@o1.ingest.sentry.io/1", "***"),
        ("API_SECRET", "anything", "***"),
        ("DJANGO_SETTINGS_MODULE", "shop.settings", "shop.settings"),
        ("PATH", "/usr/bin:/bin", "/usr/bin:/bin"),
        ("HOME", "/Users/shop", "/Users/shop"),
        ("MOTD", "it's a URL: http://x:y@z and more", "it's a URL: http://x:***@z and more"),
        ("EMPTY", "", ""),
    ],
)
def test_a_password_in_a_url_is_hidden_and_the_rest_kept(key: str, value: str, shown: str) -> None:
    assert _inspect.redact(key, value) == shown


def test_values_are_shown_for_muru_the_interpreter_and_the_release_only(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    for key in list(_inspect.os.environ):
        monkeypatch.delenv(key)

    monkeypatch.setenv("MURU_APP", "shop")
    monkeypatch.setenv("PATH", "/usr/bin")
    monkeypatch.setenv("PYTHONPATH", "/opt/muru")
    monkeypatch.setenv("LC_ALL", "C.UTF-8")
    monkeypatch.setenv("DATABASE_URL", "postgres://shop:hunter2@db/shop")
    monkeypatch.setenv("API_TOKEN", "abc")
    monkeypatch.setenv("STRIPE_ACCOUNT", "acct_1")
    monkeypatch.setenv(_inspect.DECLARED, "DATABASE_URL,API_TOKEN")

    assert _inspect.environment(every=False) == {
        "API_TOKEN": "***",
        "DATABASE_URL": "postgres://shop:***@db/shop",
        "LC_ALL": "C.UTF-8",
        "MURU_APP": "shop",
        "MURU_DECLARED_ENV": "DATABASE_URL,API_TOKEN",
        "PATH": "/usr/bin",
        "PYTHONPATH": "/opt/muru",
        "STRIPE_ACCOUNT": None,
    }

    # With --env, every value, masked as before.

    assert _inspect.environment(every=True)["STRIPE_ACCOUNT"] == "acct_1"
    assert _inspect.environment(every=True)["API_TOKEN"] == "***"


def test_the_levels_changed_are_listed_until_they_revert() -> None:
    """A listing says which loggers are changed, to what, from what and
    until when, and changes nothing itself."""

    import json
    import logging
    import time

    before = logging.getLogger("muru_test.listed").level

    try:
        _inspect.set_level("muru_test.listed", "DEBUG", 60.0, False)
        _inspect.set_level("muru_test.held", "ERROR", None, False)
        answer = json.loads(
            _inspect.handle('{"id": 1, "op": "logging", "args": {"list": true}}', [])
        )
        changed = {entry["logger"]: entry for entry in answer["result"]["changed"]}

        assert changed["muru_test.listed"]["level"] == "DEBUG"
        assert changed["muru_test.listed"]["previous"] == logging.getLevelName(before)
        until = changed["muru_test.listed"]["until_unix_ms"]
        assert time.time() * 1000 < until <= (time.time() + 61) * 1000
        assert changed["muru_test.held"]["until_unix_ms"] is None
        assert logging.getLogger("muru_test.listed").level == logging.DEBUG
    finally:
        _inspect.set_level("", "", None, True)

    assert _inspect.changed_levels() == {"changed": []}


def test_the_logging_setup_names_where_levels_came_from_and_hides_credentials() -> None:
    """The setup lists each logger with where its level came from, and of
    a handler only where it writes, never its credentials."""

    import json
    import logging
    import logging.handlers

    from muru import logging as muru_logging

    installed = logging.getLogger("muru_test.installed")
    changed = logging.getLogger("muru_test.changed")
    mailed = logging.getLogger("muru_test.mailed")
    saved = dict(muru_logging._installed)
    mail = logging.handlers.SMTPHandler(
        ("smtp.example.com", 587),
        "app@example.com",
        ["ops@example.com"],
        "boom",
        credentials=("user", "hunter2"),
    )
    web = logging.handlers.HTTPHandler("logs.example.com", "/in?token=abc")
    web.host = "user:s3cret@logs.example.com"

    try:
        installed.setLevel(logging.INFO)
        changed.setLevel(logging.INFO)
        muru_logging._installed.update(
            {"muru_test.installed": logging.INFO, "muru_test.changed": logging.INFO}
        )
        changed.setLevel(logging.ERROR)
        _inspect.set_level("muru_test.mailed", "DEBUG", None, False)
        mailed.addHandler(mail)
        mailed.addHandler(web)

        answer = json.loads(_inspect.handle('{"id": 1, "op": "logging_setup"}', []))
        setup = answer["result"]
        found = {logger["name"]: logger for logger in setup["loggers"]}

        assert setup["loggers"][0]["name"] == "root"
        assert found["muru_test.installed"]["level_from"] == "muru"
        assert found["muru_test.changed"]["level_from"] == "changed since"
        assert found["muru_test.mailed"]["level_from"] == "temporary"
        handlers = found["muru_test.mailed"]["handlers"]
        assert handlers[0]["destination"] == "mail via smtp.example.com:587 to ops@example.com"
        assert "***@logs.example.com" in handlers[1]["destination"]
        assert "hunter2" not in json.dumps(setup) and "s3cret" not in json.dumps(setup)
        assert isinstance(setup["warnings_filters"], list)
    finally:
        _inspect.set_level("", "", None, True)
        mailed.removeHandler(mail)
        mailed.removeHandler(web)
        muru_logging._installed.clear()
        muru_logging._installed.update(saved)
