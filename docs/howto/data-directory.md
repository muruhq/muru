# Keeping data between releases

A release is a source directory, and the next release may be another
one. Files an application keeps from one release to the next, a SQLite
database, files saved from uploads, generated reports, go in the
application's data directory instead: one directory per application,
shared by all its releases, that Muru makes and tells the application
about.

## The three locations

Muru gives the release's host and workers, its
[commands](commands.md) and `muructl exec` three variables:

| Variable | Holds | Reached by a client |
| --- | --- | --- |
| `MURU_DATA_DIR` | databases with their `-wal` and `-shm` files, caches, Flask's instance folder, anything the application keeps for itself | never |
| `MURU_MEDIA_DIR` | `DATA/media`: files handed to clients, such as avatars, reports and exports | through a [file hint](file-hints.md), and through a static entry when one is declared |
| `MURU_UPLOADS_DIR` | `DATA/uploads`: files received and kept but never handed out, where [`upload.save()`](uploads.md) puts a file by default | never |

What decides the part is who may read a file, not where it came from:
an upload that will be served is saved straight into media. Muru makes
the data directory and both parts when they are missing, owned by the
application's user with mode `0700`, and never removes anything in
them: releases come and go, and the data directory stays.

The variables are set by Muru, and a value of the same name in the
release's own environment or in the shell that started `muructl` is
replaced, so it cannot redirect anything. An application reads each
with a fallback of its own, which works under any server and without
the `muru` package installed:

```python
import os
from pathlib import Path

DATA_DIR = Path(os.environ.get("MURU_DATA_DIR", "var"))
```

## Where it is

| How the application runs | The data directory |
| --- | --- |
| The daemon | `STATE/data/APP` in the daemon's state directory: `/var/lib/muru/data/APP` for the system daemon, `~/.local/state/muru/data/APP` for a user daemon on Linux, `~/Library/Application Support/muru/data/APP` on macOS, and under the state directory of a [named instance](named-instances.md) |
| `muructl run` | `runs/NAME-HASH/data` in your user state directory (`~/.local/state/muru` on Linux, `~/Library/Application Support/muru` on macOS), where `NAME` is the application's name and `HASH` comes from the project's path, so two checkouts of one project do not share a database. It is kept between runs and is never inside the project |
| `muructl serve` | none, unless `--data-dir`, `MURU_SERVE_DATA_DIR` or `data_dir` in the serve table names one, normally a mounted volume; with none the three variables are not set |
| `muructl command run` and `muructl exec` with no application | where `muructl run` keeps it, so `muructl command run migrate` migrates the database `muructl run` serves; `MURU_SERVE_DATA_DIR` names another, as it does for `muructl serve` |

`muructl run` prints the locations when it starts:

```
muru: data     /home/me/.local/state/muru/runs/shop-3f9a1c2e7b40/data
muru: media    /home/me/.local/state/muru/runs/shop-3f9a1c2e7b40/data/media, a file hint root
muru: uploads  /home/me/.local/state/muru/runs/shop-3f9a1c2e7b40/data/uploads
```

A part that is a symbolic link is shown with where it leads, as
`media -> /mnt/media/shop`. `MURU_STATE_DIR`, when set, takes the
place of the user state directory, as it does for a user's own daemon.

## Moving it

Under the daemon the locations are the application's, not a release's,
since every release shares them. Set them in its `[app.NAME]` table:

```toml
[app.shop]
data_dir = '/srv/shop'
media_dir = '/mnt/media/shop'
uploads_dir = 'incoming'
```

`data_dir` is absolute. A relative `media_dir` or `uploads_dir` is taken
from `data_dir`. A change applies to releases started after it.

Under `muructl run`, `--data-dir DIR`, `data_dir` in `[tool.muru.run]`
or in `muru.local.toml` moves the data directory, a relative one taken
from the project; the parts are always `media` and `uploads` in it.

