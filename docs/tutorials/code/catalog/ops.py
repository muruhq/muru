import os
import sqlite3
import time
from datetime import UTC, datetime, timedelta
from pathlib import Path

DATA = Path(os.environ.get("MURU_DATA_DIR", Path(__file__).parent))
BACKUPS = DATA / "backups"


def backup(seconds="30"):
    """Copy the database to backups/, a step a second, as a big one would."""

    BACKUPS.mkdir(exist_ok=True)
    target = BACKUPS / f"catalog-{datetime.now(UTC):%Y%m%dT%H%M%S}.db"

    with sqlite3.connect(DATA / "catalog.db") as source, sqlite3.connect(target) as copy:
        steps = int(seconds)

        for step in range(1, steps + 1):
            time.sleep(1)
            print(f"copied {step} of {steps}", flush=True)

        source.backup(copy)

    print(f"backed up to {target.relative_to(DATA)}")


def cleanup(keep="1d", pause="0"):
    """Remove backups older than `keep`, after `pause` seconds."""

    time.sleep(int(pause))
    days = int(keep.removesuffix("d"))
    cutoff = datetime.now(UTC) - timedelta(days=days)
    removed = 0

    for path in sorted(BACKUPS.glob("*.db")) if BACKUPS.exists() else []:
        if datetime.fromtimestamp(path.stat().st_mtime, UTC) < cutoff:
            path.unlink()
            removed += 1

    print(f"removed {removed} backups older than {keep}")
