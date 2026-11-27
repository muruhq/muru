"""Traffic policies between releases (design specification section 9.4).

A route's `traffic` table shares its requests between an application's
releases: each client is bucketed by a key (an anonymous id cookie the edge
issues, a header, a cookie or the client address), the newest release's
buckets sit at the top, a sticky cookie keeps a client on the release it
first got, and a preview header or cookie pins a release, even one with no
share. A release a policy names is kept running, and one no policy names
drains, after sticky clients' grace.
"""

from __future__ import annotations

import json
import time
from collections import Counter
from pathlib import Path

from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


def policy_file(directory: Path, traffic: str) -> Path:
    """One host, one route to the shop application, and this policy."""

    path = directory / "traffic.toml"
    path.write_text(
        f"""[host.shop]
names = ["shop.test"]
default = true

[[route]]
host = "shop"
app = "shop"
{traffic}"""
    )
    return path


def served_by(daemon: Daemon, **headers: str) -> tuple[str, list[str]]:
    """The release that answered, and the cookies the edge set."""

    response = daemon.request("/", host="shop.test", headers=headers)
    assert response.status == 200, response.body
    release: str = json.loads(response.body)["release"]
    return release, response.header_values("Set-Cookie")


def states(daemon: Daemon) -> dict[str, str]:
    return {row["release"]: row["state"] for row in daemon.releases("shop")}


def test_a_split_shares_clients_and_cookies_pin_them(daemon: Daemon, tmp_path: Path) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    assert states(daemon) == {"shop@r1": "retained", "shop@r2": "ready"}

    # Half the buckets go to r1, which the policy brings back from
    # retention; r2 serves the rest, as the serving release.

    applied = daemon.muructl(
        "apply",
        "-f",
        str(
            policy_file(
                tmp_path,
                """
[route.traffic]
split = { r1 = 50 }
sticky = true
sticky_grace = "2s"
""",
            )
        ),
    )
    assert applied.returncode == 0, applied.stdout + applied.stderr
    assert states(daemon) == {"shop@r1": "ready", "shop@r2": "ready"}
    assert "shop (r1 50%, sticky)" in daemon.muructl("route", "list").stdout
    assert "traffic_policy.changed" in daemon.events()

    seen: Counter[str] = Counter()

    for n in range(200):
        release, cookies = served_by(daemon, Cookie=f"muru_id=client-{n}")
        seen[release] += 1
        assert any(cookie.startswith(f"muru_release={release};") for cookie in cookies), (
            release,
            cookies,
        )

    assert set(seen) == {"r1", "r2"} and 50 <= seen["r1"] <= 150, seen

    # A client with no id gets one, and keeps its release with it.

    release, cookies = served_by(daemon)
    issued = [cookie for cookie in cookies if cookie.startswith("muru_id=")]
    assert len(issued) == 1 and "HttpOnly" in issued[0], cookies
    muru_id = issued[0].split(";")[0]

    for _ in range(5):
        assert served_by(daemon, Cookie=muru_id)[0] == release

    # The sticky cookie holds whatever the bucket says, and a preview pins
    # a release for the request alone.

    r1, r2 = daemon.preview("shop@r1"), daemon.preview("shop@r2")

    for n in range(20):
        assert served_by(daemon, Cookie=f"muru_id=client-{n}; muru_release=r1")[0] == "r1"
        assert served_by(daemon, Cookie=f"muru_id=client-{n}; muru_preview={r2}")[0] == "r2"
        assert served_by(daemon, **{"Muru-Preview": r1, "Cookie": f"muru_id=client-{n}"})[0] == "r1"

    # At 0% a release takes only previews and sticky clients.

    daemon.muructl(
        "apply",
        "-f",
        str(
            policy_file(
                tmp_path,
                '[route.traffic]\nsplit = { r1 = 0 }\nsticky = true\nsticky_grace = "2s"\n',
            )
        ),
    )
    assert all(served_by(daemon, Cookie=f"muru_id=fresh-{n}")[0] == "r2" for n in range(50))
    assert served_by(daemon, Cookie="muru_release=r1")[0] == "r1"
    assert served_by(daemon, **{"Muru-Preview": r1})[0] == "r1"
    named = daemon.request("/", host="shop.test", headers={"Muru-Preview": r1})
    assert named.header_values("Muru-Preview") == []
    unknown = daemon.request("/", host="shop.test", headers={"Muru-Preview": "r9"})
    assert json.loads(unknown.body)["release"] in ("r1", "r2")
    assert unknown.header_values("Muru-Preview") == ["ignored"]

    # With no share left, r1 drains once its sticky clients' grace passes;
    # until then their cookie still reaches it, afterwards it is ignored.

    daemon.muructl(
        "apply",
        "-f",
        str(policy_file(tmp_path, '[route.traffic]\nsticky = true\nsticky_grace = "2s"\n')),
    )
    assert states(daemon)["shop@r1"] == "draining"
    assert served_by(daemon, Cookie="muru_release=r1")[0] == "r1"
    deadline = time.monotonic() + 15

    while states(daemon)["shop@r1"] != "retained" and time.monotonic() < deadline:
        time.sleep(0.5)

    assert states(daemon) == {"shop@r1": "retained", "shop@r2": "ready"}
    assert served_by(daemon, Cookie="muru_release=r1")[0] == "r2"


