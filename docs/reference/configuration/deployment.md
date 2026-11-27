<!-- Generated from crates/muru-config by tests/reference.rs; do not edit. -->

# Applied files

The keys of the files `muructl apply` reads, which say what the daemon serves: applications and their releases, hosts, routes, upstreams and overlays, with the client classes, rate limits and conditions that belong to them. A file of this kind kept beside a project's code is named `muru.deploy.toml`, since `muru.toml` there is the project's settings, which have other keys. A table of the daemon's own configuration here, such as `[[listener]]` or `[limits]`, is refused. A client class, rate limit or condition may not have the name of one the daemon's own file defines. A relative path is taken from the file's directory, except a release's own static directories and error pages, which are taken from its source directory; apply sends the daemon the absolute paths.

The JSON Schema of the same keys, for an editor that checks TOML against one, is `https://docs.muruhq.dev/en/latest/schema/deployment.schema.json`.

(config-deployment)=
## Top level

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | table of tables, {ref}`[app.NAME] <config-deployment-app-name>` |  | Applications by name. |
| `client_class` | array of tables, {ref}`[[client_class]] <config-deployment-client-class>` |  | Client classification rules, in order, from the daemon's own file or an applied one. |
| `condition` | table of tables, {ref}`[condition.NAME] <config-deployment-condition-name>` |  | Named conditions over Muru's own signals, from the daemon's own file or an applied one. |
| `deployment` | string | `"default"` | The deployment the file's tables belong to: a named set of applications, hosts, routes and the rest that is applied, pruned and removed together. Files with the same name are one deployment. A key of applied files alone. |
| `host` | table of tables, {ref}`[host.NAME] <config-deployment-host-name>` |  | Virtual hosts by name. |
| `overlay` | table of tables, {ref}`[overlay.NAME] <config-deployment-overlay-name>` |  | Overlays by name: partial configuration laid over the routes and applications while active. |
| `rate_limit` | table of tables, {ref}`[rate_limit.NAME] <config-deployment-rate-limit-name>` |  | Rate limits by name, from the daemon's own file or an applied one. |
| `route` | array of tables, {ref}`[[route]] <config-deployment-route>` |  | Routes, in the order written. |
| `upstream` | table of tables, {ref}`[upstream.NAME] <config-deployment-upstream-name>` |  | HTTP upstreams by name. |

(config-deployment-app-name)=
## `[app.NAME]`

An application's table and its releases.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `access_log` | string | the `[logs]` table's | Which of the application's requests the access log writes, over the `[logs]` table's choice: `on`, `errors` or `off`. |
| `access_log_exclude` | array of strings |  | Paths whose requests to the application the access log leaves out, besides those the `[logs]` table names. |
| `concurrency` | integer | the source's, else 1000 | Requests an ASGI worker's event loop takes at once, for every release, over the source's. |
| `data_dir` | string | STATE/data/APP | The application's data directory, shared by every release, an absolute path; its media and uploads parts are made in it. |
| `env` | table of strings or tables, {ref}`[app.NAME.env.NAME] <config-deployment-app-name-env-name>` |  | Environment variables every release deployed after this is applied takes, over the source's `[tool.muru.env]` and under the release's own; a secret is given by reference, never by value. |
| `group` | string | the user's primary group | The group, else the user's primary group. |
| `max_lifetime` | string | the source's, else never | How long a worker lives before it is replaced, for every release, over the source's. |
| `max_requests` | integer | the source's, else never | Requests a worker serves before it is replaced, for every release, over the source's. |
| `max_surge` | integer | the source's, else 1 | How many workers a group may run over its count while workers are swapped, for every release, over the source's. |
| `max_unavailable` | integer | the source's, else 0 | How many workers a group may run under its count while workers are swapped, for every release, over the source's. |
| `media_dir` | string | `"media"` | The media part, the one part ever served, a relative one taken from `data_dir`. |
| `memory` | table, {ref}`[app.NAME.memory] <config-deployment-app-name-memory>` |  | The memory policy for every release, each key over the source's. |
| `priority` | integer | `0` | Which applications' scale-ups the node's budget admits first, the highest first. |
| `processes` | integer | the source's, else 1 | Worker processes for every release, over the source's. |
| `release` | table of tables, {ref}`[app.NAME.release.ID] <config-deployment-app-name-release-id>` |  | Releases by identifier. |
| `schedule` | array of tables, {ref}`[[app.NAME.schedule]] <config-deployment-app-name-schedule>` |  | Scheduled capacity. |
| `serving` | string | the one release an apply adds, else the one that serves | The release that serves the application's traffic, a declared one or one the daemon has. The other releases an apply adds are started held, and naming a release the daemon has promotes it. An apply that adds more than one release of the application needs it. |
| `slow` | string | the `[logs]` table's | How slow one of the application's requests is to be written under `access_log = "errors"`. |
| `sync_threads` | integer | the source's, else 8 | Threads for synchronous code called from an ASGI application, for every release, over the source's. |
| `threads` | integer | the source's, else 4 | Application threads per worker for every release, over the source's. |
| `timeouts` | table, {ref}`[app.NAME.timeouts] <config-deployment-app-name-timeouts>` |  | The host's timeouts for every release, each over the source's. |
| `umask` | string | the daemon's own umask | The file creation mask, in octal, for example 027. |
| `uploads_dir` | string | `"uploads"` | The uploads part, never served, a relative one taken from `data_dir`. |
| `user` | string | the daemon's own user | The user the application's hosts and workers run as, when the daemon runs as root. |

(config-deployment-app-name-env-name)=
## `[app.NAME.env.NAME]`

A secret reference in an `[env]` table.

The same keys apply at `[app.NAME.release.ID.env.NAME]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `as` | string | `"env"` | How the value is delivered: `env` as the variable, or `file` as a file only the release's user can read, whose path is in `NAME_FILE`. |
| `secret` | table, {ref}`[app.NAME.env.NAME.secret] <config-deployment-app-name-env-name-secret>` | required | The store and key. |

(config-deployment-app-name-env-name-secret)=
## `[app.NAME.env.NAME.secret]`

Which secret: a store declared in the daemon's file, and a key in it.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `key` | string | required | The key within the store. |
| `store` | string | required | The store's name. |

(config-deployment-app-name-memory)=
## `[app.NAME.memory]`

The `[memory]` table of a release or a `[tool.muru]` table: the limits a worker's memory is held to.

The same keys apply at `[app.NAME.release.ID.memory]` and `[app.NAME.release.ID.groups.NAME.memory]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `check_interval` | string | `"5s"` | How often memory is measured. |
| `hard_limit` | string | off; under `muructl serve` with a memory limit, a share of it | Over this the worker leaves routing at once and is replaced, for example 1200MB. |
| `measure` | string | `"uss"` | What is measured: uss (the default), pss or rss. `uss`: Unique set size: pages no other process shares, the default, since preloaded pages are shared copy-on-write. `pss`: Proportional set size: shared pages counted in proportion. `rss`: Resident set size: every resident page. |
| `reclaim_first` | boolean | `true` | Whether to collect garbage and trim the heap before recycling over the soft limit. |
| `soft_for` | string | `"2m"` | How long the worker may stay over the soft limit. |
| `soft_limit` | string |  | Over this for soft_for, the worker reclaims and then recycles, for example 800MB; off when absent. |

(config-deployment-app-name-release-id)=
## `[app.NAME.release.ID]`

