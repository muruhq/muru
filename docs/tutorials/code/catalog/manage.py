import os
import sqlite3
import sys
from pathlib import Path

DATABASE = Path(os.environ.get("MURU_DATA_DIR", Path(__file__).parent)) / "catalog.db"

# python manage.py add NAME PRICE

command, name, price = sys.argv[1:4]

with sqlite3.connect(DATABASE) as db:
    db.execute("INSERT OR REPLACE INTO products (name, price) VALUES (?, ?)", (name, price))

print(f"added {name} at {price}")