def test_a_sticky_cookie_reaches_only_what_the_policy_names(daemon: Daemon, tmp_path: Path) -> None:
    # Issue 955: a held release takes no traffic, and naming it in the
    # sticky cookie does not reach it until the policy gives it a share.

    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP, "--hold")
    assert states(daemon) == {"shop@r1": "ready", "shop@r2": "ready"}

    applied = daemon.muructl(
        "apply", "-f", str(policy_file(tmp_path, "[route.traffic]\nsticky = true\n"))
    )
    assert applied.returncode == 0, applied.stdout + applied.stderr
    assert served_by(daemon, Cookie="muru_release=r2")[0] == "r1"
    assert served_by(daemon, Cookie="muru_release=r1")[0] == "r1"

    applied = daemon.muructl(
        "apply",
        "-f",
        str(policy_file(tmp_path, "[route.traffic]\nsplit = { r2 = 0 }\nsticky = true\n")),
    )
    assert applied.returncode == 0, applied.stdout + applied.stderr
    assert served_by(daemon, Cookie="muru_release=r2")[0] == "r2"
    assert served_by(daemon)[0] == "r1"


def answered(daemon: Daemon, path: str = "/", **headers: str) -> tuple[object, ...]:
    """What a client can tell from a response: the release that served it,
    whether its preview was ignored, and the cookies set."""

    response = daemon.request(path, host="shop.test", headers=headers)
    assert response.status == 200, response.body
    return (
        json.loads(response.body)["release"],
        response.header_values("Muru-Preview"),
        response.header_values("Set-Cookie"),
    )