A release definition, the `[app.NAME.release.ID]` table: the keys a deploy takes, which `muructl apply` reads. What a table leaves out comes from the `[tool.muru]` table of its source.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `accel_redirect` | boolean | `false` | Treat an `X-Accel-Redirect` response header as an internal redirect, for the default mount and any mount that does not say otherwise. |
| `bind` | string | `"unix"` | How an `http` release's instances bind: `unix` (the default) or `port`. `unix`: A Unix socket per instance under the release's run directory. `port`: A free loopback port per instance. |
| `build` | table, {ref}`[app.NAME.release.ID.build] <config-deployment-app-name-release-id-build>` |  | When and how the environment is built on the node. |
| `bytecode` | string | `"redirect"` | Where compiled bytecode goes. `redirect`: A writable directory of the release's own under the state directory, so nothing lands in the source tree (the default). `source`: `__pycache__` beside the source, as Python does by default; what a release with precompiled bytecode uses. `off`: No bytecode is written. |
| `cgi_local_redirect` | boolean | `false` | Treat a 200 response with a `Location` starting with `/` and no body as an internal redirect, the CGI convention. |
| `command` | array of strings |  | The command an `http` release runs, with `{bind}` where the server takes its address; without `{bind}` the profile adds the server's own flags. |
| `commands` | table of tables, {ref}`[app.NAME.release.ID.commands.NAME] <config-deployment-app-name-release-id-commands-name>` |  | Commands the application declares, by name: run with the release on request, with inputs. |
| `concurrency` | integer | `1000` | Requests an ASGI worker's event loop takes at once. |
| `cwd` | string | `source` | The directory an `http` release's command runs in; `source` when absent, and one of the two is required. A relative path is taken from the file's directory. |
| `entrypoint` | string | the source's `[tool.muru]` | The application as module:callable, else from the source's `[tool.muru]` table. |
| `env` | table of strings or tables, {ref}`[app.NAME.env.NAME] <config-deployment-app-name-env-name>` |  | Environment variables for the workers: a value, or a secret reference resolved when the release starts. |
| `error_override` | table, {ref}`[host.NAME.error_override] <config-deployment-host-name-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[host.NAME.error_pages.STATUS] <config-deployment-host-name-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `factory` | boolean | `false` | Whether the entrypoint is a factory. |
| `factory_phase` | string | `"worker"` | When a factory is called. `worker`: In each worker, after the post_fork hook. `host`: Once in the host, during preload. |
| `groups` | table of tables, {ref}`[app.NAME.release.ID.groups.NAME] <config-deployment-app-name-release-id-groups-name>` |  | Worker groups by name. |
| `grpc_web_origins` | array of strings |  | The origins whose pages may call the release's gRPC methods through gRPC-Web, answered with CORS: scheme, host and port, or `*`; none when empty. |
| `handoff` | string |  | The interface the default mount's requests may be handed to; none when absent. `asgi`: Continue the request as an ASGI application on the event loop. `wsgi`: Continue the request as a WSGI callable on the thread pool. |
| `hooks` | string |  | The module holding the lifecycle hooks, as a Python module name. |
| `import_report` | boolean | the source's | Record the host's imports for `muructl debug imports`; when given, it takes the place of the source's `import_report`. |
| `instances` | integer | `1` | How many instances of the command an `http` release runs. |
| `instrumentation` | array of tables, {ref}`[[app.NAME.release.ID.instrumentation]] <config-deployment-app-name-release-id-instrumentation>` |  | The instrumentation modules, in order; when given, they replace the list in the source's `[tool.muru]` table. |
| `interface` | string | `"auto"` | The interface, when declared rather than detected. `auto`: Detect from the callable: a coroutine function or an object with an `async def __call__` is ASGI, anything else WSGI. `wsgi`: PEP 3333. `asgi`: ASGI 3. `grpc`: gRPC services: the entrypoint is a registration function given a server to add grpcio servicers to. |
| `kind` | string | `"workers"` | What the release runs: `workers` from an entrypoint, the default, or `http`, an HTTP server started from `command`. `workers`: Muru's own workers, from an entrypoint. `http`: An HTTP server Muru starts and supervises, from a command. |
| `lazy_imports` | boolean | the source's | Defer imports with unwrapt; when given, it takes the place of the source's `lazy_imports`. |
| `logging` | table, {ref}`[app.NAME.release.ID.logging] <config-deployment-app-name-release-id-logging>` |  | How Python logging is set up in the workers. |
| `maintenance` | table, {ref}`[host.NAME.maintenance] <config-deployment-host-name-maintenance>` |  | What maintenance mode lets through. |
| `max_body_size` | string | the host's, else `max_body_size` in `[limits]` | The largest body of a request the release serves, for example 50MB, which the edge enforces. |
| `max_lifetime` | string |  | How long a worker lives before it is replaced, for example 1h, with jitter; off when absent. |
| `max_requests` | integer |  | Requests a worker serves before it is replaced, with jitter; off when absent. |
| `max_surge` | integer | `1` | How many workers a group may run over its count while workers are swapped: recycled, or restarted on request. A replacement is started before the old worker stops while the group is within it; with 0 the old worker stops first. |
| `max_unavailable` | integer | `0` | How many workers a group may run under its count while workers are swapped. Both this and `max_surge` 0 is refused. |
| `memory` | table, {ref}`[app.NAME.memory] <config-deployment-app-name-memory>` |  | The memory policy for the workers. |
| `metadata` | table of strings |  | What the deploy recorded about where the release's code came from: string keys to string values, `commit`, `ref`, `url`, `description` and `dirty` among them, which the release keeps and never gives the application. At most 32 keys. |
| `mounts` | table of tables, {ref}`[app.NAME.release.ID.mounts.NAME] <config-deployment-app-name-release-id-mounts-name>` |  | Mounts by name. |
| `preload` | string | `"auto"` | Whether the application is imported once, in the host, before the workers are forked from it. `auto`: Preload when the import check passes, otherwise import per worker. `true`: Preload, and fail the release when the import check does not pass. `false`: Import in every worker. |
| `preload_modules` | array of strings |  | Modules the host imports after the entrypoint and the mounts, so they are loaded once before the fork; when given, they replace the list in the source's `[tool.muru]` table. |
| `processes` | integer | `1` | The default group's worker processes. |
| `profile` | string |  | The server an `http` release runs: gunicorn, uvicorn, granian, mod_wsgi-express, uwsgi or generic. `gunicorn`: gunicorn: `--bind unix:PATH` or `--bind HOST:PORT`, stopped by TERM. `uvicorn`: uvicorn: `--uds PATH` or `--host HOST --port PORT`, stopped by TERM. `granian`: Granian: `--uds PATH` or `--host HOST --port PORT`, stopped by TERM. `mod_wsgi-express`: mod_wsgi-express: `--host HOST --port PORT`, stopped by TERM. `uwsgi`: uWSGI: `--http-socket PATH` or `--http-socket HOST:PORT`, with `--master` and a hook that makes TERM a graceful stop, which in uWSGI 2.0 would otherwise reload. `generic`: Any server: the command binds through `{bind}` and `stop_signal` says how it is stopped. |
| `python` | string | found at start | The interpreter: a path, or a request in uv's syntax. A relative path with a directory in it, such as `.venv/bin/python`, is taken from the file's directory. |
| `readiness` | table, {ref}`[app.NAME.release.ID.readiness] <config-deployment-app-name-release-id-readiness>` |  | How an `http` release's instances are known to be ready. |
| `reflection` | boolean | `false` | Serve gRPC server reflection for the services the release's gRPC mounts register (default false). |
| `scaling` | table, {ref}`[app.NAME.release.ID.scaling] <config-deployment-app-name-release-id-scaling>` |  | How the default group is scaled; static when absent. |
| `sendfile_header` | string |  | A second response header honoured as `X-Muru-Sendfile` is, such as `X-Sendfile` for the frameworks that send that one. |
| `sendfile_roots` | array of strings |  | Directories a path in an `X-Muru-Sendfile` response header must lie in for the file to be served, for the default mount and any mount that does not say otherwise; with none every hint is refused. `source:SUBDIR`, or a relative path, is taken from the release's `source`, and `media:SUBDIR` from the media part. |
| `source` | string |  | The source directory. A relative path is taken from the file's directory. Required by `muructl apply` for a `workers` release. |
| `static` | array of tables, {ref}`[[app.NAME.release.ID.static]] <config-deployment-app-name-release-id-static>` |  | The application's own static directories, served by the edge from the release's source, which is the project directory under `muructl run`. |
| `stop_signal` | string | the profile's, `TERM` | The signal that stops an `http` release's instance gracefully: TERM, INT, QUIT, HUP, USR1, USR2 or WINCH. Required by the generic profile; the other profiles know their server's. |
| `sync_threads` | integer | `8` | Threads for synchronous code called from an ASGI application. |
| `threads` | integer | `4` | Application threads per worker. |
| `timeouts` | table, {ref}`[app.NAME.release.ID.timeouts] <config-deployment-app-name-release-id-timeouts>` |  | The timeouts the host enforces on its workers, and those of a request the release serves, which the edge enforces. |

(config-deployment-app-name-release-id-build)=
## `[app.NAME.release.ID.build]`

The `[tool.muru.build]` table: when and how the release's environment is built from the lock. `when` is read by `muructl run` and the daemon alike; the rest describes the deployment flavour, which development ignores.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `compile_bytecode` | boolean | `true` | Compile bytecode at build time. |
| `extras` | array of strings |  | Extras the deployment environment includes. |
| `groups` | array of strings |  | Dependency groups beyond the defaults; dev is never included. |
| `source_builds` | boolean | `false` | Allow sdists to be built on the node. |
| `when` | string | `auto`; `never` under `muructl run --allow-public` | When an environment is built: `auto` builds a missing one and leaves an existing one alone, `always` builds on every start and on a lock change, `never` builds nothing. `muructl serve` never builds one. `auto`: Build a missing environment; leave an existing one alone at start, and under `muructl run` build when the lock changes. `always`: Build before every start too. `never`: Build nothing. |

(config-deployment-app-name-release-id-commands-name)=
## `[app.NAME.release.ID.commands.NAME]`

A command an application declares, the `[tool.muru.commands.NAME]` or `[app.NAME.release.ID.commands.NAME]` table: a program to run, or a Python callable to call, with the release.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `before_fork` | boolean | `false` | A gate: the release's start runs the command once the host has preloaded and before it forks workers, and fails if it fails. |
| `before_promote` | boolean | `false` | A gate: a rollout's or blue/green promotion runs the command just before it moves all traffic to the release, and stops if it fails. |
| `call` | string |  | A callable as module:function, called with the inputs as keyword arguments, instead of `run`. |
| `description` | string |  | What the command does, for `muructl command list`. |
| `env` | table of strings |  | Environment variables for this command alone, over the release's, with `{name}` where an input's value goes. |
| `every_start` | boolean | `false` | Run a `before_fork` gate at every start of the release's host, not only until it has passed once. |
| `exclusive` | boolean | `false` | Run one at a time for the application: a run asked for while one is under way is refused. |
| `inputs` | table of tables, {ref}`[app.NAME.release.ID.commands.NAME.inputs.NAME] <config-deployment-app-name-release-id-commands-name-inputs-name>` |  | The values the command takes, by name. |
| `run` | array of strings |  | The program and its arguments, run without a shell, with `{name}` where an input's value goes; `python` is the release's interpreter. |
| `schedule` | table, {ref}`[app.NAME.release.ID.commands.NAME.schedule] <config-deployment-app-name-release-id-commands-name-schedule>` |  | When the daemon runs the command by itself, with the application's current release: by a cron expression, when the daemon starts from nothing, or both. A command with a schedule needs a `timeout`. |
| `timeout` | string |  | How long it may run before it is stopped, for example 10m; no limit when absent. |

(config-deployment-app-name-release-id-commands-name-inputs-name)=
## `[app.NAME.release.ID.commands.NAME.inputs.NAME]`

An input a command takes.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `default` | string |  | The value when none is given. |
| `description` | string |  | What the value is, for `muructl command list` and prompts. |
| `pattern` | string |  | A regular expression the whole value must match. |
| `required` | boolean | `false` | The command refuses to run without it. |
| `secret` | boolean | `false` | The value is never shown, logged or put in an event, and may only be used in `env`. |

(config-deployment-app-name-release-id-commands-name-schedule)=
## `[app.NAME.release.ID.commands.NAME.schedule]`

When the daemon runs a command by itself.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `at_boot` | boolean | `false` | Run the command when the daemon starts from nothing: its first start, after a reboot or after a shutdown, but not when it takes over what a daemon left running. |
| `cron` | string |  | A cron expression, five fields or @daily and the like, as an overlay schedule's, read in `timezone`. |
| `inputs` | table of strings |  | Input values a scheduled run is given, by name; the others take their defaults. |
| `secret_inputs` | table of strings |  | Inputs taken from one of the daemon's secret stores, by name, each as STORE/KEY. |
| `timezone` | string | the system's | The IANA time zone `cron` is read in. |

(config-deployment-app-name-release-id-groups-name)=
## `[app.NAME.release.ID.groups.NAME]`

A worker group, the `[app.NAME.release.ID.groups.NAME]` or `[tool.muru.groups.NAME]` table: the paths it serves and its own sizes. Paths not claimed by a group go to the default group, which is the release's own settings.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `concurrency` | integer | the release's | Requests an ASGI worker's event loop takes at once. |
| `match` | table, {ref}`[app.NAME.release.ID.groups.NAME.match] <config-deployment-app-name-release-id-groups-name-match>` |  | What the group asks of a request beside its path: the client classes, hosts and methods it takes, each list any of its entries; a group asking more comes before one asking less, and a group with lists and no `paths` takes every path. |
| `max_body_size` | string | the release's | The largest body of a request the group serves, for example 200MB, which the edge enforces. |
| `max_lifetime` | string | the release's | How long a worker of the group lives before it is replaced, for example 1h, with jitter. |
| `max_requests` | integer | the release's | Requests a worker of the group serves before it is replaced, with jitter. |
| `max_surge` | integer | the release's | How many workers the group may run over its count while workers are swapped. |
| `max_unavailable` | integer | the release's | How many workers the group may run under its count while workers are swapped. |
| `memory` | table, {ref}`[app.NAME.memory] <config-deployment-app-name-memory>` |  | The group's own memory policy, each key the release's when absent. |
| `mounts` | array of strings |  | The named mounts the group's workers serve, beside the default mount, which every worker serves; every mount when absent. The group's paths must lie within them. |
| `optional` | boolean | `false` | Whether the release is ready without the group's workers: its requests wait until one listens. |
| `paths` | array of strings |  | Path prefixes, with an optional trailing `*`; the most specific match across all groups wins. |
| `processes` | integer | the release's, or `scaling.min_workers` when higher | The group's worker processes. |
| `scaling` | table, {ref}`[app.NAME.release.ID.scaling] <config-deployment-app-name-release-id-scaling>` |  | How the group is scaled; static when absent. |
| `sync_threads` | integer | the release's | Threads for synchronous code called from an ASGI application. |
| `threads` | integer | the release's | Application threads per worker. |
| `timeouts` | table, {ref}`[app.NAME.release.ID.timeouts] <config-deployment-app-name-release-id-timeouts>` |  | The group's own timeouts, each the release's when absent; `shutdown_timeout`, `websocket_drain_grace` and `exit_grace` belong to the release. |

(config-deployment-app-name-release-id-groups-name-match)=
## `[app.NAME.release.ID.groups.NAME.match]`

The requests a worker group takes for its paths: every list given must name the request's class, host or method.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `client_class` | array of strings |  | Client classes. |
| `host` | array of strings |  | Hosts: a virtual host's name or a name it answers to, or under `muructl run` the name the request carries. |
| `method` | array of strings |  | Methods, compared in upper case. |

(config-deployment-app-name-release-id-instrumentation)=
## `[[app.NAME.release.ID.instrumentation]]`

One instrumentation module of a release: a tool such as an import timer or an APM agent, imported before the application and set up apart from it, with settings that only it is given.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `module` | string | required | The module, as a dotted Python module name. |
| `settings` | table |  | The settings its `configure` function is given, as a table of any values. |

(config-deployment-app-name-release-id-logging)=
## `[app.NAME.release.ID.logging]`

The `[logging]` table: how Python logging is set up in the workers.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `python` | table, {ref}`[app.NAME.release.ID.logging.python] <config-deployment-app-name-release-id-logging-python>` |  | Python logging defaults. |

(config-deployment-app-name-release-id-logging-python)=
## `[app.NAME.release.ID.logging.python]`

The `[logging.python]` table: what loggers without handlers of their own do, set before the application is imported.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `capture_warnings` | boolean | `false` | Whether warnings.warn goes through the py.warnings logger. |
| `format` | string | `standard`; `json` under `muructl serve --log-format json` | The format of the platform handler: standard, json, or a logging format string. |
| `level` | string | `"WARNING"` | The root logger's level. `DEBUG`: Everything. `INFO`: Informational messages and above. `WARNING`: Python's default: warnings and above. `ERROR`: Errors and above. `CRITICAL`: Only critical messages. |
| `loggers` | table of strings |  | Levels for named loggers. |
| `warnings` | array of strings |  | Warnings filters in the syntax of Python's -W option, `action:message:category:module:lineno`, applied in order with `warnings.filterwarnings` before the application is imported, so a later one takes precedence. |

(config-deployment-app-name-release-id-mounts-name)=
## `[app.NAME.release.ID.mounts.NAME]`

A mount, the `[app.NAME.release.ID.mounts.NAME]` or `[tool.muru.mounts.NAME]` table: a path served by another entrypoint in the same workers. The release's own entrypoint is the default mount at /.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `accel_redirect` | boolean | the release's | Treat an `X-Accel-Redirect` response header from the mount as an internal redirect. |
| `accept_redirects_from` | array of strings |  | Other applications whose internal redirects may reach the mount when it is internal; the release's own always may. |
| `cgi_local_redirect` | boolean | the release's | Treat a 200 response with a `Location` starting with `/` and no body as an internal redirect, the CGI convention. |
| `concurrency` | integer |  | Requests an ASGI mount runs at once on each worker, slots of its own at the edge; it shares the event loop's concurrency when absent. |
| `entrypoint` | string | required | The application as module:callable. |
| `handoff` | string |  | The interface the mount's requests may be handed to; none when absent. `asgi`: Continue the request as an ASGI application on the event loop. `wsgi`: Continue the request as a WSGI callable on the thread pool. |
| `interface` | string | `"auto"` | The interface, when declared rather than detected. `auto`: Detect from the callable: a coroutine function or an object with an `async def __call__` is ASGI, anything else WSGI. `wsgi`: PEP 3333. `asgi`: ASGI 3. `grpc`: gRPC services: the entrypoint is a registration function given a server to add grpcio servicers to. |
| `internal` | boolean | `false` | Reachable only through internal redirects: a client's request for its path is answered 404. |
| `max_body_size` | string | the worker group's, else the release's | The largest body of a request the mount serves, for example 200MB, which the edge enforces. |
| `max_connections` | integer |  | Open WebSockets an ASGI mount holds at once on each worker, apart from the requests it runs; each holds a request slot when absent. |
| `path` | string |  | The path prefix; the longest wins, on segment boundaries. A gRPC mount has none: its calls are matched by service name. |
| `sendfile_header` | string | the release's | A second response header honoured as `X-Muru-Sendfile` is, such as `X-Sendfile`. |
| `sendfile_roots` | array of strings | the release's | Directories a path in an `X-Muru-Sendfile` response header must lie in for the file to be served; the release's when absent, and an empty list refuses every hint. `source:SUBDIR`, or a relative path, is taken from the release's source directory, and `media:SUBDIR` from the media part. |
| `threads` | integer or table, {ref}`[app.NAME.release.ID.mounts.NAME.threads] <config-deployment-app-name-release-id-mounts-name-threads>` |  | Threads of the mount's own, for a WSGI or gRPC mount; it shares the worker's pool when absent. |
| `tracing_middleware` | boolean | `false` | Wrap the mount's application in wrapture's request middleware when tracing is on, so each request records as one tree. For an application no framework instrumentation records requests for. |

(config-deployment-app-name-release-id-mounts-name-threads)=
## `[app.NAME.release.ID.mounts.NAME.threads]`

The table form of a mount's threads.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `max` | integer | required | The most requests the mount runs at once, on its reserved threads and borrowed ones together. |
| `reserved` | integer | required | Threads only the mount uses. |

(config-deployment-app-name-release-id-readiness)=
## `[app.NAME.release.ID.readiness]`

The `readiness` table of a managed HTTP release: how an instance is known to be ready.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `path` | string |  | The path probed with a GET until it answers 2xx or 3xx; without it, any HTTP response makes the instance ready. |
| `timeout` | string | `"60s"` | How long the instances get to become ready. |

(config-deployment-app-name-release-id-scaling)=
## `[app.NAME.release.ID.scaling]`

The `scaling` table of a release, a worker group or a `[tool.muru]` table: the mode, the bounds, and the windows and budgets that keep the autoscaler from churning. The release's own table is the default group's.

The same keys apply at `[app.NAME.release.ID.groups.NAME.scaling]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `advisory` | boolean | `false` | Decide and report without acting. |
| `churn_budget` | integer | `6` | Reaps allowed per hour per release before scale-in is suspended. |
| `hold_after_growth` | string | `"5m"` | No scale-in this long after a scale-out. |
| `host_idle_timeout` | string |  | How long the release's host stays once every group has scaled to zero, after which it stops too and the next request starts it again; it stays while the release runs when omitted. In the release's own scaling table. |
| `idle_reap_after` | string | `"10m"` | A group wholly idle this long scales in without waiting for the scale-in window. |
| `max_spare` | integer | the larger of 3 and a quarter of `max_workers` | Spare mode: scale in while more idle workers than this remain. |
| `max_step_up` | integer | `4` | The most workers one scale-out adds. |
| `max_workers` | integer |  | The most workers; required by the spare and target modes. |
| `min_lifetime` | string | the larger of 15m and 30 times the time to ready | No worker younger than this is reaped. |
| `min_spare` | integer | `1` | Spare mode: scale out when fewer idle workers than this remain. |
| `min_workers` | integer | `1` | The fewest workers. |
| `mode` | string | `"static"` | How the group is scaled. `static`: A fixed worker count, the default. `spare`: Keep between min_spare and max_spare idle workers, Apache-style. `target`: Track a slot utilization and a queue wait target. |
| `queue_wait_p95` | string |  | Target mode: scale out while the p95 queue wait is over this, for example 100ms; off when absent. |
| `reap_interval` | string | `"60s"` | At most one worker reaped per this interval. |
| `scale_in_window` | string | `"10m"` | How long every scale-in condition must hold. |
| `scale_out_window` | string | `"5s"` | How long a scale-out condition must hold. |
| `slot_utilization` | number | `0.7` | Target mode: the share of slots to keep busy, between 0 and 1. |

