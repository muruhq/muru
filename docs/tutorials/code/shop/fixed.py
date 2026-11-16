"""The tutorial's later fixes, each a function of shop.py once fixed.

The tutorial shows each of them in place of the one of the same name in
shop.py, after joined.py's books(), and its test makes the same changes.
"""

import json
import urllib.request

from flask import abort, render_template_string, request
from shop import PAGE, app, database


@app.route("/")
def books():
    query = (
        "SELECT book.id, book.title, author.name FROM book"
        " JOIN author ON author.id = book.author_id ORDER BY book.id"
    )
    rows = database().execute(query).fetchall()
    stock = in_stock([book_id for book_id, _, _ in rows])
    lines = [f"{title} by {author}: {stock[book_id]} in stock" for book_id, title, author in rows]
    return render_template_string(PAGE, title="Books", lines=lines)


def in_stock(book_ids):
    """Ask the stock service how many of each book are in stock, in one call."""

    ids = ",".join(str(book_id) for book_id in book_ids)

    with urllib.request.urlopen(f"{request.host_url}stock?ids={ids}") as response:
        return {int(book_id): count for book_id, count in json.load(response).items()}


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

    if not stock:
        return render_template_string(PAGE, title=row[0], lines=["No books yet"])

    average = sum(stock) / len(stock)
    lines = [f"{len(stock)} books, {average:.1f} copies of each on average"]
    return render_template_string(PAGE, title=row[0], lines=lines)
