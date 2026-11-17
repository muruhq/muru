# Deferring imports

An import can be deferred until the code first uses what it imports. The
module is then not loaded as the application loads, and is loaded on
first use only if something uses it. Muru defers imports with
[unwrapt](https://unwrapt.readthedocs.io) when a project sets
`lazy_imports`.

## Writing `__lazy_modules__`

A module names the imports it wants deferred in `__lazy_modules__`,
before them:

```python
# shop/reports.py
__lazy_modules__ = ["openpyxl", "pandas"]

import openpyxl
import pandas


def export(rows):
    book = openpyxl.Workbook()
    ...
```

The same code defers the imports on every Python version Muru supports:

- **Python 3.15 and later** defer the imports a module's
  `__lazy_modules__` names themselves, whether or not `lazy_imports` is
  on.

- **Python 3.11 to 3.14** ignore `__lazy_modules__` on their own. With
  `lazy_imports` on, unwrapt defers the imports it names.

Python 3.15 also has a `lazy import` statement. It is a syntax error on
earlier versions, so a module that is to run on them uses
`__lazy_modules__`.

A deferred import binds its name to a stand-in, and the module is
imported when the code first takes an attribute of it, such as
`openpyxl.Workbook` above. Write `import openpyxl` rather than `from
openpyxl import Workbook` for an import to defer: below 3.15 unwrapt
defers whole-module imports alone, and a `from` import needs the module
at once.

## Turning it on

Add unwrapt to the project's dependencies, and set `lazy_imports`:

```toml
[project]
dependencies = ["flask", "unwrapt"]

[tool.muru]
entrypoint = "shop.app:app"
lazy_imports = true
```

`lazy_imports` needs unwrapt 1.0.0a2 or later in the project's
environment. Muru imports unwrapt only when `lazy_imports` is on.

## Imports in code the project does not own

A package the project depends on cannot be given a `__lazy_modules__` of
its own. unwrapt's `[tool.unwrapt.lazy_modules]` table names imports to
defer in such a module, each key the importing module and its value the
modules it imports:

```toml
[tool.unwrapt.lazy_modules]
"thirdparty.pkg.client" = ["numpy", "pandas"]
```

The table works on every Python version, 3.15 included, and with it
`lazy_imports` is what makes those imports deferred. unwrapt's
[external configuration](https://unwrapt.readthedocs.io/en/latest/external-config.html)
page says what each entry can name.

## Where the configuration comes from

unwrapt finds its configuration itself, in the directory the host runs
in: the project's directory under `muructl run` and `muructl serve`, and
the release's source under the daemon. The first of these it finds is
used:

1. the file the `UNWRAPT_CONFIG` variable names, which a project sets in
   `[tool.muru.env]` to keep the file elsewhere;

2. `unwrapt.toml`;

3. the `[tool.unwrapt]` table of `pyproject.toml`.

With none of them, only each module's own `__lazy_modules__` is
deferred. `muructl run --print-config` and `muructl serve --print-config`
show `lazy_imports` and the file the host finds.

A missing unwrapt, one older than 1.0.0a2, and a configuration file
unwrapt cannot use are logged with the reason, and the release starts
without deferral.

## On one machine, and under the daemon

`lazy_imports` in a project's `muru.local.toml` takes the place of the
project's for `muructl run` on that machine, so a developer can try a run
with deferral, or without it, and leave the committed settings alone:

```toml
# muru.local.toml
lazy_imports = false
```

A release's table for `muructl apply` takes `lazy_imports` too, in place
of the source's.

## At the fork and in the workers

Deferral is set up before anything of the application is imported, by an
instrumentation module of Muru's own, `muru._lazy_imports`, which comes
first in the release's [instrumentation list](instrumentation.md), ahead
of tracing's. The imports the other instrumentation modules make can be
deferred too. The module is not listed by hand.

Under preload, deferral stays in place through the fork. A deferred
module the host's load did not use is not loaded before the fork, and
each worker loads it when a request first uses it. As the host forks, it
writes one line to the release's log about the imports unwrapt deferred:

```
muru.host[4821]: lazy_imports: 14 imports deferred, none of them used before the fork
```

A deferred import that something used while the host loaded, the
application's own start-up code or an `on_load` hook say, was loaded
before the fork anyway, and the line names it with the module whose
import was deferred:

```
muru.host[4821]: lazy_imports: 14 imports deferred, 2 of them used before the fork, where deferring them saves nothing: pandas (by shop.reports), numpy (by pandas.core.api)
```

With per-worker imports, each worker defers as it loads.

In the [toolbar](toolbar.md)'s Timing panel, a module a request loaded
by using a deferred import is marked "(deferred)", with the file and line
of the code that used it. The line and the panel know of the imports
unwrapt defers: from Python 3.15, those the interpreter defers itself for
a module's `__lazy_modules__` are loaded on first use the same way and
show as plain loads.

## When deferring pays off

Muru starts a new release's host, and lets it load, before it sends the
release any traffic, so the time the host takes to import the
application is mostly out of sight: the release before it serves in the
meantime. Under preload, deferring an import to save that time saves
little. What it does change:

- **Memory.** A module the host loads before the fork is shared by every
  worker. A deferred module is loaded by each worker that uses it, and
  not shared, so deferring a module every request uses costs memory in
  every worker. Deferring one that few requests use, a reporting
  library or an export format, saves its memory in each worker that
  never needs it.

- **Fork safety.** A module that starts a thread or opens a file or a
  connection as it is imported makes the import check fall back to
  per-worker imports. Deferring that import moves it after the fork, into
  the worker that uses it, and the host can preload the rest.

- **Cold starts.** Where no release serves while the host loads, the
  first start of `muructl serve` in a container, say, the load's time is
  the time to the first response.

- **Development.** `muructl run` loads the application again on every
  reload, so a shorter load is felt on every change.

Against these, the first request that uses a deferred module in each
worker waits for it to load. The Timing panel shows that time, and the
Hints panel points at modules whose first load took a large share of a
request.

## Beside `preload_modules`

[`preload_modules`](../explanation/preload.md#modules-loaded-on-first-use)
works the other way: it loads before the fork a module the application
imports only on first use, so no worker loads it on a request.
`lazy_imports` takes a module the application imports as it loads out of
the host's load. A module that is deferred and also listed in
`preload_modules` is loaded before the fork, as the list says, and the
deferred import finds it loaded when it is first used.