(config-deployment-app-name-release-id-static)=
## `[[app.NAME.release.ID.static]]`

A directory of an application's own files, an entry of the `[[tool.muru.static]]` or `[[app.NAME.release.ID.static]]` array: a path prefix the edge serves from a directory under the release's source, with the application taking what no file answers. Served by `muructl run` and the daemon alike, from the release the request was routed to.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cache` | table, {ref}`[route.cache] <config-deployment-route-cache>` |  | The cache headers files are sent with. |
| `case` | string | `"strict"` | `strict` (the default), which matches names as they are on disk, or `insensitive`. |
| `dir` | string | required | The directory: `source:` for the release's source directory, `static:` for the release's built static files or `media:` for the application's media part, each with an optional directory inside it after the colon; or a path, absolute, or relative to the source directory as `source:` is. |
| `fallback` | string |  | The file served for paths with no file of their own, for a single-page application under the prefix. |
| `index` | array of strings | `["index.html"]` | The files tried, in order, when a directory is requested. |
| `path` | string | required | The path prefix, such as `/static/`; the longest wins, on segment boundaries, and `/` is not allowed, since every path would then be looked up as a file first. |
| `redirect_slash` | boolean | `true` | Whether a directory asked for without a trailing slash is redirected to the slash form. |

(config-deployment-app-name-release-id-timeouts)=
## `[app.NAME.release.ID.timeouts]`

The `[timeouts]` table of a release or a `[tool.muru]` table: the timeouts the host enforces on its workers, and the four the edge enforces on a request the release serves, which are otherwise the virtual host's or the daemon's `[limits]`.

The same keys apply at `[app.NAME.release.ID.groups.NAME.timeouts]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `body_idle_timeout` | string | the host's, else `body_idle_timeout` in `[limits]` | Time a client may pause while sending a body to what is served here. |
| `exit_grace` | string | `"2s"` | How long a worker has for its shutdown work (lifespan shutdown, the `on_exit` hook and `atexit` callbacks) after its requests have had `websocket_drain_grace` and `shutdown_timeout`, before it is killed. |
| `gil_stall_timeout` | string | `"30s"` | How long a worker may go with no thread running Python, or with no report to its host, before it is replaced as for the hard timeout. |
| `hard_drain_grace` | string | `"10s"` | How long a worker being replaced is given to finish its other requests before it is told to stop. |
| `hard_timeout` | string |  | How long a request may run in a worker before the worker is replaced, for example 60s; off when absent. Must exceed the soft timeout by at least 5s. |
| `loop_stall_timeout` | string | `"10s"` | How long an ASGI worker's event loop may go without running before the worker is replaced as for the hard timeout. |
| `queue_timeout` | string | the host's, else `queue_timeout` in `[limits]` | Time a request served here may wait for a worker, which the edge enforces; the virtual host's when absent, or else the daemon's `[limits]` value. |
| `request_deadline` | string | the host's, else `request_deadline` in `[limits]` | Time a request served here may take to produce its response head. |
| `response_idle_timeout` | string | the host's, else `response_idle_timeout` in `[limits]` | Time a worker here may pause mid-response. |
| `shutdown_timeout` | string | `"5s"` | How long a worker told to stop is given before it is killed, counted from when its WebSockets are closed. |
| `soft_timeout` | string |  | How long a request may run in a worker before it is interrupted by raising `muru.RequestTimeout` in its thread, for example 30s; off when absent. |
| `websocket_drain_grace` | string | `"0s"` | How long a worker told to stop keeps its open WebSockets before it closes them with 1001. |

