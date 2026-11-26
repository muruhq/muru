"""`muructl daemon config` (issue 894): the daemon's own configuration
file found, checked and changed, each change checked before it is
written and then given to the running daemon by a reload."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from support import server as support
from support.daemon import start_daemon


def test_the_daemons_file_is_changed_checked_and_reloaded(tmp_path: Path) -> None:
    daemon = start_daemon(tmp_path, listeners=[(support.free_port(), False)])
    file = tmp_path / "muru.toml"

    def config(
        *arguments: str, stdin: str | None = None, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        return daemon.muructl("daemon", "config", *arguments, check=False, stdin=stdin, env=env)

    try:
        # The running daemon says which file it reads.

        assert config("path").stdout.strip() == str(file)
        assert config("check").stdout.strip() == f"muru: {file} is valid"

        # set changes one key, keeps the rest of the file, and reloads.

        file.write_text("# how it listens\n" + file.read_text())
        changed = config("set", "limits.max_body_size", "50MB")
        assert changed.returncode == 0, changed.stderr
        assert f"muru: wrote {file}" in changed.stdout, changed.stdout
        assert "changed [limits] (the edge has it)" in changed.stdout, changed.stdout
        written = file.read_text()
        assert written.startswith("# how it listens\n[[listener]]"), written
        assert 'max_body_size = "50MB"' in written, written

        # A setting read only at start is written, and the reload says
        # the daemon keeps what it started with.

        late = config("set", "daemon.detours", "true")
        assert late.returncode == 0, late.stderr
        assert (
            "[daemon] detours changed and is read at start, so the daemon keeps what it started "
            "with until it is restarted"
        ) in late.stdout, late.stdout

        # With --json each says what it did (issue 941), the settings
        # that wait for a restart among it.

        shown = json.loads(config("show", "--json").stdout)
        assert shown["path"] == str(file) and shown["present"] is True, shown
        assert "detours = true" in shown["document"], shown
        assert json.loads(config("check", "--json").stdout)["valid"] is True

        told = json.loads(config("set", "limits.max_body_size", "60MB", "--json").stdout)
        assert told["written"] is True and told["restarted"] is False, told
        assert told["stale"] == ["[daemon] detours"], told
        assert "changed [limits]" in told["reload"], told

        refused = config("set", "limits.max_body_size", "lots", "--json")
        assert refused.returncode != 0
        assert "is not a size" in json.loads(refused.stdout)["error"], refused.stdout
        assert config("set", "limits.max_body_size", "50MB").returncode == 0

        # A mistake is refused and nothing is written, by set, by replace
        # and by check of a file that is elsewhere.

        before = file.read_text()
        bad = config("set", "limits.max_body_size", "50 megs")
        assert bad.returncode != 0 and "is not a size" in bad.stderr, bad.stderr

        serving = config("replace", "-", stdin="[app.shop]\ndata_dir = '/srv/shop'\n")
        assert serving.returncode != 0
        assert "[app.shop] says what the daemon serves" in serving.stderr, serving.stderr
        assert file.read_text() == before

        other = tmp_path / "other.toml"
        other.write_text("[limits]\nmax_body_size = 'lots'\n")
        elsewhere = config("check", str(other))
        assert elsewhere.returncode != 0 and str(other) in elsewhere.stderr, elsewhere.stderr

        # replace --check tests a whole file in the daemon's place, and
        # replace installs it.

        whole = before.replace("50MB", "70MB")
        tested = config("replace", "-", "--check", stdin=whole)
        assert tested.returncode == 0, tested.stderr
        assert "nothing was written" in tested.stdout and file.read_text() == before

        installed = config("replace", "-", stdin=whole)
        assert installed.returncode == 0, installed.stderr
        assert "changed [limits]" in installed.stdout, installed.stdout

        # edit runs the editor on a copy, checks what it saved and writes
        # it; an edit that changes nothing writes nothing.

        edited = config("edit", env={"EDITOR": "sed -i.bak s/70MB/80MB/"})
        assert edited.returncode == 0, edited.stderr
        assert 'max_body_size = "80MB"' in file.read_text()
        assert "changed [limits]" in edited.stdout, edited.stdout

        same = config("edit", env={"EDITOR": "true"})
        assert same.stdout.strip() == "muru: no changes", same.stdout + same.stderr

        broken = config("edit", env={"EDITOR": "sed -i.bak s/80MB/eighty/"})
        assert broken.returncode != 0
        assert "is not a size" in broken.stderr and "the file was not written" in broken.stderr
        assert 'max_body_size = "80MB"' in file.read_text()

        # unset takes the key out, and the full form of show is a file
        # the daemon takes.

        assert config("unset", "limits.max_body_size").returncode == 0
        assert "max_body_size" not in file.read_text()

        full = tmp_path / "full.toml"
        full.write_text(config("show", "--full").stdout)
        assert "(default)" in full.read_text()
        assert config("check", str(full)).returncode == 0
    finally:
        daemon.stop()

    # With no daemon running the file is still found and changed, and
    # the command says the daemon reads it when it starts.

    where = [
        "--config",
        str(file),
        "--run-dir",
        str(daemon.run_dir),
        "--state-dir",
        str(tmp_path / "state"),
        "--log-dir",
        str(tmp_path / "logs"),
    ]
    muructl = support.muructl_path()
    assert muructl is not None
    stopped = subprocess.run(
        [str(muructl), "daemon", "config", "set", "limits.max_body_size", "20MB", *where],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert stopped.returncode == 0, stopped.stderr
    assert "the daemon is not running; it reads the file when it starts" in stopped.stdout
    assert 'max_body_size = "20MB"' in file.read_text()
