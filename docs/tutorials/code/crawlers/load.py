import sys
import threading
import time
import urllib.error
import urllib.request

seconds = float(sys.argv[1])
crawl_for = float(sys.argv[2]) if len(sys.argv) > 2 else 0
url = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:8080/"
start = time.monotonic()
counts = {"people": {}, "crawlers": {}}
lock = threading.Lock()


def client(kind, agent, until):
    while time.monotonic() < until:
        began = time.monotonic()
        request = urllib.request.Request(url, headers={"User-Agent": agent})

        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            status = error.code

        with lock:
            counts[kind][status] = counts[kind].get(status, 0) + 1

        time.sleep(max(0.0, 0.2 - (time.monotonic() - began)))


# Two people's browsers, five requests a second each, for the whole run,
# and four crawlers as fast for the first crawl_for seconds.

threads = [
    threading.Thread(target=client, args=("people", "Mozilla/5.0", start + seconds))
    for _ in range(2)
] + [
    threading.Thread(target=client, args=("crawlers", "GPTBot/1.2", start + crawl_for))
    for _ in range(4)
]

for thread in threads:
    thread.start()


def answers(kind):
    found = counts[kind]
    return ", ".join(f"{count} x {status}" for status, count in sorted(found.items())) or "none"


# Every five seconds, the answers so far, by status.

while time.monotonic() < start + seconds:
    time.sleep(5)

    with lock:
        print(
            f"{time.monotonic() - start:3.0f}s  people {answers('people')}"
            f"  crawlers {answers('crawlers')}",
            flush=True,
        )

for thread in threads:
    thread.join()
