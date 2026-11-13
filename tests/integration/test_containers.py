"""Muru as a container's main process (design specification sections 6.10,
13.3 and 13.9), in a container built from the working tree. PID 1 is
muructl's built-in init, so what a killed worker leaves behind is collected
and no zombie remains, and `docker stop` means shutdown: a request in flight
completes, the container exits 0 inside the budget. Both hold for `muructl
serve` and for the daemon, and serve runs under an arbitrary user ID with a
read-only root filesystem, answering its probes from inside.

The image takes minutes to build the first time and is cached after; the
suite carries the requires_docker marker and runs with `just test-docker`.

The image tag and the container names carry a short hash of the checkout's
path, so two checkouts can run the suite at once and a rerun in the same
checkout reuses its image layers. An image is left behind for that reason:
images from checkouts that are gone are left to `docker image prune`."""

from __future__ import annotations

import hashlib
import http.client
import shutil
import subprocess
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest

pytestmark = pytest.mark.requires_docker

ROOT = Path(__file__).resolve().parents[2]
CHECKOUT = hashlib.sha256(str(ROOT).encode()).hexdigest()[:8]
IMAGE = f"muru-test:{CHECKOUT}"

# A short worker stop budget, so the scenarios run in seconds.

TIMEOUTS = ["--timeout", "hard=1s", "--timeout", "hard_drain_grace=1s", "--timeout", "shutdown=1s"]


def docker_available() -> bool:
    binary = shutil.which("docker")
    if binary is None:
        return False
    return subprocess.run([binary, "info"], capture_output=True, check=False).returncode == 0


def wait_for(predicate: Callable[[], bool], timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout

    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.1)


@pytest.fixture(scope="module")
def image() -> str:
    """The test image, built from the working tree."""

    if not docker_available():
        pytest.fail("the container scenarios need a running container runtime")

    completed = subprocess.run(
        ["docker", "build", "-q", "-t", IMAGE, "-f", "images/test/Dockerfile", "."],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
        timeout=3600,
    )

    if completed.returncode != 0:
        pytest.fail(f"building {IMAGE} failed:\n{completed.stdout}{completed.stderr[-4000:]}")

    return IMAGE


