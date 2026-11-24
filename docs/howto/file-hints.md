# File hints

A file hint hands the body of a response to the edge, which sends the
file itself while the application thread or task is free. There are two
forms. By descriptor, the application's own open file is passed to the
edge, which sends exactly the range the application chose with the
status and headers it set. By path, a response header names a file, and
the edge serves it the way it serves a [static file](static-files.md).
Either way, for an application Muru's workers serve, the file is opened
in the worker, as the application's user, and handed to the edge open,
so the edge's user needs no access to the application's files.

## By descriptor

A WSGI application returns the object `wsgi.file_wrapper` gives it as
the response iterable:

```python
def application(environ, start_response):
    handle = open("/srv/media/report.pdf", "rb")
    start_response("200 OK", [("Content-Type", "application/pdf")])
    return environ["wsgi.file_wrapper"](handle)
```

When the wrapper itself is returned and the file is a regular file, a
duplicate of its descriptor goes to the edge, starting at the file's
current position and covering `Content-Length` bytes when that header
is set, or the rest of the file otherwise. The application's file is
closed as soon as the descriptor has been passed; the edge sends from
its own copy. An object without a regular file behind it, such as a
`BytesIO`, a wrapper that middleware has wrapped in something else, or a
wrapper returned after `write()` has sent output, is iterated in blocks
as PEP 3333 describes.

An ASGI application finds `http.response.pathsend` and
`http.response.zerocopysend` in `scope["extensions"]`. A `pathsend`
message names a file, which the worker opens as the application's user;
a `zerocopysend` message gives an open file with an optional `offset`
(the file's position when absent), `count` (the rest of the file when
absent) and `more_body`, so a file can sit between body chunks:

```python
await send({"type": "http.response.start", "status": 200, "headers": headers})
await send({"type": "http.response.pathsend", "path": "/srv/media/report.pdf"})
```

Starlette's `FileResponse` sends `pathsend` when the server advertises
it, so Starlette and FastAPI applications get this without changes.

The application has already decided the status and headers, so the edge
never applies a client's `Range` to a descriptor hint: a framework that
supports ranges computes the range itself and passes the matching offset
and length.

## By path

A response with an `X-Muru-Sendfile` header naming an absolute path has
its body replaced by that file, sent by the edge with static-file
behaviour: `Content-Length`, `ETag`, `Last-Modified`, `Accept-Ranges`,
conditional requests answered 304, `Range` requests answered 206 (416
past the end), `HEAD` answered with the headers alone, and
`Content-Type` from the file's name. The response's own status and
headers go with the file: a `Content-Type` the application set replaces
the guessed one, and a status other than 200 sends the whole file with
that status. Any body the application sent with the header is
discarded, and the `X-Muru-Sendfile` header itself never reaches the
client. The application changes nothing for this under WSGI
(`start_response` headers) or ASGI (`http.response.start` headers).

Muru's runtime in the worker takes the header from the response, opens
the file as the application's user, and passes the open file to the
edge, which reads its length, `ETag` and `Last-Modified` from it. The
file needs to be readable by the application's user alone: one written
`0600` is served as well as one written `0644`, and the edge's user
needs nothing. Where a file with a `.br`, `.zst` or `.gz` suffix lies
beside the hinted file in an encoding the client accepts and
[compression](compression.md) allows, the worker opens that variant
too, and the edge sends it with `Content-Encoding` and `Vary`, as it
does for a static route; a `Range` request is answered from the file
itself.

## What a hint may name

The worker opens a hinted file only within the mount's roots: the
application's media part, `MURU_MEDIA_DIR` in its [data
directory](data-directory.md), and the directories the mount's
`sendfile_roots` lists. The roots catch a hint built from a request
parameter by mistake (`?file=../../settings.py`, `.env`), and keep what
a hint may name the same whatever answers the route.

The media part is a root of every mount wherever it lives, so a file
saved there is served by a hint with nothing configured:

```python
path = os.path.join(os.environ["MURU_MEDIA_DIR"], "reports", name)
start_response("200 OK", [("X-Muru-Sendfile", path)])
```