Under `muructl serve`, `--data-dir`, `--media-dir` and `--uploads-dir`,
or `MURU_SERVE_DATA_DIR`, `MURU_SERVE_MEDIA_DIR` and
`MURU_SERVE_UPLOADS_DIR`, name each, so on Kubernetes media can be a
ReadWriteMany volume the replicas share while the database stays on a
ReadWriteOnce one. A relative media or uploads directory is taken from
the data directory, and `--media-dir` or `--uploads-dir` without a data
directory is refused. `muructl serve` checks at start that the data
directory exists and can be written, and fails naming the volume to
mount when it cannot.

An operator may also move any part to another disk and leave a symbolic
link in its place, with no change of configuration. The application
follows the link through the path at once, and Muru follows it the next
time it opens the directory. Under a daemon whose applications run as
[their own users](users.md), a media part that is a link must lead to a
directory owned by the application's user, or file hints beneath it are
refused, so an application cannot point its media at a directory of
another user's.

## Who owns it

Muru gives the directories it makes to the application's user and
changes nothing afterwards. A release that runs as a different user
from the one that owns the data directory is not refused, and fails
where its code cannot write; `muructl data show` prints each part's
owner, so a mismatch is visible.

## What is served

Only the media part is ever served. It is one of the
[`sendfile_roots`](file-hints.md) of every mount by default, wherever it
lives, so a file hint to a file in it works with nothing configured. A
static entry may serve a part of it to everyone:

```toml
[[tool.muru.static]]
path = '/media/'
dir = 'media:public'
```

`media:` names the media part and `media:SUBDIR` a directory in it, in
`sendfile_roots`, a static entry's `dir` and `--static`. Every file
under such an entry is public. Under a daemon whose applications run as
their own users, the edge serves those files as its own user, so they
must be readable by the edge's user, which you arrange; the rest of
media stays reachable through file hints alone, which the worker opens
as the application's user.

The data directory, the uploads part and any directory that contains
either can never be a `sendfile_roots` entry or a static directory, and
`data:` and `uploads:` are refused; a deploy that names one fails, saying
why. A media part that contains the data directory or the uploads part
is refused too. A relative directory whose name really starts with
`media:` is written `./media:x`.

## SQLite on it

A SQLite database belongs in `MURU_DATA_DIR`, and needs three things
there:

- A local file system. SQLite's locks and the shared-memory file of WAL
  mode do not work on NFS, CIFS or another network file system. On
  Kubernetes that means a block-backed ReadWriteOnce volume.

- WAL mode. During a drain, a canary or while a rollback release is
  kept warm, two releases have the database open at once, and WAL lets
  readers and a writer work side by side.

- Schema changes that both releases can live with, as [database
  migrations](migrations.md) describes: add first, use in the next
  release, remove in the one after.

```python
import sqlite3

db = sqlite3.connect(DATA_DIR / "app.db")
db.execute("PRAGMA journal_mode=WAL")
```

## Checking it

```
muructl data show shop
muructl data check shop
muructl status --app shop --sizes
```

```
data     /var/lib/muru/data/shop, shop 0700, ext4
media    /var/lib/muru/data/shop/media -> /mnt/media/shop, shop 0700, xfs, a file hint root
uploads  /var/lib/muru/data/shop/uploads, shop 0700, ext4
spool    /var/lib/muru/spool, on the uploads part's file system, so upload.save() links
```

`muructl data show APP` prints each part's location, where a symbolic
link leads, its owner and mode, the file system it is on (on Linux),
which part is a file hint root, and whether the uploads part shares the
upload spool's file system, which decides whether `upload.save()` links
or copies. `--sizes` adds what each part holds, which walks it, and
`--json` gives the same as an object. `muructl status --app APP` prints
the parts in brief beside the daemon's status, and takes `--sizes` too.

`muructl data check APP` warns of a database file (`*.db`, `*.sqlite`,
`*.sqlite3`, or a `-wal` file) inside media, where a file hint could
send it; a database on a network file system; and a part not owned by
the application's user. It exits with status 1 when it finds any.
`muructl check` gives the same warnings for the directory `muructl run`
uses.

Under `muructl run` and `muructl serve`, `muructl status` prints the
run's data directory, and `muructl data show` and `muructl data check`
need no application named.

## Cleaning up

