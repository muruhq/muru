# Static files

A route that names a directory instead of an application is served by
the edge: Python never sees the request.

```toml
[[route]]
host = 'site'
path = '/static/'
static = '/srv/shop/collected-static'
cache = { max_age = '1y', immutable = true }
```

| Key | Default | Meaning |
| --- | --- | --- |
| `static` | | the directory served |
| `index` | `['index.html']` | the files tried, in order, when a directory is requested |
| `fallback` | none | `{ app = 'name' }` sends what no file answers to the application, `{ upstream = 'name' }` to an [HTTP upstream](upstreams.md); `{ file = 'index.html' }` serves that file instead, for a single-page application |
| `cache` | none | `max_age` (for example `1y` or `1h`), `immutable`, and `except`, patterns of files that get `no-cache` instead |
| `redirect_slash` | `true` | redirect a directory asked for without a trailing slash (301) to the slash form, so relative links resolve |
| `case` | `strict` | `insensitive` accepts names that differ from the file's on disk in case |
| `internal` | `false` | the route answers 404 to clients and is reached only through an [internal redirect](internal-redirects.md) |
| `accept_redirects_from` | none | applications other than the `fallback` one whose redirects may reach the route |

Every file is sent with `Content-Type` from its name, `ETag`,
`Last-Modified` and `Accept-Ranges`. `If-None-Match` and
`If-Modified-Since` are answered `304`, a single `Range` is answered
`206` (`416` when it lies past the end), and `HEAD` sends the headers
alone. Where a client accepts `br`, `zstd` or `gzip` and a file with
that suffix lies next to the file (`app.js.br`, `app.js.zst`,
`app.js.gz`), the variant is served with `Content-Encoding` and `Vary`;
otherwise a large text file is [compressed](compression.md) on the fly.

## An application's own files

Files that ship with the application, such as a Flask package's
`static` folder, are declared in the project rather than in a route,
so `muructl run` and the daemon serve them the same way, and nothing in
the application has to serve them in development:

```toml
[[tool.muru.static]]
path = '/static/'
dir = 'source:shop/static'
cache = { max_age = '1h' }
```

