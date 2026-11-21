import os
import sqlite3
import sys
from pathlib import Path

HERE = Path(__file__).parent
DATA = Path(os.environ.get("MURU_DATA_DIR", HERE))

# Each file of migrations/ not applied yet, in name order, runs in a
# transaction of its own and is recorded as applied.

with sqlite3.connect(DATA / "catalog.db", isolation_level=None) as db:
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("CREATE TABLE IF NOT EXISTS applied (name TEXT PRIMARY KEY)")
    done = {name for (name,) in db.execute("SELECT name FROM applied")}

    for path in sorted((HERE / "migrations").glob("*.sql")):
        if path.name in done:
            continue

        db.execute("BEGIN")

        try:
            for statement in path.read_text().split(";"):
                if statement.strip():
                    db.execute(statement)

            db.execute("INSERT INTO applied VALUES (?)", (path.name,))
        except sqlite3.Error as error:
            db.execute("ROLLBACK")
            print(f"{path.name} failed: {error}", file=sys.stderr)
            sys.exit(1)

        db.execute("COMMIT")
        print(f"applied {path.name}", flush=True)

print("the schema is up to date")