Muru never removes files from media or uploads, since only the
application knows which files its database still refers to. Cleanup is
a [declared command](commands.md) on a schedule:

```toml
[tool.muru.commands.cleanup]
description = 'Remove uploads no record refers to'
call = 'shop.ops:remove_orphans'
timeout = '10m'
schedule = { cron = '30 3 * * *' }
```

`muructl release prune` and `muructl release remove` never touch the
data directory. `muructl app remove APP` keeps it and prints where it
is; `muructl app remove APP --data` removes it too, after asking. A part
moved elsewhere, by configuration or through a symbolic link, is not
removed, and the command says where it is.

## Django

```python
import os
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent
DATA_DIR = Path(os.environ.get("MURU_DATA_DIR", BASE_DIR / "var"))

DATABASES = {
    "default": {
        "ENGINE": "django.db.backends.sqlite3",
        "NAME": DATA_DIR / "db.sqlite3",
        "OPTIONS": {"init_command": "PRAGMA journal_mode=WAL;"},
    }
}

MEDIA_ROOT = os.environ.get("MURU_MEDIA_DIR", DATA_DIR / "media")
MEDIA_URL = "/media/"

STORAGES = {
    "default": {"BACKEND": "django.core.files.storage.FileSystemStorage"},
    "private": {
        "BACKEND": "django.core.files.storage.FileSystemStorage",
        "OPTIONS": {"location": os.environ.get("MURU_UPLOADS_DIR", DATA_DIR / "uploads")},
    },
    "staticfiles": {"BACKEND": "django.contrib.staticfiles.storage.StaticFilesStorage"},
}
```

A `FileField(storage=storages["private"])` keeps its files where no
client reaches them. A file in `MEDIA_ROOT` is served after the view's
own permission check by a response with a file hint:

```python
from django.http import HttpResponse


def report(request, name):
    if not request.user.has_perm("reports.view"):
        return HttpResponse(status=403)

    response = HttpResponse(content_type="application/pdf")
    response["X-Muru-Sendfile"] = os.path.join(settings.MEDIA_ROOT, "reports", name)
    return response
```

or, for files anyone may see, by a static entry with `dir =
'media:public'`.

## Flask

```python
import os

from flask import Flask, send_from_directory

app = Flask(__name__, instance_path=os.environ.get("MURU_DATA_DIR"))
app.config["UPLOAD_FOLDER"] = os.environ.get("MURU_UPLOADS_DIR", "uploads")
app.config["USE_X_SENDFILE"] = True

MEDIA = os.environ.get("MURU_MEDIA_DIR", "media")


@app.route("/reports/<name>")
def report(name):
    return send_from_directory(MEDIA, name)
```

The instance folder is where Flask's own tutorial keeps its database
and an operator's `config.py`, so `instance_path` puts both in the data
directory; with the variable unset, Flask uses its usual `instance/`
folder. With `USE_X_SENDFILE`, `send_from_directory` answers with a file
hint and the edge sends the file.

## FastAPI

```python
import os

from fastapi import FastAPI, Request, Response
from sqlalchemy import create_engine

import muru.uploads

DATA_DIR = os.environ.get("MURU_DATA_DIR", "var")
MEDIA = os.environ.get("MURU_MEDIA_DIR", "media")

engine = create_engine(f"sqlite:///{DATA_DIR}/app.db")
app = FastAPI()


@app.post("/documents")
async def upload(request: Request):
    form = await request.form()
    stored = muru.uploads.get(request.scope, form["document.ref"])
    return {"saved": stored.save()}


@app.get("/reports/{name}")
async def report(name: str):
    return Response(headers={"X-Muru-Sendfile": os.path.join(MEDIA, "reports", name)})
```

`stored.save()` with no destination puts the file in `MURU_UPLOADS_DIR`
under a name of its own. The file hint takes the place of
`FileResponse`, which sends the file through Python.

## In a container

```
docker run -v shop-data:/data -e MURU_SERVE_DATA_DIR=/data registry.example.com/shop
```

[In a container](containers.md#keeping-data) and [a Kubernetes
Deployment](kubernetes.md#keeping-data) show the volumes.