class Container:
    """A container running muructl as its main process, reachable on a host port."""

    def __init__(self, name: str, port: int) -> None:
        self.name = name
        self.port = port

    def exec(
        self, *command: str, check: bool = True, user: str | None = None
    ) -> subprocess.CompletedProcess[str]:
        """Run a command in the container, as its own user unless told otherwise."""

        return subprocess.run(
            ["docker", "exec", *(["--user", user] if user else []), self.name, *command],
            capture_output=True,
            text=True,
            check=check,
            timeout=120,
        )

    def processes(self) -> list[tuple[int, int, str, str]]:
        """Every process in the container: pid, parent pid, state and command."""

        listing = self.exec(
            "python3",
            "-c",
            "import os\n"
            "for entry in sorted(int(e) for e in os.listdir('/proc') if e.isdigit()):\n"
            "    try:\n"
            "        stat = open(f'/proc/{entry}/stat', 'rb').read()\n"
            "    except OSError:\n"
            "        continue\n"
            "    name = stat[stat.find(b'(') + 1 : stat.rfind(b')')].decode()\n"
            "    fields = stat[stat.rfind(b')') + 1 :].split()\n"
            "    print(entry, fields[1].decode(), fields[0].decode(), name)\n",
        ).stdout
        processes = []

        for line in listing.splitlines():
            pid, parent, state, name = line.split(" ", 3)
            processes.append((int(pid), int(parent), state, name))

        return processes

    def logs(self) -> str:
        completed = subprocess.run(
            ["docker", "logs", self.name], capture_output=True, text=True, check=False
        )
        return completed.stdout + completed.stderr

    def get(self, path: str) -> http.client.HTTPResponse:
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=60)
        connection.request("GET", path)
        response = connection.getresponse()
        response.read()
        connection.close()
        return response

    def request_in_the_background(self, path: str) -> tuple[threading.Thread, list[str]]:
        """Request a path on a thread of its own, recording the status or the error."""

        outcome: list[str] = []

        def request() -> None:
            try:
                outcome.append(f"status {self.get(path).status}")
            except (OSError, http.client.HTTPException) as error:
                outcome.append(type(error).__name__)

        thread = threading.Thread(target=request)
        thread.start()
        return thread, outcome

    def stop(self, grace: int = 10) -> tuple[float, int]:
        """docker stop with the grace, returning how long it took and the exit code."""

        started = time.monotonic()
        subprocess.run(
            ["docker", "stop", "-t", str(grace), self.name], check=True, capture_output=True
        )
        elapsed = time.monotonic() - started
        code = subprocess.run(
            ["docker", "inspect", "-f", "{{.State.ExitCode}}", self.name],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        return elapsed, int(code)

    def remove(self) -> None:
        subprocess.run(["docker", "rm", "-f", self.name], check=False, capture_output=True)


@pytest.fixture
def container(image: str, request: pytest.FixtureRequest) -> Iterator[Callable[..., Container]]:
    """Start containers from the image, removed when the test ends."""

    started: list[Container] = []

    def start(
        *command: str, env: dict[str, str] | None = None, options: tuple[str, ...] = ()
    ) -> Container:
        name = f"muru-test-{CHECKOUT}-{request.node.name[:40]}-{len(started)}".replace(
            "[", "-"
        ).replace("]", "")
        subprocess.run(["docker", "rm", "-f", name], check=False, capture_output=True)
        environment = [f"--env={key}={value}" for key, value in (env or {}).items()]
        subprocess.run(
            [
                "docker",
                "run",
                "-d",
                "--name",
                name,
                "-p",
                "127.0.0.1::8080",
                *environment,
                *options,
                image,
                *command,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        mapping = subprocess.run(
            ["docker", "port", name, "8080"], capture_output=True, text=True, check=True
        ).stdout
        port = int(mapping.split("\n")[0].rsplit(":", 1)[1])
        container = Container(name, port)
        started.append(container)
        return container

    yield start

    for container in started:
        container.remove()


def wait_until_serving(container: Container) -> None:
    def answers() -> bool:
        try:
            return container.get("/").status == 200
        except OSError:
            return False

    deadline = time.monotonic() + 60

    while not answers():
        assert time.monotonic() < deadline, f"not serving:\n{container.logs()}"
        time.sleep(0.2)


def test_muructl_serve_is_pid_1_with_the_init_and_leaves_no_zombie(
    container: Callable[..., Container],
) -> None:
    instance = container(
        "muructl",
        "serve",
        "plain_wsgi:application",
        "--processes",
        "1",
        "--threads",
        "2",
        *TIMEOUTS,
    )
    wait_until_serving(instance)

    # PID 1 is the init, and the command runs as its child.

    processes = instance.processes()
    assert processes[0][:2] == (1, 0) and processes[0][3] == "muructl", processes
    assert any(
        pid != 1 and parent == 1 and name == "muru-serve" for pid, parent, _, name in processes
    ), processes

    # A request starts a child and runs past the hard timeout, which kills
    # the worker; the host, a subreaper, stops the child and collects it,
    # and whatever else reparents to PID 1 is collected there.

    thread, _ = instance.request_in_the_background("/spawn?seconds=30&pidfile=/tmp/child.pid")
    wait_for(lambda: instance.exec("cat", "/tmp/child.pid", check=False).returncode == 0)
    child = int(instance.exec("cat", "/tmp/child.pid").stdout)
    wait_for(lambda: "worker.timeout_killed" in instance.logs())
    wait_for(lambda: child not in {pid for pid, _, _, _ in instance.processes()})
    thread.join(timeout=10)
    assert not [p for p in instance.processes() if p[2] == "Z"], instance.processes()
    assert instance.get("/").status == 200


def test_docker_stop_lets_a_request_finish_under_serve(
    container: Callable[..., Container],
) -> None:
    instance = container(
        "muructl",
        "serve",
        "plain_wsgi:application",
        "--processes",
        "1",
        "--threads",
        "2",
    )
    wait_until_serving(instance)
    thread, outcome = instance.request_in_the_background("/sleep?seconds=3")
    time.sleep(0.5)

    # SIGTERM through the init: the request in flight completes and the
    # container exits 0 inside the budget, under docker stop's grace.

    elapsed, code = instance.stop()
    thread.join(timeout=10)
    assert outcome == ["status 200"]
    assert code == 0, instance.logs()
    assert elapsed < 9, elapsed


def test_docker_stop_drains_the_daemon(container: Callable[..., Container]) -> None:
    instance = container(
        "muructl",
        "daemon",
        "run",
        "--listen",
        "0.0.0.0:8080",
        "--state-dir",
        "/var/lib/muru",
        "--run-dir",
        "/run/muru",
        "--log-dir",
        "/var/log/muru",
    )
    wait_for(
        lambda: instance.exec("test", "-S", "/run/muru/admin.sock", check=False).returncode == 0
    )
    deployed = instance.exec(
        "muructl",
        "deploy",
        "shop",
        "/projects/shop",
        "--python",
        "/usr/local/bin/python3",
        "--processes",
        "1",
        "--socket",
        "/run/muru/admin.sock",
    ).stdout
    assert "ready and serving" in deployed, deployed
    wait_until_serving(instance)

    # A health check needs nothing but muructl: the daemon answers the
    # readiness probe over its admin socket.

    probed = instance.exec("muructl", "probe", "ready", "--run-dir", "/run/muru", check=False)
    assert probed.returncode == 0, probed.stdout + probed.stderr
    thread, outcome = instance.request_in_the_background("/slow?ms=3000")
    time.sleep(0.5)

    # As the container's main process the daemon shuts down on SIGTERM
    # rather than detaching: the request finishes, the workers and the
    # edge stop, and the container exits 0 inside the budget.

    elapsed, code = instance.stop()
    thread.join(timeout=10)
    assert outcome == ["status 200"]
    assert code == 0, instance.logs()
    assert elapsed < 9, elapsed
    assert "SIGTERM: shutting down" in instance.logs(), instance.logs()


def test_the_init_can_be_turned_off(container: Callable[..., Container]) -> None:
    instance = container(
        "muructl",
        "serve",
        "plain_wsgi:application",
        "--processes",
        "1",
        env={"MURU_NO_INIT": "1"},
    )
    wait_until_serving(instance)

    # PID 1 runs the command itself, and a stop still drains.

    processes = instance.processes()
    assert processes[0][3] == "muru-serve", processes
    assert not any(pid != 1 and name == "muru-serve" for pid, _, _, name in processes), processes
    elapsed, code = instance.stop()
    assert code == 0, instance.logs()
    assert elapsed < 9, elapsed


def test_serve_reports_the_container_limits_and_guards_its_memory(
    container: Callable[..., Container],
) -> None:
    instance = container(
        "muructl",
        "serve",
        "plain_wsgi:application",
        "--memory",
        "check_interval=1s",
        *TIMEOUTS,
        options=("--cpus", "1.5", "--memory", "512m", "--memory-swap", "512m"),
    )
    wait_until_serving(instance)
    logs = instance.logs()
    assert (
        "muru: container limits: 1.5 CPUs from cpu.max 150000 100000, "
        "memory.max 512MB with 128MB of headroom"
    ) in logs, logs
    wait_for(lambda: "once the application is imported" in instance.logs())

    # A worker that leaves less than the headroom under memory.max is
    # replaced by the guard, stopping first since its successor would not
    # fit beside it, though no hard limit is set.

    thread, _ = instance.request_in_the_background("/grow?mb=400")
    wait_for(lambda: "the container was short of memory" in instance.logs())
    thread.join(timeout=30)
    assert "stops before its replacement starts" in instance.logs(), instance.logs()
    wait_until_serving(instance)

    # A resize while it runs is read at the next check.

    subprocess.run(
        ["docker", "update", "--memory", "768m", "--memory-swap", "768m", instance.name],
        check=True,
        capture_output=True,
    )
    wait_for(lambda: "memory.max from 512MB to 768MB" in instance.logs())

    # A worker that takes more than the container has, at once, is killed
    # by the kernel before its hard limit is checked; that is said, counted
    # and the worker started again.

    thread, _ = instance.request_in_the_background("/grow?mb=900")
    wait_for(lambda: "worker.oom_killed" in instance.logs())
    thread.join(timeout=30)
    metrics = instance.exec(
        "python3",
        "-c",
        "import urllib.request\n"
        "print(urllib.request.urlopen('http://127.0.0.1:9090/metrics').read().decode())",
    ).stdout
    assert 'muru_worker_exits_total{app="plain-wsgi"' in metrics, metrics
    assert 'reason="oom_killed"} 1' in metrics, metrics
    assert 'reason="container_memory"} 1' in metrics, metrics
    wait_until_serving(instance)


def test_serve_runs_as_an_arbitrary_user_on_a_read_only_root(
    container: Callable[..., Container],
) -> None:
    instance = container(
        "muructl",
        "serve",
        "plain_wsgi:application",
        "--processes",
        "1",
        options=(
            "--user",
            "1000770000:0",
            "--read-only",
            "--tmpfs",
            "/run/muru:mode=0770,gid=0",
            "--tmpfs",
            "/tmp",
            "--memory",
            "512m",
            "--memory-swap",
            "512m",
        ),
    )
    wait_until_serving(instance)

    # The probes answer inside the container, and a command reaches the
    # serve through the run directory.

    ready = instance.exec("muructl", "probe", "ready", check=False)
    assert ready.returncode == 0, ready.stdout + instance.logs()
    status = instance.exec("muructl", "status", check=False)
    assert status.returncode == 0, status.stdout + status.stderr
    assert instance.exec("python3", "-c", "import os; print(os.getuid())").stdout.strip() == (
        "1000770000"
    )

    # Section 13.9: another user ID with group 0 may use the serve too,
    # the spool stays its owner's, and a user outside the group that names
    # the socket is told which it may not use and whose it is.

    other = instance.exec("muructl", "status", check=False, user="5555:0")
    assert other.returncode == 0, other.stdout + other.stderr
    modes = instance.exec(
        "python3",
        "-c",
        "import os\n"
        "for name in ('admin.sock', 'probe.sock', 'serve.json', 'release', 'spool'):\n"
        "    print(name, oct(os.stat(f'/run/muru/{name}').st_mode & 0o777))\n",
    ).stdout.split("\n")
    assert modes[:5] == [
        "admin.sock 0o770",
        "probe.sock 0o770",
        "serve.json 0o660",
        "release 0o770",
        "spool 0o700",
    ], modes
    outside = instance.exec(
        "muructl", "status", "--socket", "/run/muru/admin.sock", check=False, user="5555:5555"
    )
    assert outside.returncode != 0
    assert "permission denied on /run/muru/admin.sock" in outside.stderr, outside.stderr
    assert "belongs to UID" in outside.stderr, outside.stderr

    # Without --socket the same user is told the same, having nothing else
    # to find, and with a run of their own finds that and is told nothing
    # about a directory they may not enter.

    nothing = instance.exec("muructl", "status", check=False, user="5555:5555")
    assert nothing.returncode != 0
    assert "permission denied on /run/muru/admin.sock" in nothing.stderr, nothing.stderr
    subprocess.run(
        [
            "docker",
            "exec",
            "--detach",
            "--user",
            "5555:5555",
            "--env",
            "HOME=/tmp/five",
            instance.name,
            "muructl",
            "run",
            "plain_wsgi:application",
            "--port",
            "auto",
            "--dashboard",
            "off",
            "--no-reload",
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )

    def own_run_found() -> bool:
        return instance.exec("muructl", "status", check=False, user="5555:5555").returncode == 0

    wait_for(own_run_found)
    own = instance.exec("muructl", "status", check=False, user="5555:5555")
    assert "/run/muru" not in own.stdout + own.stderr, own.stdout + own.stderr

    # Section 9.12: a command of the project runs through the serve, and
    # in a container of its own, where nothing serves, by muructl itself.

    release = "import muru, os; print('release', os.environ.get('MURU_RELEASE', 'none'))"
    through = instance.exec("muructl", "exec", "--", "python", "-c", release, check=False)
    assert through.returncode == 0, through.stdout + through.stderr
    assert through.stdout.strip() == "release r1"
    assert "[command exec] release r1" in instance.logs()

    alone = ["muructl", "exec", "--", "python", "-c", release]
    completed = subprocess.run(
        ["docker", "run", "--rm", "--user", "1000770000:0", "--read-only", IMAGE, *alone],
        capture_output=True,
        text=True,
        check=False,
        timeout=120,
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert completed.stdout.strip() == "release none"

    # Section 13.9: over its hard limit with no room for a second worker
    # of that size, the worker stops first and its successor starts after.

    thread, _ = instance.request_in_the_background("/grow?mb=360")
    wait_for(lambda: "stops before its replacement starts" in instance.logs(), timeout=30)
    thread.join(timeout=30)
    wait_for(lambda: ": worker.recycled: worker 0" in instance.logs(), timeout=30)
    wait_until_serving(instance)
    assert "worker.oom_killed" not in instance.logs(), instance.logs()

    elapsed, code = instance.stop()
    assert code == 0, instance.logs()
    assert elapsed < 9, elapsed


def test_serve_without_a_writable_run_directory_says_what_it_needs(
    image: str,
) -> None:
    def serve(*options: str) -> str:
        completed = subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--user",
                "1000770000:0",
                *options,
                image,
                "muructl",
                "serve",
                "plain_wsgi:application",
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
        assert completed.returncode != 0
        return completed.stdout + completed.stderr

    # A read-only root filesystem needs a volume; a writable one that the
    # user may not create the directory in needs the image to make it.

    output = serve("--read-only")
    assert "/run/muru cannot be written" in output, output
    assert "with a read-only root filesystem" in output, output
    output = serve()
    assert "/run/muru cannot be written by user 1000770000" in output, output
    assert "RUN install -d -o 1000770000 -g 0 -m 0770 /run/muru" in output, output
