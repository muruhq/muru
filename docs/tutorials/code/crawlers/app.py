import time

PAGE = b"<!doctype html><title>Shop</title><h1>Today's offers</h1>\n"


def application(environ, start_response):
    time.sleep(0.02)  # as if rendering from a database
    start_response("200 OK", [("Content-Type", "text/html")])
    return [PAGE]