def test_a_preview_needs_the_releases_secret(daemon: Daemon, tmp_path: Path) -> None:
    # Issue 928: r2 is held, and nothing but its ID and its preview secret
    # together reaches it.

    # With --open-preview the held release is opened in the browser as it
    # is deployed, which $BROWSER names here, by an entry link.

    opened = tmp_path / "opened"
    launcher = tmp_path / "browser"
    launcher.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{opened}'\n")
    launcher.chmod(0o755)
    daemon.deploy("shop", SHOP)
    held = daemon.muructl(
        "deploy", "shop", str(SHOP), "--hold", "--open-preview", env={"BROWSER": str(launcher)}
    )
    assert held.stdout.startswith("muru: release shop@r2 is ready and held: not serving\n")
    assert "in preview of shop@r2" in held.stdout + held.stderr, held.stderr
    deadline = time.monotonic() + 20

    while not opened.exists() and time.monotonic() < deadline:
        time.sleep(0.05)

    first = opened.read_text().split()[0].split(f":{daemon.port}", 1)[1]
    entered = daemon.request(first)
    assert (entered.header("set-cookie") or "").startswith("muru_preview=r2."), entered.headers
    assert "muructl preview shop@r2 previews it" in daemon.muructl("release", "list").stdout
    applied = daemon.muructl("apply", "-f", str(policy_file(tmp_path, "")))
    assert applied.returncode == 0, applied.stdout + applied.stderr

    # The application alone means its one held release.

    value = daemon.preview("shop")
    release, _, secret = value.rpartition(".")
    assert release == "r2" and len(secret) == 32 and int(secret, 16) >= 0
    assert daemon.preview("shop@r2") == value, "the secret is kept"
    other = daemon.preview("shop@r1").rpartition(".")[2]
    assert other != secret

    # The header and the cookie preview it, the serving release too, and
    # the application sees neither, with its other cookies as they were.

    assert answered(daemon, **{"Muru-Preview": value}) == ("r2", [], [])
    assert answered(daemon, Cookie=f"muru_preview={value}") == ("r2", [], [])
    assert answered(daemon, **daemon.preview_header("shop@r1")) == ("r1", [], [])
    seen = daemon.request(
        "/preview-seen",
        host="shop.test",
        headers={"Muru-Preview": value, "Cookie": f"a=b; muru_preview={value}; c=d"},
    )
    assert json.loads(seen.body) == {"release": "r2", "header": None, "cookie": "a=b; c=d"}

    # Every miss is one answer: a bare ID, a wrong secret, another
    # release's secret, a release that does not exist with and without a
    # secret. Nothing says whether the release exists.

    misses = ["r2", f"r2.{secret[:-1]}0", f"r2.{secret[:-1]}1", f"r2.{other}", "r9", f"r9.{secret}"]
    by_header = {miss: answered(daemon, **{"Muru-Preview": miss}) for miss in misses}
    assert all(got == ("r1", ["ignored"], []) for got in by_header.values()), by_header
    by_cookie = {miss: answered(daemon, Cookie=f"muru_preview={miss}") for miss in misses}
    expired = "muru_preview=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"
    assert all(got == ("r1", ["ignored"], [expired]) for got in by_cookie.values()), by_cookie

    # An entry link works once and sets the cookie; a link is refused a
    # life of more than an hour, and one past its time enters nothing.

    def entry(*arguments: str) -> str:
        link = daemon.muructl("preview", "shop@r2", "--print", *arguments).stdout.strip()
        assert link.startswith(f"http://shop.test:{daemon.port}/_muru/preview/"), link
        return link.split(f":{daemon.port}", 1)[1]

    link = entry("--any-name")
    entered = daemon.request(link, host="shop.test")
    assert entered.status == 303 and entered.header("location") == "/"
    assert (entered.header("set-cookie") or "").split(";")[0] == f"muru_preview={value}"
    again = daemon.request(link, host="shop.test")
    assert again.status == 404 and b"muructl preview gives a new one" in again.body

    asked = daemon.muructl("preview", "shop@r2", "--valid-for", "10m", "--any-name", "--json")
    assert json.loads(asked.stdout)["valid_for_ms"] == 600_000
    refused = daemon.muructl("preview", "shop@r2", "--valid-for", "2h", check=False)
    assert refused.returncode != 0 and "lasts at most an hour" in refused.stderr, refused.stderr
    brief = entry("--any-name", "--valid-for", "1s")
    time.sleep(1.2)
    assert daemon.request(brief, host="shop.test").status == 404

    # A new secret: what was given out before previews nothing, a link
    # made before sets the new cookie, and the event says nothing secret.

    earlier = entry("--any-name")
    renewed = daemon.muructl("preview", "shop@r2", "--new-secret", "--header")
    assert "shop@r2 has a new preview secret" in renewed.stderr
    fresh = renewed.stdout.strip().removeprefix("Muru-Preview: ")
    assert fresh.startswith("r2.") and fresh != value
    assert answered(daemon, **{"Muru-Preview": value}) == ("r1", ["ignored"], [])
    assert answered(daemon, **{"Muru-Preview": fresh}) == ("r2", [], [])
    entered = daemon.request(earlier, host="shop.test")
    assert (entered.header("set-cookie") or "").split(";")[0] == f"muru_preview={fresh}"
    assert "release.preview_secret_changed" in daemon.events()

    # A second held release says how to preview it, and makes the
    # application alone ambiguous: the refusal names both.

    third = daemon.muructl("deploy", "shop", str(SHOP), "--hold")
    assert third.stdout.strip() == "muru: release shop@r3 is ready and held: not serving"
    assert third.stderr.strip() == "muru: muructl preview shop@r3 opens a preview of it"
    refused = daemon.muructl("preview", "shop", "--header", check=False)
    assert refused.returncode != 0, refused.stdout
    assert "shop has 2 held releases" in refused.stderr, refused.stderr
    assert "shop@r2, shop@r3" in refused.stderr, refused.stderr

    # A release that takes no requests is not previewed, and an unknown
    # one is refused.

    for target, why in (("shop@r9", "there is no release shop@r9"), ("nothing", "no application")):
        refused = daemon.muructl("preview", target, "--header", check=False)
        assert refused.returncode != 0 and why in refused.stderr, refused.stderr

    # None of the secrets is in what the daemon shows or writes: the
    # listings and their JSON, the events and the logs.

    shown = [
        daemon.muructl("release", "list", "--json").stdout,
        daemon.muructl("release", "show", "shop@r2", "--json").stdout,
        daemon.muructl("release", "show", "shop@r2").stdout,
        daemon.muructl("events", "--json").stdout,
        daemon.muructl("status").stdout,
        daemon.log(),
    ]
    written = [path.read_bytes() for path in (tmp_path / "logs").rglob("*") if path.is_file()]
    assert written, "the daemon wrote no logs"

    for given in (secret, other, fresh.rpartition(".")[2]):
        assert not any(given in text for text in shown), given
        assert not any(given.encode() in content for content in written), given