(config-deployment-app-name-schedule)=
## `[[app.NAME.schedule]]`

An `[[app.NAME.schedule]]` entry: capacity for a window, shorthand for an overlay named `APP.NAME` that sets only the floor, the ceiling, how scale-in behaves and a deploy freeze.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cap_workers` | integer |  | At most this many workers during the window, whatever the policy and the autoscaler choose. |
| `cron` | string |  | A recurring window's start times. |
| `duration` | string |  | A recurring window's length. |
| `end` | string |  | A one-off window's end. |
| `freeze_deploys` | boolean | `false` | Refuse deploys and rollouts during the window. |
| `group` | string | `"default"` | The worker group; the default group without it. |
| `max_workers` | integer |  | The most workers during the window. |
| `min_workers` | integer |  | The fewest workers during the window. |
| `name` | string | required | The schedule's name, unique within the application. |
| `scale_in` | string | `"normal"` | `hold` keeps the group from scaling in during the window; `normal`, the default, leaves it to the autoscaler. |
| `start` | string |  | A one-off window's start, as for an overlay's schedule. |
| `timezone` | string | the daemon's system time zone | The time zone of local times. |
| `warm_up` | string |  | How long before each window the floor applies, such as 30m. |

(config-deployment-app-name-timeouts)=
## `[app.NAME.timeouts]`

The host's timeouts an application's table sets for its releases: those of a release's `timeouts` table that a deploy flag can set, and not the limits on the requests the edge serves, which belong to a release, a route or a virtual host.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `exit_grace` | string |  | How long a worker has for its shutdown work. |
| `gil_stall_timeout` | string |  | How long a worker may go with no thread running Python before it is replaced. |
| `hard_drain_grace` | string |  | How long a worker being replaced is given to finish its other requests before it is told to stop. |
| `hard_timeout` | string |  | How long a request may run in a worker before the worker is replaced. |
| `loop_stall_timeout` | string |  | How long an ASGI worker's event loop may go without running before the worker is replaced. |
| `shutdown_timeout` | string |  | How long a worker told to stop is given before it is killed. |
| `soft_timeout` | string |  | How long a request may run in a worker before it is interrupted by raising `muru.RequestTimeout` in its thread. |
| `websocket_drain_grace` | string |  | How long a worker told to stop keeps its open WebSockets. |

(config-deployment-client-class)=
## `[[client_class]]`

A `[[client_class]]` rule. A request gets the class when every input the rule names matches, each input matching when any of its entries does; rules are tried in order and a request that matches none is class `default`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `address_list` | string |  | A file of addresses and networks, one a line, as for `user_agent_list`. |
| `addresses` | array of strings |  | Client addresses and networks, such as 203.0.113.0/24. |
| `cookies` | table of strings |  | Cookies by name, each value matched by a regular expression. |
| `headers` | table of strings |  | Request headers by name, each value matched by a regular expression: a CDN's verdict, say. |
| `ja4` | array of strings |  | JA4 fingerprints of the client's TLS ClientHello, or their beginnings ending in `*`. |
| `ja4_list` | string |  | A file of JA4 fingerprints, one a line, as for `user_agent_list`. |
| `name` | string | required | The class's name, as logs, metrics, rate limits and applications see it. |
| `user_agent` | array of strings |  | User-Agent substrings, compared without regard to case. |
| `user_agent_list` | string |  | A file of User-Agent substrings, one a line, relative to the configuration file's directory and read again when it changes. |
| `user_agent_regex` | array of strings |  | Regular expressions matched against the User-Agent header. |
| `verify` | string |  | `dns` confirms the client's address by reverse and forward lookup against `verify_domains`; until it is confirmed the rule does not match. |
| `verify_domains` | array of strings |  | The domains a verified name must be, or end in. |

(config-deployment-condition-name)=
## `[condition.NAME]`

A named condition over Muru's own signals: when it fires and when it resolves, evaluated by the daemon every second. Nothing acts on a condition but what names it.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `clear_for` | string | `"0s"` | How long the condition must be clear at every evaluation before it resolves. |
| `clear_when` | string |  | The expression that resolves it; without one, it resolves when `when` has not held for `clear_for`. |
| `for` | string | `"0s"` | How long `when` must hold at every evaluation before the condition fires, such as `1m`. |
| `min_requests` | integer | `100` | The requests a share, error rate or latency needs in its window to be a figure; fewer leave the term unknown. |
| `when` | string | required | The expression that fires the condition, such as `error_rate{app='shop'} > 0.2`. |

(config-deployment-host-name)=
## `[host.NAME]`

A virtual host, the `[host.NAME]` table.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `access_log` | string | `access_log` in `[logs]` | Which of the host's requests the access log writes: `on`, `errors` or `off`, replacing the `[logs]` table's. An application's own replaces it. |
| `access_log_exclude` | array of strings |  | Paths whose requests the access log leaves out, besides those the `[logs]` table leaves out. |
| `access_log_file` | string |  | A file in the daemon's log directory that takes the host's records in place of `access.log`, for example `shop.log`. |
| `access_log_format` | string | `access_log_format` in `[logs]` | How the host's records are written: `json` or `combined`. |
| `access_log_query` | string | `access_log_query` in `[logs]` | What the host's records keep of the query string: `keep`, `mask` or `drop`. |
| `canonical` | string |  | The name the host is known by, for `use_canonical_name`. |
| `canonical_redirect` | boolean | `false` | Redirect requests for the host's other names to the canonical name. |
| `compression` | table, {ref}`[host.NAME.compression] <config-deployment-host-name-compression>` |  | How responses to the host's requests are compressed, each key replacing the daemon's `[compression]` value. A route's own replace them. |
| `default` | boolean | `false` | Answer requests whose host matches no virtual host, which are otherwise refused with 421. |
| `error_override` | table, {ref}`[host.NAME.error_override] <config-deployment-host-name-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[host.NAME.error_pages.STATUS] <config-deployment-host-name-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `https_redirect` | boolean | `false` | Redirect plain HTTP requests to HTTPS. |
| `listeners` | array of strings |  | The listener addresses it answers on; every listener when empty. |
| `maintenance` | table, {ref}`[host.NAME.maintenance] <config-deployment-host-name-maintenance>` |  | What the host's maintenance mode lets through, while `muructl maintenance on --host` has it on. |
| `max_body_size` | string | `max_body_size` in `[limits]` | The largest request body for the host's routes, for example 1MB, replacing the daemon's `[limits]` value. A route's own replaces it. |
| `names` | array of strings |  | The names it answers to: exact names, or `*.example.com` for one label. Compared case-insensitively, without a port or trailing dot. |
| `rate_limits` | array of strings |  | The `[rate_limit.NAME]` tables applied to every request for the host, before a route's. |
| `request_headers` | table, {ref}`[host.NAME.request_headers] <config-deployment-host-name-request-headers>` |  | Headers removed from, and set on, the host's requests before anything is asked to serve them. A route's own are applied after them. |
| `response_headers` | table, {ref}`[host.NAME.response_headers] <config-deployment-host-name-response-headers>` |  | Headers removed from, set on and added to every response to the host's requests. A route's own are applied after them. |
| `slow` | string | `slow` in `[logs]` | How slow a request is to be written under `access_log = "errors"`. |
| `timeouts` | table, {ref}`[host.NAME.timeouts] <config-deployment-host-name-timeouts>` |  | The timeouts of a request for the host's routes, each replacing the daemon's `[limits]` value. A route's own replace them. |
| `tls` | table, {ref}`[host.NAME.tls] <config-deployment-host-name-tls>` |  | Its certificate and key, for TLS listeners. |
| `use_canonical_name` | boolean | `false` | Give the application the canonical name as SERVER_NAME rather than the name matched. |