Each entry names a path prefix and a directory, with `index`,
`fallback` (a file), `cache`, `redirect_slash` and `case` as a static
route has them. `source:SUBDIR` names a directory in the project, and
`source:` the project itself. A plain relative path, `dir =
'shop/static'`, means the same; `source:` says so where `static:` or
`media:` entries sit beside it. Neither may climb out with `..`. An
absolute path names that directory wherever the release comes from.
`--static /static/=DIR`, repeatable on `muructl run` and `muructl
deploy`, adds an entry or replaces the one with its prefix for that run
or release. A deploy without the flag takes the source's entries alone,
so a source that drops an entry drops it. The edge serves a path under the
prefix from the directory when a file answers it, with the cache
headers, conditional requests, ranges and precompressed variants of a
static route; what has no file, and every method files do not answer,
reaches the application as usual, so a `/static/` path the application
handles itself still works. `/` cannot be a prefix, since every request
would then be looked up as a file first; a site that is files at `/`
with the application under them is a [static route with a
fallback](#mixing-files-and-an-application).

Under the daemon a directory in the source is read from the release's
own source, so the release the request was routed to serves its own
files: a canary answers with its own assets, and a rollback brings the
old ones back with the old code. `muructl config export` writes such a
directory as `source:SUBDIR`, so the exported release names its own
source's directory rather than one path on one node. `muructl run`
prints each entry at start-up and refuses a directory that does not
exist, as a deploy does.

## Built files

Files a release builds rather than ships, such as the output of
Django's `collectstatic`, go in a directory of the release's own, which
the release is given as `MURU_STATIC_DIR` and a static entry names as
`static:`:

```toml
[[tool.muru.static]]
path = '/static/'
dir = 'static:'
cache = { max_age = '1y', immutable = true }
```

`static:SUBDIR` names a directory in it. Where it is depends on how the
application runs:

| How the application runs | `MURU_STATIC_DIR` |
| --- | --- |
| The daemon | `STATE/static/APP/RELEASE` in the daemon's state directory, made empty when the release is created and removed with it; `muructl release list` counts it in what each release takes |
| `muructl run` | `runs/NAME-HASH/static` in the user state directory, beside the run's [data directory](data-directory.md), never in the project |
| `muructl serve` | `staticfiles` in the project, which the image's build fills; `--static-dir`, `MURU_SERVE_STATIC_DIR` or `static_dir` in the serve table names another, a relative one taken from the project |
| `muructl command run` and `muructl exec` with no application | the run's, unless `MURU_SERVE_STATIC_DIR` names another |

Under the daemon a declared command run as a `before_fork` gate fills
the directory once for each release, after its import and before its
workers fork and before it takes traffic, so a canary and a rollback
each serve their own assets:

```toml
[tool.muru.commands.collectstatic]
run = ['python', 'manage.py', 'collectstatic', '--noinput']
before_fork = true
timeout = '5m'
```

The directory belongs to the release's user. Under a daemon whose
applications run as [their own users](users.md), its group is the
edge's alone and it is setgid, so the files the command writes keep
that group and the edge can read them, as long as they are readable by
their group, as Django's storage writes them.

A `static:` entry whose directory is empty or missing is served as
always: its requests find no file and reach the application.
`muructl run` says so at start-up, and Django's
`staticfiles_urlpatterns()` serves the files while `DEBUG` is on.
`muructl command run collectstatic` fills the run's directory, and the
edge serves the files from then on.

A Flask package's own `static` folder ships with the code, and is
`source:` and its directory in the project; `static:` is for files a
release builds. The two look alike when written `dir = 'static'` and
`dir = 'static:'`, which is what `source:static` avoids.

## Files from the media part

`media:SUBDIR` serves a directory in the application's media part, the
files it saved for clients, to everyone:

```toml
[[tool.muru.static]]
path = '/media/'
dir = 'media:public'
```

Every file under it is public. Under a daemon whose applications run as
their own users the edge reads these files as its own user, so they
must be readable by the edge's user, which you arrange. The rest of the
media part is served through [file hints](file-hints.md) alone, after
the application's own checks, and the rest of the data directory never
is; see [keeping data between releases](data-directory.md#what-is-served).

## Mixing files and an application

Routes match by longest path prefix, so a site can be files with the
application under a prefix, or an application with files under a prefix:

```toml
[[route]]
host = 'site'
path = '/'
static = '/srv/site/public'

[[route]]
host = 'site'
path = '/app/'
app = 'shop'
```

When files and application paths overlap, a fallback sends the
application what no file answers. For `GET` and `HEAD` the edge serves
the file when there is one, or the directory's index file; otherwise, and
for every other method, the request goes to the application, so a `POST`
to a path that also has a file still reaches it:

```toml
[[route]]
host = 'site'
path = '/'
static = '/srv/site/public'
fallback = { app = 'shop' }
```

A single-page application serves its shell for every path that has no
file, with the hashed assets cached for a year and the shell itself
always revalidated:

```toml
[[route]]
host = 'site'
path = '/'
static = '/srv/spa/dist'
fallback = { file = 'index.html' }
cache = { max_age = '1y', immutable = true, except = ['index.html'] }
```

## What is never served

Hidden files and directories (names starting with a dot) are never
served and fall through to the fallback. A path is percent-decoded and
normalised before the file is looked up, and `..` never climbs out. The
file is opened beneath the directory, one name at a time from the
directory's own open descriptor, and what is sent is read from that
open file, never from a path looked up again. A symbolic link inside
the directory is followed only when its target is relative and stays
inside, so `latest.css -> v2/site.css` is served while a link to
`../secrets` or to any absolute path is not. The directory itself may
be a symbolic link, to another disk for example, and when it is pointed
somewhere else the next request serves from the new directory. Each
name in the path must match the name
on disk exactly, in case and in Unicode form, so a request for
`logo.png` does not serve `Logo.PNG` on macOS the way the file system
would; `case = 'insensitive'` opts a route out of that check.

## Serving a directory in development

`muructl serve-static DIR` serves a directory with no configuration, for
checking a built front end or a documentation build, with the same code
as a static route:

```
muructl serve-static dist/ --port 3000
muructl serve-static dist/ --fallback index.html
muructl serve-static site/ --index index.htm --index index.html --no-compression
```

`--bind`, `--port` (or `auto`), `--access-log` and `--access-log-file`
work as they do for `muructl run`.
