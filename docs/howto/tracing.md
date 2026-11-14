# Tracing with wrapture

Muru traces an application with [wrapture](https://wrapture.readthedocs.io):
the calls it makes while it serves a request, its database queries,
template renders and outgoing HTTP calls, recorded as one tree per request
and written wherever wrapture's configuration sends them. Nothing in the
application changes.

## Turning it on

Add wrapture, and the instrumentation packages for what the application
uses, to the project's dependencies, then set `tracing` and give wrapture
its configuration in the same `pyproject.toml`:

```toml
[project]
dependencies = ["flask", "wrapture", "wrapture-instrumentation"]

[tool.muru]
entrypoint = "shop.app:app"
tracing = true

[[tool.wrapture.instrument]]
name = "flask"

[[tool.wrapture.instrument]]
name = "sqlite3"
statement = true

[[tool.wrapture.sink]]
type = "printer"
```

`muructl run` then prints each request's tree as it is served, from the
worker that served it:

```
muru.worker[1]: GET /books (shop.app.wsgi_app)
muru.worker[1]:   books()
muru.worker[1]:     sqlite3:Connection.execute(sql='<48 chars>', parameters='<0 values>')
muru.worker[1]:     sqlite3:Connection.execute -> '<Cursor>' [115us]
muru.worker[1]:   books -> '<str 505 chars>' [1.8ms]
muru.worker[1]: shop.app.wsgi_app -> '200 OK' [3.0ms, body 3us over 1 chunk]
```

With `tracing = true`, the configuration is the one wrapture finds itself:
the file `WRAPTURE_CONFIG` names, else `wrapture.toml`, else the
`[tool.wrapture]` table of `pyproject.toml`, in the project's directory.
To name the file, give `tracing` as a table:

```toml
[tool.muru.tracing]
config = "tracing/wrapture.toml"
```

A relative path is relative to the project's directory. `tracing = false`
turns tracing off and leaves the configuration in place.

Tracing needs Python 3.12 or later and wrapture 1.0.0b5 or later in the
project's environment. When either is missing, or no configuration is
found, or the configuration cannot be applied, the reason is logged with
its traceback and the release serves untraced.

## Requests

A framework's instrumentation, `flask` or `django` for instance, records
each request as the root of its tree, with the matched route, and with the
framework's own settings, such as the paths it ignores.

An application no instrumentation records requests for, a WSGI or ASGI
callable of its own or a framework with no instrumentation, has its
requests recorded by wrapture's request middleware, which Muru wraps
around the entry point with `tracing_middleware`:

```toml
[tool.muru]
entrypoint = "shop.app:app"
tracing = true

[tool.muru.mounts.feed]
path = "/feed"
entrypoint = "feed.asgi:app"
tracing_middleware = true
```

`tracing_middleware = true` in `[tool.muru]` wraps the entrypoint, and in a
mount's table wraps that mount. The request event is labelled with the
entry point as the table names it, `feed.asgi:app` here. Where a framework's
instrumentation records the request too, the middleware's boundary is the
outer one and records it, and the framework's passes through, so a
framework's request settings no longer apply; leave `tracing_middleware`
off for those. A gRPC entry point is not wrapped. While tracing is off,
`tracing_middleware` has no effect, so a project can say which entry
points to wrap and leave tracing itself to each developer.

## Tracing on one machine

`tracing` in a project's `muru.local.toml` takes the place of the
project's, for `muructl run` on that machine, so a developer can trace
their own runs without a change to the committed settings:

```toml
# muru.local.toml
tracing = true
```

or, with a configuration of their own:

```toml
[tracing]
config = "my-tracing.toml"
```

The project still needs wrapture in its dependencies, and the
`tracing_middleware` keys stay the project's. `muructl run --print-config`
shows which file `tracing` came from.

## Preloading and the workers

The configuration is applied before anything of the application is
imported, so its patches are in place for the application's own imports.
What it sends events to, its sinks, starts in each worker:

- **Preload.** The host applies the configuration before it imports the
  application, and each worker starts the sinks after it is forked. The
  host holds no sink, thread or open file of wrapture's when it forks, and
  what it does while it loads the application is not traced.

- **Per-worker imports.** With `preload = false`, each worker applies the
  configuration and starts the sinks before it imports the application,
  so the application's import is traced too, to the configuration's sinks.

Each worker writes its own output. A file sink names a file per process
with `{pid}` in its path, as in `path = "traces/trace-{pid}.jsonl"`, and a
relative path is relative to the configuration file's directory. What a
sink holds is delivered as the worker stops.

## Beside other instrumentation modules

Tracing is set up by an instrumentation module of Muru's own,
`muru._tracing`, which comes before the modules the
[instrumentation list](instrumentation.md) names. Its request middleware
is therefore the outermost wrapper of an entry point. The module is not
listed by hand.

## Checking

`muructl run --print-config` shows `tracing` and the entrypoint's
`tracing_middleware`. Under the daemon, `muru._tracing` is the first of a
release's instrumentation modules, and `muructl config export` shows it
with the configuration file and the entry points it wraps in its
settings.