(config-deployment-host-name-compression)=
## `[host.NAME.compression]`

The `[compression]` table: how the edge compresses responses. Every key is optional. A virtual host and a route take the same table, where a key left out is the level beneath's.

The same keys apply at `[route.compression]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Whether responses are compressed at all. |
| `encodings` | array of strings | `["br", "zstd", "gzip"]` | The encodings offered in preference order: br, zstd and gzip. An empty list is refused; `enabled = false` turns compression off. |
| `level` | string | `"default"` | How hard to compress: fast, default or best. |
| `min_size` | string | `"1KB"` | The smallest body worth compressing, for example 1KB; at least 1 byte. |
| `types` | array of strings | `["text/*", "application/json", "application/javascript", "application/xml", "image/svg+xml"]` | The content types compressed: exact types or a family as text/*. An empty list is refused. |

(config-deployment-host-name-error-override)=
## `[host.NAME.error_override]`

Replacing an application's own error responses with error pages, the `error_override` table.

The same keys apply at `[route.error_override]` and `[app.NAME.release.ID.error_override]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `statuses` | array of integers |  | The statuses whose responses are replaced. |
| `when` | string | `"all"` | When a listed response is replaced: `all` (the default), `empty_body`, `not_json` or `html_only`. |

(config-deployment-host-name-error-pages-status)=
## `[host.NAME.error_pages.STATUS]`

