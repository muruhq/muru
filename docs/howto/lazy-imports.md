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

## Finding what to defer: the import report

`import_report = true` records the host's imports with unwrapt's profile
recorder as it loads the application, and keeps the report for
`muructl debug imports`. It is independent of `lazy_imports`:

| `lazy_imports` | `import_report` | What it gives |
| --- | --- | --- |
| on | off | Deferral alone |
| off | on | A report with nothing deferred: the run for finding candidates |
| on | on | A report with the deferrals in place: the run for checking them |

```toml
[tool.muru]
entrypoint = "shop.app:app"
import_report = true
```

`import_report` in `muru.local.toml` takes the place of the project's for
that machine's runs, and a release's table for `muructl apply` takes it
too. It needs unwrapt in the project's environment, as `lazy_imports`
does.

`muructl debug imports` prints the report of the host's load:

```
Import report of the host's load of shop@r3: outcome fork, Python 3.13.2, 4 workers
  412.3 ms importing, 213 modules loaded, 2804 import calls; deferring what can be deferred would save about 120.4 ms

Advice
  defer: openpyxl imported by shop.reports, 96.4 ms, loading entrypoint
    paid by the host, once before the fork, shared by the 4 workers; deferred, each worker that uses it loads it on first use
    add 'openpyxl' to __lazy_modules__ in shop.reports
      __lazy_modules__ = ['openpyxl']
  fork safety: shop.metrics imported by shop.app, 12.0 ms, loading entrypoint
    paid by its import left a thread running (metrics-flush) in the host, which cannot cross the fork
    add 'shop.metrics' to __lazy_modules__ in shop.app
      __lazy_modules__ = ['shop.metrics']

Imports that loaded a module, by cumulative time
  CUM MS SAVES MS IMPORTED      IMPORTED BY     LOADING    3.15 UNWRAPT NOTES
   140.2    140.2 shop.reports  shop.views      entrypoint yes  yes
    96.4     96.4 openpyxl      shop.reports    entrypoint yes  yes
...
```

- **The summary** is unwrapt's: the time the load spent importing, the
  modules it loaded, and a conservative estimate of what deferring every
  import that can be deferred would save.

- **The advice** names the imports deferring which saves the most, each
  with who pays for the module and the configuration to write. An import
  in the project's own code is deferred with the module's
  `__lazy_modules__`; one in a package the project depends on, with an
  entry of `[tool.unwrapt.lazy_modules]`. Under preload the host pays for
  a module once and its workers share it, so deferring it moves the load
  into each worker that uses it; with per-worker imports each worker pays
  for it as it starts. A module whose import left a thread, a socket or a
  child process in the host, which makes the release fall back to
  per-worker imports, is a strong candidate: deferred, it is loaded after
  the fork, in the worker that uses it. A deferred import the load used
  anyway is named too, since deferring it gains nothing.

- **LOADING** says which of the host's targets the import was made while
  loading: `hooks`, `entrypoint`, `mount:NAME`, or `module:NAME` for one
  of the `preload_modules`. A module several targets need is paid for by
  the first. An import made outside them, by an instrumentation module or
  by the `on_load` and `pre_fork` hooks, has no target.

- **3.15** and **UNWRAPT** say whether each way of deferring can defer the
  import's form: `from x import y` defers on Python 3.15 alone, and below
  it only once rewritten as `import x`. A module first loaded by a `from`
  import is counted, with its time, like any other. Its SAVES MS is 0,
  since unwrapt cannot defer it, and the advice names it only when it is
  in the project's own code and its load takes at least 10 ms, with the
  rewrite it needs below 3.15. A `from` import in a package the project
  depends on is never advised.

An import that takes less than half a millisecond is not kept as a
candidate.

The options narrow it:

| Option | Keeps |
| --- | --- |
| `--target TARGET` | The imports made while loading one target, or `none` for those outside any |
| `--top N` | The first N rows of each table (20 by default) |
| `--min-ms MS` | Rows that took at least MS milliseconds |
| `--package NAME` | Modules and importers under a top-level package; repeatable |
| `--module NAME` | The chain of imports that brought one module in, outermost first, with the best place to cut it |
| `--worker N` | A worker's own report, when it loaded the application itself, with `preload = false` or after the fallback to per-worker imports |
| `--json` | The same as JSON |

### The workers

After the report, `muructl debug imports` asks each worker which modules
it has loaded for the first time since the fork, as `muructl debug
status` asks for its status:

```
Modules the 4 workers asked loaded for the first time since the fork
  MODULE          LOADED BY FIRST REQUEST               MS ADVICE
  shop.export     4 of 4    01M7KQB0T2RJAQ4CG18KYPG80D 41.0 every worker loads it: load it before the fork with preload_modules = ["shop.export"]
  pandas (deferred) 1 of 4  01M7KQC4YV3M8T5WQ0J1E9X2HB 310.2 deferral pays off: only some workers load it
  matplotlib (deferred in shop.charts): never needed: deferring it saves it in every worker
```

Each row is a module a worker loaded while it served, by how many of the
workers asked, the first request that loaded it and its time, with
deferred modules marked. A module every worker loads is better loaded
before the fork; one only some of them load pays off loaded on first use,
or deferred; and a deferred import of the host's load that no worker has
used is never needed. The figures grow as the workers serve, so read them
after the application has seen representative traffic. A module loaded
only inside another's load comes with it and is not listed.

### Checking a change

`muructl check --imports` records one load of the application and prints
its report, without serving anything, whatever `import_report` says, and
works in CI. Without `--imports`, `muructl check` prints the report when
`import_report` is on. Run it before and after adding a deferral to see
what it saved.

A coding agent reads the same report through the `imports` tool of
[`muructl mcp serve`](coding-agents.md), can read the code at each import
the advice names, and can run `muructl check --imports` again to confirm
a change.

### Where the report is kept

The daemon keeps each release's report in its state, beside the release,
and removes it with the release. `muructl run` and `muructl serve` keep
the reports of their last four loads in memory, so `muructl debug imports
APP@RELEASE` reads an earlier load's report of a run that has reloaded.
The host sends the report once its supervisor connects, so a load that
fails, or one `preload = true` refuses, has no report kept;
`muructl check --imports` shows the report of such a load.

### In events and metrics

The host's load is a `release.loaded` event, which with `import_report`
on gives the report's headline figures:

```
release.loaded  shop@r3  the host loaded the application in 520 ms before the fork; importing took 412 ms over 213 modules, and deferring what can be deferred would save about 120 ms
```

Its time is the `muru_release_load_seconds` metric whether or not the
report is on, and `muru_worker_modules_loaded_total` and
`muru_worker_module_load_seconds_total` count what each worker has
loaded since the fork, so a deploy that made the load slower, or workers
that keep loading code, can be graphed and alerted on. The
[metrics guide](metrics.md) lists them.

### What it costs

Recording costs the load time, and analysing the report when the load is
done costs more. Measured on Python 3.15 on macOS, the median of 21 fresh
processes loading Django with its admin and contrib applications and
FastAPI, 801 modules and 4,061 import calls recorded: the load took 152
ms without the recorder and 179 ms with it, and the analysis 81 ms more.
The report was 2.4 MB of JSON. Under preload the time is taken before
the release is sent traffic, but `muructl run` takes it again on every
reload, so `import_report` is for the runs that look for what to defer
rather than one to leave on.

## Beside `preload_modules`

[`preload_modules`](../explanation/preload.md#modules-loaded-on-first-use)
works the other way: it loads before the fork a module the application
imports only on first use, so no worker loads it on a request.
`lazy_imports` takes a module the application imports as it loads out of
the host's load. A module that is deferred and also listed in
`preload_modules` is loaded before the fork, as the list says, and the
deferred import finds it loaded when it is first used.
