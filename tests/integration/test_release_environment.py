"""What a release's processes start from under the daemon (specification section 8.7)."""

from __future__ import annotations

import json
import sys
from pathlib import Path

from support import server as support
from support.daemon import SHOP, start_daemon

LEGACY = support.ROOT / "tests" / "fixtures" / "projects" / "legacy"
PRINT = (
    "import os; print(os.environ.get('INHERITED_ONLY'), os.environ.get('PASSED_ON'), "
    "os.environ.get('HOME') is not None)"
)


def test_a_release_starts_from_a_clean_environment(tmp_path: Path) -> None:
    daemon = start_daemon(
        tmp_path,
        config_extra="\n[daemon]\npass_env = ['PASSED_*']\n",
        env={"INHERITED_ONLY": "from the shell", "PASSED_ON": "by name"},
    )

    try:
        deployed = daemon.muructl(
            "deploy", "shop", str(SHOP), "--python", sys.executable, "--env", "DECLARED=1"
        )
        assert deployed.returncode == 0, deployed.stderr

        # The workers: what the daemon was given stays out, what its file
        # passes comes through, and inspect shows those values.

        inspected = json.loads(daemon.muructl("debug", "inspect", "shop", "--json").stdout)
        environment = inspected["workers"][0]["result"]["environment"]
        assert "INHERITED_ONLY" not in environment
        assert environment["PASSED_ON"] == "by name"
        assert environment["DECLARED"] == "1"
        assert environment["HOME"] is not None and environment["HOME"] != "***", environment

        # A command, the same.

        ran = daemon.muructl("exec", "shop", "--", "python", "-c", PRINT)
        assert ran.stdout.split() == ["None", "by", "name", "True"], ran.stdout

        # A managed server, the same.

        daemon.muructl("drain", "shop@r1")
        command = [sys.executable, "server.py", "--bind", "{bind}"]
        deployed = daemon.muructl(
            "deploy",
            "legacy",
            str(LEGACY),
            "--server",
            "generic",
            "--stop-signal",
            "TERM",
            "--",
            *command,
        )
        assert deployed.returncode == 0, deployed.stderr
        seen = json.loads(daemon.request("/").body)
        assert seen["env"]["INHERITED_ONLY"] is None
        assert seen["env"]["PASSED_ON"] == "by name"
    finally:
        daemon.stop()