`sendfile_roots` adds others. `media:` names the media part and
`media:SUBDIR` a directory in it; `source:SUBDIR` names a directory in
the release's source directory, so the setting travels with the project
and means the same in every release and under `muructl run`, `muructl
serve` and the daemon. A plain relative root means the same as
`source:` with it. An absolute root is used as written:

```toml
[tool.muru]
sendfile_roots = ['source:build/reports']

[tool.muru.mounts.archive]
path = '/archive'
entrypoint = 'media.archive:application'
sendfile_roots = ['media:archive', '/srv/archive']
```

A root in the source must be a directory there when the release is
deployed, or the deploy fails, as it does for a static
entry's directory; `muructl run` refuses to start without it. An
absolute root may be made or mounted later. The source directory as a
whole is never a root unless it is named.

The rest of the data directory is never served, whatever the
configuration says. The data directory itself holds the application's
databases, with their `-wal` and `-shm` files, and its caches, and the
uploads part holds files kept but never handed out, so neither, nor any
directory containing either, can be a root or a static directory:
`data:` and `uploads:` are refused, and so is a path that leads there,
by its name or through a symbolic link, with a deploy failing and
`muructl run` refusing to start, saying why. A directory whose name
really starts with `media:` is written `./media:x`.

The release's roots apply to its default mount and to every mount that
does not list its own; without any, and with no data directory, as
under `muructl serve` without `--data-dir`, every path hint is refused.
The hint must name an absolute path, under a root by its leading
components, either the root as configured or the directory it leads
to. The file is then opened beneath the root, one name at a time from
the root's own open directory, and is never opened again by its path:

- A symbolic link beneath a root is followed only when its target is
  relative and stays beneath the root, so `media/latest.pdf ->
  reports/2026.pdf` is served and `media/escape -> ../settings.py` is
  not. An absolute link is refused, even one naming a file inside the
  root, and so is a path that climbs out with `..`.

- A root itself may be a symbolic link, as may the directories above
  it: an operator can move a root to another disk and leave a link in
  its place, with no change of configuration. Each hint checks that the
  root's path still leads to the directory being used, and opens the
  new one when it has been moved. Under a daemon whose applications run
  as [their own users](users.md), the media part must lead to a
  directory the application's user owns, or every hint beneath it is
  refused and the reason written to the application's output, so an
  application cannot point its media part at another user's files.

A path that is relative, outside the roots, or leaves its root is
answered 500 `sendfile_refused` and logged with the path; a path that
names nothing, or not a regular file, or a file or root that cannot be
opened, is 404 `sendfile_missing`, and a file the application's user
cannot read is logged with the reason in the application's output. `muructl deploy --sendfile-root
DIR` sets the release's roots from the command line, a relative one
taken from the source, and `muructl run` reads `sendfile_roots` from
the project's `[tool.muru]` table. In a file `muructl apply` reads, a
release's roots, and its mounts', are taken from its `source` too.

Frameworks and middleware that already speak another server's header
keep working: `sendfile_header = 'X-Sendfile'` on a release or a mount
honours that header exactly as `X-Muru-Sendfile`, and removes it from
the response. Without the key, `X-Sendfile` is an ordinary header that
reaches the client. `muructl deploy --sendfile-header NAME` sets it from
the command line.

## Upstreams and managed servers

An [upstream](upstreams.md) with `trust_muru_headers = true` may give
path hints too, within its own `sendfile_roots` and with its own
`sendfile_header`. The edge opens these files itself, as its own user,
beneath the roots in the same way, so they must be readable by the
edge's user, which the operator arranges; they are served without
precompressed variants:

```toml
[upstream.legacy]
addresses = ['127.0.0.1:8001']
trust_muru_headers = true
sendfile_roots = ['/srv/legacy/media']
sendfile_header = 'X-Sendfile'
```

An upstream's roots are absolute; in a file `muructl apply` reads, a
relative one is taken from the file's directory.

A [managed HTTP server](managed-servers.md) release is trusted as an
upstream, and its release's `sendfile_roots`, relative ones taken from
its source, and `sendfile_header` apply to the server's responses; the
edge opens those files as it does an upstream's.