A custom error page: one entry of an `error_pages` table, keyed by status (`404`) or status and reason (`"503:maintenance"`). Exactly one of `file`, `template` and `path` is given.

The same keys apply at `[route.error_pages.STATUS]`, `[app.NAME.release.ID.error_pages.STATUS]` and `[overlay.NAME.routes.NAME.error_pages.STATUS]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `content_type` | string | `"text/html; charset=utf-8"` | The content type of a template. |
| `file` | string |  | A file sent as the body, with the content type its name implies. A relative path is taken from the source directory for a release's own page (in `[tool.muru]`, `muru.toml` or the release's table), and from the file's directory for any other page in a file `muructl apply` reads; in the daemon's own file the path is absolute. |
| `path` | string |  | An application path the edge asks for the page, as an internal sub-request that sees `REDIRECT_STATUS` and `muru.error_reason`. |
| `template` | string |  | A template the edge renders, with `{status}`, `{title}`, `{reason}`, `{description}`, `{request_id}`, `{retry_after}`, `{host}` and `{path}` filled in. |

(config-deployment-host-name-maintenance)=
## `[host.NAME.maintenance]`

What maintenance mode lets through, the `maintenance` table of a release.

The same keys apply at `[app.NAME.release.ID.maintenance]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `bypass_cookie` | string |  | A `name=value` cookie that lets a client through. |
| `exempt_addresses` | array of strings |  | Client addresses, or ranges in CIDR form, served as usual. |
| `exempt_paths` | array of strings |  | Path prefixes served as usual while the application is in maintenance. |
| `retry_after` | string | `"60s"` | The `Retry-After` the maintenance page carries, for example 5m. |

(config-deployment-host-name-request-headers)=
## `[host.NAME.request_headers]`

What a virtual host or a route does to the headers of a request before anything is asked to serve it: the `request_headers` table. The names are removed first, then the headers set.

The same keys apply at `[route.request_headers]` and `[overlay.NAME.routes.NAME.request_headers]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `remove` | array of strings |  | Headers removed from the request, by name. |
| `set` | table of strings |  | Headers set on the request, by name, each replacing any the client sent. |

(config-deployment-host-name-response-headers)=
## `[host.NAME.response_headers]`

What a virtual host or a route does to the headers of a response: the `response_headers` table. The names are removed first, then the headers set, then those added.

The same keys apply at `[route.response_headers]` and `[overlay.NAME.routes.NAME.response_headers]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `add` | table of strings or arrays of strings |  | Headers added to the response beside any it has, by name: a value, or a list of values for a header sent more than once. |
| `remove` | array of strings |  | Headers removed from the response, by name. |
| `set` | table of strings |  | Headers set on the response, by name, each replacing any the response has. |

(config-deployment-host-name-timeouts)=
## `[host.NAME.timeouts]`

The timeouts of one request, the `timeouts` table of a virtual host or a route. Each key replaces the same key of the level beneath, down to the daemon's `[limits]`.

The same keys apply at `[route.timeouts]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `body_idle_timeout` | string | `body_idle_timeout` in `[limits]` | Time a client may pause while sending a body. |
| `queue_timeout` | string | `queue_timeout` in `[limits]` | Time a request may wait for a worker. |
| `request_deadline` | string | `request_deadline` in `[limits]` | Time a request may take to produce its response head. |
| `response_idle_timeout` | string | `response_idle_timeout` in `[limits]` | Time a worker may pause mid-response. |

(config-deployment-host-name-tls)=
## `[host.NAME.tls]`

A virtual host's certificate chain and private key, as PEM files.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `certificate` | string | required | The certificate chain, leaf first. A relative path is taken from the file's directory. |
| `key` | string | required | The private key. A relative path is taken from the file's directory. |

(config-deployment-overlay-name)=
## `[overlay.NAME]`

An `[overlay.NAME]` table: changes to named routes and to applications, applied over the base configuration while the overlay is active, by its schedule or by hand.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `advisory` | boolean | `false` | Its schedule only raises overlay.would_activate events; it is activated by hand only. |
| `apps` | table of tables, {ref}`[overlay.NAME.apps.NAME] <config-deployment-overlay-name-apps-name>` |  | Changes to applications, by name. |
| `cooldown` | string | `"10m"` | For an overlay its condition activates: how long after it ends its condition cannot activate it again. |
| `latch` | boolean | `false` | Once its schedule or its condition activates it, it stays active, past the window or the condition resolving, until a person ends it. |
| `max_per_hour` | integer | `4` | For an overlay its condition activates: the most activations by its condition in an hour. |
| `min_active` | string | `"5m"` | For an overlay its condition activates: the least it stays active, however soon the condition resolves. |
| `preconditions` | table, {ref}`[overlay.NAME.preconditions] <config-deployment-overlay-name-preconditions>` |  | What must hold for the overlay to activate. |
| `priority` | integer |  | Which of two active overlays setting the same field wins: the higher. Overlays that would conflict must both set one. |
| `routes` | table of tables, {ref}`[overlay.NAME.routes.NAME] <config-deployment-overlay-name-routes-name>` |  | Changes to routes, by the route's `name`. |
| `trigger` | table, {ref}`[overlay.NAME.trigger] <config-deployment-overlay-name-trigger>` |  | When the overlay is active on its own; by hand only without it. |
| `warm_up` | table, {ref}`[overlay.NAME.warm_up] <config-deployment-overlay-name-warm-up>` |  | What to start ahead of its schedule's windows. |

(config-deployment-overlay-name-apps-name)=
## `[overlay.NAME.apps.NAME]`

What an overlay changes on an application: its maintenance mode, and its scheduled capacity, for its default worker group or `group`. Capacity does not conflict between overlays: the highest floor and ceiling stand.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `answer` | string | `"read_only"` | How refused methods are answered, with the values a route's `methods.answer` takes. |
| `cap_workers` | integer |  | At most this many workers, whatever the policy and the autoscaler choose (issue 712): the lowest across overlays applies, and it wins over a floor. |
| `deny` | array of strings |  | Methods refused on each of the application's routes. |
| `freeze_deploys` | boolean | `false` | Refuse deploys and rollouts of the application unless overridden. |
| `group` | string | `"default"` | The worker group the capacity applies to; the default group without it. |
| `maintenance` | boolean |  | Maintenance mode while the overlay is active. |
| `max_workers` | integer |  | The most workers, a ceiling over the group's own. |
| `min_workers` | integer |  | The fewest workers, a floor over the group's own. |
| `mode` | string |  | The site mode on each of the application's routes. |
| `scale_in` | string | `"normal"` | `hold` keeps the group from scaling in; `normal` (the default) leaves it to the autoscaler's rules. |

(config-deployment-overlay-name-preconditions)=
## `[overlay.NAME.preconditions]`

What must hold for an overlay to activate.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `probe` | string |  | A path the `release_ready` release must answer with 2xx or 3xx, asked through the edge, such as /healthz. |
| `release_ready` | string |  | A release that must be ready, as app@release, or as release when the overlay changes routes of one application only. |

(config-deployment-overlay-name-routes-name)=
## `[overlay.NAME.routes.NAME]`

What an overlay changes on a route.

The same keys apply at `[overlay.NAME.routes.NAME.client_routes.NAME]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `answer` | string | `"read_only"` | How refused methods are answered, with the values a route's `methods.answer` takes. |
| `client_routes` | table of tables, {ref}`[overlay.NAME.routes.NAME] <config-deployment-overlay-name-routes-name>` |  | Routes for client classes at the route's host and path, by class: each a copy of the route with these changes, or the route for that class already there with them. |
| `deny` | array of strings |  | Methods refused, as the route's `methods.deny`. |
| `error_pages` | table of tables, {ref}`[host.NAME.error_pages.STATUS] <config-deployment-host-name-error-pages-status>` |  | Error pages, by status or status:reason, each in place of the route's page for the same key. |
| `mode` | string |  | The site mode. |
| `rate_limits` | array of strings |  | The rate limits, in place of the route's. |
| `request_headers` | table, {ref}`[host.NAME.request_headers] <config-deployment-host-name-request-headers>` |  | The headers removed from and set on the route's requests, in place of the route's `request_headers`. |
| `response_headers` | table, {ref}`[host.NAME.response_headers] <config-deployment-host-name-response-headers>` |  | The headers removed from, set on and added to the route's responses, in place of the route's `response_headers`. |
| `target` | table, {ref}`[overlay.NAME.routes.NAME.target] <config-deployment-overlay-name-routes-name-target>` |  | What serves the route instead. |
| `traffic` | table, {ref}`[overlay.NAME.routes.NAME.traffic] <config-deployment-overlay-name-routes-name-traffic>` |  | The traffic policy: a pin sends every request to one release. |

