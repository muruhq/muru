# File hints

A file hint hands the body of a response to the edge, which sends the
file itself while the application thread or task is free. There are two
forms. By descriptor, the application's own open file is passed to the
edge, which sends exactly the range the application chose with the
status and headers it set. By path, a response header names a file, and
the edge serves it the way it serves a [static file](static-files.md).

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
conditional requests answered 304 and `Range` requests answered 206.
The response's own status and headers go with the file: a
`Content-Type` the application set replaces the guessed one, and a
status other than 200 sends the whole file with that status. The
`X-Muru-Sendfile` header itself never reaches the client.

The edge opens the path as its own user, and only within the
directories the mount's `sendfile_roots` lists. A relative root is
taken from the release's source directory, so the setting travels with
the project and means the same in every release and under `muructl
run`, `muructl serve` and the daemon; an absolute root is used as
written:

```toml
[tool.muru]
sendfile_roots = ['media', 'build/reports']

[tool.muru.mounts.archive]
path = '/archive'
entrypoint = 'media.archive:application'
sendfile_roots = ['/srv/archive']
```

A relative root must be a directory in the release's source when the
release is deployed, or the deploy fails, as it does for a static
entry's directory; `muructl run` refuses to start without it. An
absolute root may be made or mounted later. The source directory as a
whole is never a root unless it is named.

The release's roots apply to its default mount and to every mount that
does not list its own; without any, every path hint is refused. The
hint must name an absolute path, under a root by its leading
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
  new one when it has been moved.

A path that is relative, outside the roots, or leaves its root is
answered 500 `sendfile_refused` and logged with the path; a path that
names nothing, or not a regular file, or a file or root that cannot be
opened, is 404 `sendfile_missing`. `muructl deploy --sendfile-root
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
`sendfile_header`:

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
its source, and `sendfile_header` apply to the server's responses.