def test_header_and_client_keys_and_invalid_policies(daemon: Daemon, tmp_path: Path) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    daemon.muructl(
        "apply",
        "-f",
        str(
            policy_file(
                tmp_path, '[route.traffic]\nsplit = { r1 = 50 }\nhash_key = "header:x-user"\n'
            )
        ),
    )
    assert "by header:x-user" in daemon.muructl("route", "list").stdout

    # The same user lands on the same release every time, without any
    # cookie; users spread over both releases.

    by_user = {f"user-{n}": served_by(daemon, **{"X-User": f"user-{n}"}) for n in range(40)}
    assert all(cookies == [] for _, cookies in by_user.values())
    assert {release for release, _ in by_user.values()} == {"r1", "r2"}
    assert all(
        served_by(daemon, **{"X-User": user})[0] == release
        for user, (release, _) in by_user.items()
    )

    # Without the header the client address is the key, so every request
    # from this test lands on one release.

    assert len({served_by(daemon)[0] for _ in range(10)}) == 1

    for policy, complaint in [
        ("[route.traffic]\nsplit = { r9 = 10 }\n", "names shop@r9, which does not exist"),
        ("[route.traffic]\nsplit = { r1 = 60, r2 = 50 }\n", "more than 100 percent"),
        (
            '[route.traffic]\nhash_key = "ip"\n',
            "hash_key must be cookie, header:NAME, cookie:NAME or client",
        ),
        ('[route.traffic]\nsticky_grace = "soon"\n', "sticky_grace"),
    ]:
        refused = daemon.muructl("apply", "-f", str(policy_file(tmp_path, policy)), check=False)
        assert refused.returncode != 0 and complaint in refused.stdout + refused.stderr, (
            refused.stdout + refused.stderr
        )


def test_a_cohort_and_a_route_take_releases_of_their_own(daemon: Daemon, tmp_path: Path) -> None:
    daemon.deploy("shop", SHOP)
    daemon.deploy("shop", SHOP)
    config = tmp_path / "cohorts.toml"
    config.write_text(
        """[[client_class]]
name = 'beta'
cookies = { plan = '^beta$' }

[host.shop]
names = ["shop.test"]
default = true

[[route]]
host = "shop"
app = "shop"
traffic = { classes = { beta = 'r1' } }

[[route]]
host = "shop"
path = "/legacy"
app = "shop"
traffic = { split = { r1 = 100 } }
"""
    )
    applied = daemon.muructl("apply", "-f", str(config))
    assert applied.returncode == 0, applied.stdout + applied.stderr

    # The beta cohort, known by its cookie, gets r1, and everyone else the
    # serving release.

    assert {served_by(daemon, Cookie="plan=beta")[0] for _ in range(5)} == {"r1"}
    others = {served_by(daemon, Cookie=f"plan=free; muru_id=c{n}")[0] for n in range(20)}
    assert others == {"r2"}

    # One route of the application stays on r1 for everyone.

    for n in range(10):
        response = daemon.request("/legacy", host="shop.test", headers={"Cookie": f"muru_id=l{n}"})
        assert json.loads(response.body)["release"] == "r1"