(config-deployment-overlay-name-routes-name-target)=
## `[overlay.NAME.routes.NAME.target]`

What serves a route under an overlay: one of an application, a static directory or an upstream.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | string |  | An application. |
| `static` | string |  | A directory the edge serves. A relative path is taken from the file's directory. |
| `upstream` | string |  | An HTTP upstream. |

(config-deployment-overlay-name-routes-name-traffic)=
## `[overlay.NAME.routes.NAME.traffic]`

An overlay's traffic policy for a route.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `pin` | string |  | Send every request to this release of the route's application. |
| `split` | table of integers |  | Shares by release, in place of the route's split. |

(config-deployment-overlay-name-trigger)=
## `[overlay.NAME.trigger]`

When an overlay is active on its own.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `condition` | string |  | A named condition: the overlay is active while it is firing, and ends once it has resolved. |
| `external` | boolean | `false` | Whether the signed webhook of the `[overlay_webhook]` table may activate and end it. |
| `schedule` | table, {ref}`[overlay.NAME.trigger.schedule] <config-deployment-overlay-name-trigger-schedule>` |  | A window in time. |

(config-deployment-overlay-name-trigger-schedule)=
## `[overlay.NAME.trigger.schedule]`

A window in time: one-off, from `start` until `end` when it has one, or recurring, from each time `cron` names for `duration`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `cron` | string |  | When each window starts, as a cron expression such as '0 2 * * *' (see [`cron::Cron`]). |
| `duration` | string |  | How long each recurring window lasts, such as 1h. |
| `end` | string |  | When it ends; it stays active until ended or promoted without it. |
| `start` | string |  | When it starts, as a local date and time such as 2026-11-02T09:00, or with an offset. |
| `timezone` | string | the daemon's system time zone | The time zone local times are in, such as Australia/Sydney; the daemon's own zone without it. |

(config-deployment-overlay-name-warm-up)=
## `[overlay.NAME.warm_up]`

An overlay's warm-up: for `lead` before each window of its schedule, `start_release` runs, so a switch to it lands on warm workers.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `lead` | string | required | How long before the window, such as 30m. |
| `start_release` | string |  | The release to start, as app@release, or as release when the overlay changes routes of one application only. |

(config-deployment-rate-limit-name)=
## `[rate_limit.NAME]`

A `[rate_limit.NAME]` table: a token bucket for each key, which requests of the matching classes draw from.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `action` | string | `"reject"` | `reject` (the default) answers 429, `delay` holds the request up to `max_delay` first, `log` only counts. |
| `burst` | integer | the rate's count | How many requests a full bucket allows at once. |
| `headers` | boolean | `false` | Send the RateLimit-Policy and RateLimit fields with a 429. |
| `ipv4_prefix` | integer | `32` | The leading bits of an IPv4 address that make the key (32). |
| `ipv6_prefix` | integer | `64` | The leading bits of an IPv6 address that make the key (64). |
| `key` | string or array of strings | `"address"` | What a bucket is kept for: `address` (the default), `client_class`, `header:NAME`, `cookie:NAME`, or a list of them. |
| `match` | table, {ref}`[rate_limit.NAME.match] <config-deployment-rate-limit-name-match>` |  | Which requests draw from the buckets; all of them without it. |
| `max_delay` | string | `"1s"` | The longest a request is held under `delay`. |
| `rate` | string | required | How fast a bucket fills, for example 2/s, 100/m or 1000/h. |

(config-deployment-rate-limit-name-match)=
## `[rate_limit.NAME.match]`

The requests a rate limit or route applies to.

