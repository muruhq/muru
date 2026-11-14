# Instrumentation modules

An instrumentation module sets up a tool, such as an import timer or an
APM agent, for a release without any change to the application or its
[hooks module](../reference/python-api.md#lifecycle-hooks). Muru imports
the instrumentation modules before anything of the application, in the
order they are listed, and calls functions they define at points in the
release's life.

## Listing the modules

Each entry names a module and, optionally, settings for it. In a
`pyproject.toml`:

```toml
[tool.muru]
entrypoint = "shop.wsgi:application"
hooks = "shop.hooks"

[[tool.muru.instrumentation]]
module = "importtimer"

[tool.muru.instrumentation.settings]
output = "/tmp/import-times.json"

[[tool.muru.instrumentation]]
module = "newrelic_setup"
settings = { app_name = "shop", shutdown_timeout = 3 }
```

A `[tool.muru.instrumentation.settings]` table belongs to the entry
above it. In a `muru.toml`, the same entries are `[[instrumentation]]`
and `[instrumentation.settings]`.

- **muru.local.toml.** `[[instrumentation]]` entries in a project's
  `muru.local.toml` apply to `muructl run` on that machine and come
  before the project's own, so a timer one developer adds also measures
  the other modules.

- **A declared release.** `instrumentation` in a release's table for
  `muructl apply` replaces the list of the release's source when it
  gives one.

- **Deploys.** A deploy takes the list from the source's `[tool.muru]`
  table, never from the release before it, so a source that drops an
  entry drops it.

- **Checking.** `muructl run --print-config`, `muructl serve
  --print-config`, `muructl deploy --dry-run` and `muructl config
  export` show the list.

A module may be listed once. `module` is a dotted Python module name.

## What a module defines

A module defines any of the functions below at module level. Muru finds
them by name; nothing in the configuration says which a module has.

| Function | Called |
| --- | --- |
| `configure(settings)` | Once, straight after the module is imported, with a `dict` of its entry's settings, empty when the entry has none |
| `load_started(context)` | Straight after `configure`, before the next module is imported and before anything of the application |
| `before_import(context, target)` | Before the application's hooks module, its entrypoint and each mount is imported |
| `after_import(context, target)` | After each of those imports, with `target.error` set when it failed |
| `load_finished(context, outcome)` | Once the application is loaded, or its load has failed, before any worker is forked |
| `wrap_application(context, target, application, interface)` | In each worker, once for each entry point, with its application, before the worker serves; what it returns is served in place of the application |

A module sees only its own settings, and the application sees none of
them.

```python
# importtimer.py
output = None


def configure(settings):
    global output
    output = settings.get("output", "import-times.json")
```

## Timing the load

`load_started`, `before_import`, `after_import` and `load_finished` let
a tool follow the application's load, for example to time its imports:

```python
# importtimer.py
import json
import os
import sys
import time

timings = {}
current = None


class Timer:
    def find_spec(self, name, path, target=None):
        ...  # time each module's load, charged to `current`


def load_started(context):
    sys.meta_path.insert(0, Timer())


def before_import(context, target):
    global current
    current = target.name or target.kind


def after_import(context, target):
    global current
    current = None


def load_finished(context, outcome):
    sys.meta_path[:] = [finder for finder in sys.meta_path if not isinstance(finder, Timer)]

    with open(f"import-times-{os.getpid()}.json", "w") as output:
        json.dump(timings, output)
```

- **Where the load runs.** Under preload the host runs the sequence
  once, before any worker is forked. With per-worker imports each worker
  runs it before its `post_fork` hook, loading only its own group's
  mounts. After the fallback to per-worker imports, it runs once in the
  host and again in each worker. `muructl check` runs it too, which
  gives import timings without serving anything.

- **What each pair covers.** `before_import` and `after_import` bracket
  the hooks module (`target.kind` is `hooks`), the entrypoint
  (`entrypoint`) and each mount (`mount`, with `target.name`), and a
  factory the host calls falls inside the entrypoint's pair. What
  `on_load` and `pre_fork` import falls outside any pair. `before_import`
  runs in listed order and `after_import` in reverse, so the pairs of
  several modules nest.

- **load_finished.** It runs in reverse order after `pre_fork` and the
  import check, once on every way out, and `outcome` says which:

  | `outcome` | What happens next |
  | --- | --- |
  | `fork` | The host forks the workers from what it loaded |
  | `serve` | This process serves what it loaded: a worker with per-worker imports, or `--single` |
  | `fallback` | The host starts again with per-worker imports |
  | `refused` | `preload = true` refused what the import check found, and the release fails |
  | `failed` | An import, a factory the host calls, `on_load` or `pre_fork` failed, or an instrumentation module could not be imported |
  | `check` | `muructl check` is done |

  Nothing is turned off for a module there: a timer removes itself, and
  an agent can leave its import hooks in place for the workers to
  inherit.

- **Writing an import timer.** Wrap `sys.meta_path` or the loaders, not
  `builtins.__import__`: the entrypoint, the hooks module and the first
  load of `from pkg import sub` are imported through
  `importlib.import_module`, which does not call `__import__`. A module
  that several entry points use is charged to the first one that loads
  it, and modules Muru's host imported before the application cost
  nothing. A file a timer writes needs a name that is unique to its
  process, since the host and each worker may write one. Under preload
  the import check also does a little work around each first import,
  which shows in the importing module's own time.

## Wrapping an entry point's application

`wrap_application` receives each entry point's application once in
each worker: the entrypoint's, and each mount's. It runs after a
factory has been called, wherever the factory runs, so it always gets
the application itself, and before the event loop and ASGI lifespan
start, so lifespan goes through the wrapper too.

```python
# apm_setup.py
import someapm


def wrap_application(context, target, application, interface):
    if interface == "wsgi":
        return someapm.WSGIMiddleware(application)

    if interface == "asgi":
        return someapm.ASGIMiddleware(application)

    return None  # served unchanged
```

- **target** says which entry point: `target.kind` is `entrypoint` or
  `mount`, `target.name` is the mount's name (empty for the
  entrypoint), and `target.spec` is its `module:callable`.

