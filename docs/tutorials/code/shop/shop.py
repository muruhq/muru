import json
import logging
import os
import sqlite3
import urllib.request

from flask import Flask, abort, g, jsonify, redirect, render_template_string, request

app = Flask(__name__)
log = logging.getLogger("shop")

DATABASE = os.path.join(os.path.dirname(__file__), "shop.db")

PAGE = """<!doctype html>
<html>
<head><title>{{ title }}</title></head>
<body>
<h1>{{ title }}</h1>
<ul>{% for line in lines %}<li>{{ line }}</li>{% endfor %}</ul>
<p><a href="/">Books</a> | <a href="/books/new">Add a book</a>
| <a href="/authors/4">An author</a></p>
</body>
</html>
"""

FORM = """<!doctype html>
<html>
<head><title>Add a book</title></head>
<body>
<h1>Add a book</h1>
<form method="post" action="/books">
<p><input name="title" value="Middlemarch"> by author <input name="author" value="1" size="2"></p>
<p><button>Add</button></p>
</form>
</body>
</html>
"""


def stock_the_shelves():
    """Make the shop's tables and books, once, when the application is imported."""

    connection = sqlite3.connect(DATABASE)
    connection.executescript(
        """
        CREATE TABLE IF NOT EXISTS author (id INTEGER PRIMARY KEY, name TEXT);
        CREATE TABLE IF NOT EXISTS book (
            id INTEGER PRIMARY KEY, title TEXT, author_id INTEGER, stock INTEGER
        );
        INSERT OR IGNORE INTO author VALUES
            (1, 'Jane Austen'), (2, 'Charles Dickens'), (3, 'George Eliot'),
            (4, 'Anne Brontë');
        INSERT OR IGNORE INTO book VALUES
            (1, 'Emma', 1, 4), (2, 'Persuasion', 1, 0),
            (3, 'Pride and Prejudice', 1, 7), (4, 'Bleak House', 2, 2),
            (5, 'Great Expectations', 2, 5), (6, 'Hard Times', 2, 1),
            (7, 'Silas Marner', 3, 3);
        """
    )
    connection.close()


stock_the_shelves()


def database():
    """The request's connection, made on first use."""

    if "db" not in g:
        g.db = sqlite3.connect(DATABASE)

    return g.db


@app.teardown_appcontext
def close(exc):
    connection = g.pop("db", None)

    if connection is not None:
        connection.close()


@app.route("/")
def books():
    rows = database().execute("SELECT id, title, author_id FROM book ORDER BY id").fetchall()
    lines = []

    for book_id, title, author_id in rows:
        query = "SELECT name FROM author WHERE id = ?"
        (author,) = database().execute(query, (author_id,)).fetchone()
        stock = in_stock(book_id)
        lines.append(f"{title} by {author}: {stock} in stock")

    return render_template_string(PAGE, title="Books", lines=lines)


def in_stock(book_id):
    """Ask the stock service how many of a book are in stock."""

    with urllib.request.urlopen(f"{request.host_url}stock/{book_id}") as response:
        return int(response.read())


# The stock service, which another team runs; here it is part of the same
# application, so the tutorial needs nothing else running.


@app.route("/stock/<int:book_id>")
def stock(book_id):
    (count,) = database().execute("SELECT stock FROM book WHERE id = ?", (book_id,)).fetchone()
    return str(count)


@app.route("/stock")
def stock_of_many():
    ids = [int(book_id) for book_id in request.args["ids"].split(",")]
    marks = ",".join("?" * len(ids))
    rows = database().execute(f"SELECT id, stock FROM book WHERE id IN ({marks})", ids)
    body = json.dumps({book_id: count for book_id, count in rows})
    return body, {"Content-Type": "application/json"}


@app.route("/authors/<int:author_id>")
def author(author_id):
    row = database().execute("SELECT name FROM author WHERE id = ?", (author_id,)).fetchone()

    if row is None:
        abort(404)

    stock = [
        count
        for (count,) in database().execute(
            "SELECT stock FROM book WHERE author_id = ?", (author_id,)
        )
    ]
    average = sum(stock) / len(stock)
    lines = [f"{len(stock)} books, {average:.1f} copies of each on average"]
    return render_template_string(PAGE, title=row[0], lines=lines)


@app.route("/books/new")
def new_book():
    return FORM


@app.route("/books", methods=["POST"])
def add_book():
    title = request.form["title"]
    database().execute(
        "INSERT INTO book (title, author_id, stock) VALUES (?, ?, 0)",
        (title, int(request.form["author"])),
    )
    database().commit()
    log.warning("added %r, with none in stock yet", title)
    return redirect("/")


@app.route("/report")
def report():
    total = 0

    for book_id in range(400_000_000):
        total += book_id % 7

    return f"{total}\n"


@app.route("/api/books")
def api_books():
    rows = (
        database()
        .execute(
            "SELECT book.title, author.name FROM book JOIN author ON author.id = book.author_id"
        )
        .fetchall()
    )
    return jsonify([{"title": title, "author": name} for title, name in rows])