The same keys apply at `[route.match]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `client_class` | array of strings |  | Client classes. |

(config-deployment-route)=
## `[[route]]`

A route, one of the `[[route]]` entries: a path prefix of a virtual host to an application.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `accept_redirects_from` | array of strings |  | Applications other than the route's own (or a static route's fallback) whose internal redirects may reach the route. A static route with no fallback needs it for any redirect to reach it. |
| `app` | string |  | The application whose serving release takes the requests; empty for a static or upstream route. |
| `auth` | table, {ref}`[route.auth] <config-deployment-route-auth>` |  | An authorisation decision the edge asks an application for before it serves the route. |
| `cache` | table, {ref}`[route.cache] <config-deployment-route-cache>` |  | The cache headers files are sent with. |
| `case` | string | `"strict"` | `strict` (the default), which matches names as they are on disk, or `insensitive`. |
| `compression` | table, {ref}`[host.NAME.compression] <config-deployment-host-name-compression>` |  | How the route's responses are compressed, each key replacing the host's, or else the daemon's `[compression]` value. |
| `error_override` | table, {ref}`[host.NAME.error_override] <config-deployment-host-name-error-override>` |  | Replace the application's own error responses with error pages. |
| `error_pages` | table of tables, {ref}`[host.NAME.error_pages.STATUS] <config-deployment-host-name-error-pages-status>` |  | Custom error pages, by status or status:reason. |
| `fallback` | table, {ref}`[route.fallback] <config-deployment-route-fallback>` |  | What takes a request no file answers. |
| `hash` | array of strings | `["sha256"]` | The digests computed while receiving: sha256 (the default), sha384, sha512 or sha1; an empty list for none. |
| `host` | string | required | The virtual host. |
| `index` | array of strings | `["index.html"]` | The files tried, in order, when a directory is requested. |
| `internal` | boolean | `false` | Reachable only through internal redirects: a client's request for the path is answered 404. |
| `match` | table, {ref}`[rate_limit.NAME.match] <config-deployment-rate-limit-name-match>` |  | The client classes the route takes; every request without it. Among routes with the same path, one naming the request's class comes first. |
| `max_body_size` | string | the host's `max_body_size` | The largest request body, for example 1GB, replacing the host's, or else the daemon's `[limits]` value, for the route. On an upload route it bounds the body as received, files included. |
| `max_file_size` | string |  | The largest uploaded file, for example 512MB; no limit without it. |
| `max_files` | integer | `100` | The most files in one request. |
| `methods` | table, {ref}`[route.methods] <config-deployment-route-methods>` |  | Methods the route refuses. |
| `mode` | string |  | The site mode passed to the application as muru.mode, for example read-only. |
| `name` | string |  | A name for the route, unique among routes, by which overlays change it. |
| `path` | string | `"/"` | The path prefix, `/` for everything. The longest prefix wins. |
| `precheck` | boolean | `false` | Ask the application with the request's headers alone first, and read the body only when it answers 2xx. |
| `rate_limits` | array of strings |  | The `[rate_limit.NAME]` tables applied to the route's requests, after the host's. |
| `redirect_slash` | boolean | `true` | Whether a directory asked for without a trailing slash is redirected to the slash form. |
| `request_headers` | table, {ref}`[host.NAME.request_headers] <config-deployment-host-name-request-headers>` |  | Headers removed from, and set on, the route's requests before anything is asked to serve them, after the host's. |
| `response_headers` | table, {ref}`[host.NAME.response_headers] <config-deployment-host-name-response-headers>` |  | Headers removed from, set on and added to every response to the route's requests, after the host's. |
| `spool_quota` | string | `"10GB"` | What the application's stored uploads may hold at once when a request on the route arrives. |
| `static` | string |  | The directory served by the edge, instead of an application. A relative path is taken from the file's directory. |
| `strip_prefix` | string |  | The leading part of `path` that is where the application lives: `path` itself, or a leading part of it whole segments long, so `/sub` of `/sub/path` and never `/su`. An application's release gets it as SCRIPT_NAME, after a trusted X-Forwarded-Prefix and before a mount's path, and its mounts and worker groups are matched against the rest of the path. An upstream, a managed HTTP release or a detour is sent the rest of the path, with this part appended to X-Forwarded-Prefix. A static route takes it only with a fallback, which it applies to. Without it nothing is stripped. |
| `timeouts` | table, {ref}`[host.NAME.timeouts] <config-deployment-host-name-timeouts>` |  | The timeouts of the route's requests, each replacing the host's, or else the daemon's `[limits]` value. |
| `traffic` | table, {ref}`[route.traffic] <config-deployment-route-traffic>` |  | How requests are shared between the application's releases; without it every request goes to the serving release. |
| `upload` | string |  | `multipart` or `raw` makes the edge receive and store uploads before the application sees the request; off without it. |
| `upstream` | string |  | The HTTP upstream that takes the requests, instead of an application. |

(config-deployment-route-auth)=
## `[route.auth]`

A route's authentication sub-request: the edge sends a bodiless GET to an application path and serves the route only when that answers 2xx.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | string |  | The application that decides; empty when an upstream does. |
| `cache` | table, {ref}`[route.auth.cache] <config-deployment-route-auth-cache>` |  | Remember allowing decisions for a while (off without it). |
| `copy_headers` | array of strings |  | Response headers of an allowing answer added to the request as trusted headers; a client's own copies are always removed. |
| `path` | string | required | The path asked, usually of an internal mount or route. |
| `timeout` | string | `"2s"` | How long the handler has, queueing included, for example 2s. |
| `upstream` | string |  | The HTTP upstream that decides, instead of an application. |

(config-deployment-route-auth-cache)=
## `[route.auth.cache]`

How long and by what an allowing decision is remembered.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `key` | array of strings | required | What a decision is keyed by: `cookie:NAME` and `header:NAME` entries. A request with none of them present is never served from the cache. |
| `ttl` | string | required | How long a decision stands, for example 30s. |

(config-deployment-route-cache)=
## `[route.cache]`

A static route's cache headers.

The same keys apply at `[app.NAME.release.ID.static.cache]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `except` | array of strings |  | Files that get no-cache instead, by pattern on their path within the route. |
| `immutable` | boolean | `false` | Whether files are marked immutable. |
| `max_age` | string |  | How long clients may keep a file, for example 1y or 1h. |

(config-deployment-route-fallback)=
## `[route.fallback]`

A static route's fallback: an application, or a file within the directory.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `app` | string |  | The application that takes requests no file answers. |
| `file` | string |  | The file served for paths with no file of their own. |
| `upstream` | string |  | The HTTP upstream that takes them instead. |

(config-deployment-route-methods)=
## `[route.methods]`

A route's method restrictions.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `answer` | string | `"read_only"` | `read_only` (the default) answers 503 read_only; `method_not_allowed` answers 405 with the methods left in Allow. |
| `deny` | array of strings |  | The methods refused, for example POST, PUT, PATCH and DELETE. |

(config-deployment-route-traffic)=
## `[route.traffic]`

A route's traffic policy: the share of requests each release takes, the key requests are bucketed by, and stickiness.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `classes` | table of strings |  | The release, by id, or `upstream:NAME`, that takes every request of a client class, ahead of stickiness and the split. |
| `hash_key` | string | `"cookie"` | What a request is bucketed by: `cookie` (the default, an anonymous id cookie the edge issues), `header:NAME`, `cookie:NAME`, or `client` (the client address). A missing header or cookie falls back to the client address. |
| `salt` | string | the route's host and path | Mixed into the hash, so two routes split the same users differently. |
| `split` | table of integers |  | Percent of requests to each release, by id; the application's serving release takes the rest. A release at 0 is reached only by preview or by a sticky cookie. |
| `sticky` | boolean | `false` | Whether a client keeps the release it first got, through a cookie. |
| `sticky_grace` | string | `"10m"` | How long sticky clients keep a release that is leaving the route alive, for example 10m. |

(config-deployment-upstream-name)=
## `[upstream.NAME]`

An HTTP upstream, one of the `[upstream.NAME]` tables: an unmanaged HTTP server that routes, fallbacks, traffic shares and authentication sub-requests can target.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `addresses` | array of strings | required | Where the server listens: `host:port` entries, or `unix:/path` for a Unix socket. Requests take the addresses in turn, and one that fails is set aside for a while. |
| `connect_timeout` | string | `"5s"` | How long a connection may take to open, for example 5s. |
| `health` | table, {ref}`[upstream.NAME.health] <config-deployment-upstream-name-health>` |  | Active health checks and passive ejection. |
| `protocol` | string | `"http1"` | `http1` (the default), `h2c` (HTTP/2 in the clear, by prior knowledge) or `h2` (HTTP/2 over TLS). |
| `sendfile_header` | string |  | A second response header honoured as `X-Muru-Sendfile` is, such as `X-Sendfile`. |
| `sendfile_roots` | array of strings |  | Directories a path in a trusted `X-Muru-Sendfile` response header must lie in for the edge to serve the file; with none every hint is refused. A relative path is taken from the file's directory. |
| `tls` | table, {ref}`[upstream.NAME.tls] <config-deployment-upstream-name-tls>` |  | Connect with TLS; `h2` implies it. |
| `trust_muru_headers` | boolean | `false` | Honour X-Muru-Redirect from the upstream, as from an application: its X-Muru-* response headers are otherwise removed. |

(config-deployment-upstream-name-health)=
## `[upstream.NAME.health]`

An upstream's health checks: probes of each address, and the ejection of one whose requests fail.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `eject_after` | integer | `3` | Failed requests in a row before an address is ejected; 0 never ejects. |
| `eject_for` | string | `"30s"` | How long an ejected address is left out before a request tries it again, for example 30s; doubled each time the trial fails, up to ten times as long. |
| `healthy_after` | integer | `2` | Successful probes in a row before it is healthy again. |
| `interval` | string | `"10s"` | How often each address is probed, for example 10s. |
| `path` | string |  | The path probed with a GET, healthy on 2xx or 3xx; without it a connection that opens is healthy. |
| `timeout` | string | `"2s"` | How long a probe may take, for example 2s. |
| `unhealthy_after` | integer | `3` | Failed probes in a row before an address is unhealthy. |

(config-deployment-upstream-name-tls)=
## `[upstream.NAME.tls]`

How an upstream's TLS connections are made.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `ca` | string |  | A PEM file of the certificates trusted; the public roots without it. A relative path is taken from the file's directory. |
| `server_name` | string | the address's host | The name the certificate must carry, sent as SNI; the address's host without it. |
| `verify` | boolean | `true` | Check the certificate. |
