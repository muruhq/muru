import os
import sqlite3
from pathlib import Path

# The application's data directory, which every release shares; beside
# the code when it runs outside Muru.

DATA = Path(os.environ.get("MURU_DATA_DIR", Path(__file__).parent))
DATABASE = DATA / "catalog.db"


def application(environ, start_response):
    with sqlite3.connect(DATABASE) as db:
        rows = db.execute("SELECT name, price FROM products ORDER BY name").fetchall()

    body = "".join(f"{name}  {price}\n" for name, price in rows) or "no products yet\n"
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [body.encode()]
