"""The tutorial's first fix: shop.py's books() with its authors joined.

The tutorial shows it in place of books() in shop.py, and its test makes
the same change.
"""

from flask import render_template_string
from shop import PAGE, app, database, in_stock


@app.route("/")
def books():
    query = (
        "SELECT book.id, book.title, author.name FROM book"
        " JOIN author ON author.id = book.author_id ORDER BY book.id"
    )
    lines = []

    for book_id, title, author in database().execute(query).fetchall():
        stock = in_stock(book_id)
        lines.append(f"{title} by {author}: {stock} in stock")

    return render_template_string(PAGE, title="Books", lines=lines)