- **interface** is `wsgi`, `asgi` or `grpc`, as Muru settled it for that
  entry point. `context.interface` describes only the entrypoint, so a
  wrapper for a mount goes by `interface`.

- **The interface stays.** Muru does not look at the wrapper to decide
  the interface again, so a wrapper for an ASGI application has to be
  an ASGI application, and one for WSGI a WSGI callable.

- **Several modules.** The last module listed wraps first, so the first
  listed is outermost.

- **None or an exception.** Returning `None` serves the application
  unchanged. An exception is logged, and the application is served as
  it was before that module's call.

- **gRPC.** For a gRPC entry point, `application` is Muru's gRPC server
  with the services registered, which is not a callable. The call is
  still made, so a tool hears of every entry point, but anything other
  than `None` or the same object is logged and not served.

## When the modules are imported

Each module is imported and configured before the next one is
imported, so an earlier module can measure a later one's import.

- **Preload.** The host imports the modules before the application's
  hooks module and entrypoint, and before the import check takes its
  baseline, so what a module opens or starts as it is imported is not
  reported by the check. A module that leaves a thread or a socket open
  in the host has to close it before the workers are forked.

- **Per-worker imports.** With `preload = false`, each worker imports
  the modules before the application. After the fallback to per-worker
  imports, the host has imported them once and each worker imports them
  again.

- **muructl check.** `muructl check` imports and configures the modules
  as the host does, and lists the functions each defines.

## When something fails

- A module that cannot be imported stops the release with the error in
  the log, as an entrypoint that cannot be imported does.

- An exception from one of a module's functions, `configure` included,
  is logged with its traceback and the release carries on.

- An attribute with one of these names that is not a function is
  logged and left out.

## Writing and installing a module

- **Keep it outside the application's package.** Importing
  `shop.timer` runs `shop/__init__.py` first, which may import much of
  the application before the timer is in place. A top-level module or a
  package of its own is imported alone.

- **Install its packages through a dependency group.** Put the tool's
  packages in a dependency group of the project and
  name the group in `[tool.muru.build]`:

  ```toml
  [dependency-groups]
  observability = ["newrelic"]

  [tool.muru.build]
  groups = ["observability"]
  ```

  The [deployment environment](interpreters.md#building-the-environment)
  then installs them.

- **Keep credentials out of the settings.** Settings are configuration
  that exports and dry runs show. A licence key or a token goes in
  `[tool.muru.env]`, as a [secret reference](secrets.md#references)
  under the daemon, and the module reads it from the environment.
