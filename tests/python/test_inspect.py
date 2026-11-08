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
