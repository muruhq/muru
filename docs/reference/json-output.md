# JSON output

Every `muructl` command that reports something takes `--json` and prints
what it reports as JSON instead of text, for scripts, CI jobs and coding
agents.

## The shape

- **The admin API's answer, with its field names.** A command that asks the
  daemon (or a development run) for something prints the admin API's answer
  for it as one indented JSON object. Field names are those of the
  `muru.admin.v1` protocol, in snake case: `current_release`,
  `resource_version`, `time_unix_ms`. Numbers are JSON numbers, times are
  milliseconds since the epoch in fields ending `_unix_ms`, and nested
  records are objects. A field the answer does not set is present with its
  empty value (`""`, `0`, `false`, `[]`), and an absent nested record is
  `null`.

- **A list is wrapped in its answer.** `muructl app list --json` prints
  `{"apps": [...]}`, not a bare array, so a field can be added to an answer
  without changing how a script reads it.

- **Streams are JSON lines.** `muructl events --follow --json` and
  `muructl logs --json` print one compact JSON object per line as each
  arrives.

- **Errors are JSON too.** A command given `--json` that fails prints
  `{"error": "..."}` on standard output, with the sentence it would have
  printed, and exits with the same status as without `--json`.

```console
$ muructl release show shop@r3 --json
{
  "app": "shop",
  "id": "r3",
  "state": "ready",
  "message": "",
  "source": "/srv/shop",
  "entrypoint": "shop.wsgi:application",
  ...
  "serving": true,
  ...
}
$ muructl release show shop@r9 --json
{
  "error": "release shop@r9 does not exist"
}
```

## Commands

| Command | Prints |
| --- | --- |
| `muructl status` | The daemon's or run's status: `version`, `pid`, `listeners`, `app_count`, `release_count`, `edge_pid`, `no_host` (the applications serving with no hosts declared to choose between them, empty otherwise), `limits` (in a cgroup v2, the [container's limits](../howto/containers.md#container-limits) as last read and the memory guard: `cgroup`, `cpu_quota` in CPUs, `cpus`, and in bytes `memory_max`, `memory_used`, `memory_left`, `headroom`, `workers_memory` and `host_memory`, then `guard` (`watching`, `replacing` or `holding`), `guard_note`, `last_replaced` and `last_replaced_unix_ms`; null without a cgroup) and the rest. With `--processes`, the processes as a tree: `version`, `hostname`, `mode` (`daemon`, `run` or `serve`), `started_unix_ms`, `sampled_unix_ms`, `limits` (as above), `figures` (the instance's last second: `requests_per_second`, `errors_per_second`, `p50_ms`, `p95_ms`, `in_flight`, `websockets`, `connections`, `tls_handshakes_per_second`, `received_bytes_per_second`, `sent_bytes_per_second`) and `root`. Each process has `pid`, `os_parent_pid`, `role`, `what`, `app`, `release`, `group`, `index` (a worker's within its group), `user`, `started_unix_ms`, `cpu_percent`, `rss_bytes`, `oom_score_adj` (on Linux, the kernel's adjustment for the process, 500 for a worker; absent elsewhere), `readopted` (true when `os_parent_pid` is not the process above it, except a relay whose host is marked with the same `os_parent_pid`), `state` (a host's: its release's part in routing, `serving`, `canary`, `held`, `draining` or `retained`, or its state while it starts), `interface` (a host's and its workers': what the release's default mount runs as, `wsgi`, `asgi` or `grpc`, which chooses the GIL/LAG column, or empty before the host has said), `figures` (`requests_per_second`, `busy`, `slots`, `queue`, `p95_ms`, `gil_wait_p95_ms`, `loop_lag_p95_ms`, each null when not known, and for a host or a group `departed`, the workers no longer listed whose last second the figures count), `groups` (a host's groups with their `figures`, when its release has more than one) and `children`. With `--watch`, each frame is one such document on a line of its own |
| `muructl daemon status` | The same as `muructl status`, with `config`, `state_dir`, `run_dir`, `log_dir`, `cache_dir` and `socket`. With no daemon running, `kind` is `stopped` and those six say where a daemon started with the same options would keep them, the rest being empty or 0, and the command exits with status 3 |
| `muructl daemon reload` | What was reloaded: `message` |
| `muructl app list` | `{"apps": [...]}`, each with `name`, `current_release`, `previous_release`, `user`, `maintenance` and the rest |
| `muructl release list` | `{"releases": [...]}`, each a release as `release show` prints it |
| `muructl release show` | The release: `app`, `id`, `state`, `message`, `source`, `entrypoint`, `serving`, its settings, mounts and groups; `host_running` (whether a host runs for it now), `workers` (each with `index`, `pid`, `group`, `socket` and `state`, one of `starting`, `ready`, `debugging`, `replacing` or `retiring`, empty when no host runs), `detected_interface` (`wsgi`, `asgi` or `grpc`, empty until known), `retained_until_unix_ms` (for a retained release, when it is stopped, otherwise `0`); for a managed server, `server_instances`, each with `index`, `pid`, `address` and `started_unix_ms` |
| `muructl release wait` | The release once it reached a state waited for |
| `muructl host list` | `{"hosts": [...]}` |
| `muructl route list` | `{"routes": [...]}` |
| `muructl upstream list` | `{"upstreams": [...]}` |
| `muructl deploy` | The new release, as `release show` prints it; with `--no-wait`, as recorded; with `--dry-run`, the plan: `app`, `release`, `new_app`, `description`, `settings` (each with `key`, `value` and `layer`), `env` (each variable and secret the release would get, with `name`, `secret` as `store/key` or empty, `as_file` and `layer`, never its value), `builds`, `serving`, `settings_from`, `hold`, `retain_ms`, `retain_workers` (null unless `--retain-workers` gave it), `retained` (what the serving release would keep, such as `1 worker to a group`) and `refusals` |
| `muructl render` | The document `muructl apply` takes: `{"app": {NAME: {"release": {ID: {...}}}}}`, the release's table with the keys of the daemon's format |
| `muructl rollback` | The release traffic went back to |
| `muructl drain` | The release drained |
| `muructl rollout canary` | The rollout as it started, or with `--wait` as it finished: `id`, `app`, `candidate`, `baseline`, `state`, `step`, `percent`, `message`, its `steps` and its gates |
| `muructl rollout bluegreen` | The release that now serves |
| `muructl rollout status` | `{"rollouts": [...]}`, or with `--wait` the rollout as it finished |
| `muructl rollout pause` | The rollout after the change |
| `muructl rollout resume` | The rollout after the change |
| `muructl rollout promote` | The rollout after the change |
| `muructl rollout abort` | The rollout after the change |
| `muructl scale` | Each worker group's scaling state after the change: `app`, `release`, `groups` |
| `muructl scale status` | Each worker group's policy, workers, signals, last decision and churn: `app`, `release`, `groups` |
| `muructl restart` | `{"release": ..., "workers": [...]}`, each with `index` (within its group), `group`, `pid`, `new_pid`, `forced`, and `served_by` once the new worker serves (`null` with `--no-wait`) |
| `muructl overlay list` | `{"overlays": [...]}`, each with `name`, `active`, `state`, `trigger`, `changes` and its times |
| `muructl overlay preview` | `{"changes": [...]}`: what activating the overlay now would change |
| `muructl overlay activate` | The outcome: `message` |
| `muructl overlay end` | The outcome: `message` |
| `muructl overlay promote` | The outcome: `message` |
| `muructl maintenance on` | `app`, or `host` with `--host`, `on` and `since_unix_ms` |
| `muructl maintenance off` | `app`, or `host` with `--host`, `on` and `since_unix_ms` (0) |
| `muructl dashboard login` | The sign-in link: `url`, `code`, `expires_in_s`, and with `--token` the `token` |
| `muructl apply` | The report: `errors`, `warnings`, `notes`, `changes`, `unchanged`, `applied` and `snapshot_error`, why the edge did not take the snapshot that carries the changes, which fails the apply. Each change has `action`, `subject`, `detail` and, once applied, `outcome`, which for a release that failed is `failed:` and the reason |
| `muructl config diff` | The report, as `apply` prints it |
| `muructl config export` | The desired state as a JSON document rather than TOML |
| `muructl config show` | `document` (TOML), with `--effective` also `attributions` and `layers`, and where it came from: `version`, `node`, `time_unix_ms`, `state_version` |
| `muructl mcp skill status` | How the project's copy of the skill compares: `path`, `state` (`current`, `missing`, `out_of_date`, `edited` or `edited_out_of_date`), `edited`, `version` and `installed_version` |
| `muructl secret list` | `{"store": ..., "keys": [...]}`; values are never shown |
| `muructl events` | `{"events": [...]}`, each with `sequence`, `time_unix_ms`, `type`, `subject`, `level`, `message`; with `--follow`, one event per line |
| `muructl logs` | One line per log line: a release's output as `{"app", "release", "line"}`, the access log (`--access`) as the records it keeps, the edge's and the daemon's own logs (`--edge`, `--daemon`) as `{"app", "release", "line"}` with `app` and `release` empty |
| `muructl command list` | `app`, `release` and `commands`, each with `name`, `description`, `run` or `call`, `inputs` (`name`, `description`, `required`, `default`, `pattern`, `secret`), `timeout`, `exclusive` and its `last_run` (`release`, `started_unix_ms`, `duration_ms`, `exit_code`, `ok`, `message`), which only the daemon keeps. `release` is empty for a project's commands listed with nothing serving it |
| `muructl command run` | As `muructl exec` prints it |
| `muructl exec` | One object per line of output, with `release`, `line` and `stderr`, then one with `finished` true, the `exit_code`, `ok`, true when the command exited with 0 and was not stopped, and the ending as a `message`. `release` is empty for a command `muructl` runs itself |
| `muructl logs reopen` | What reopened its files: `edge`, and `hosts`, the releases whose log relay was told to |
| `muructl release prune` | `freed` (bytes), `dry_run`, `removed` and `kept`. `removed` holds what goes, each with `app`, `id`, `state`, `created_unix_ms`, `disk_bytes`, `why` (`keep` or `failed`) and `error` when it could not be removed or part of it was left behind. `kept` holds what stays, each with `app`, `id`, `state` and `why` (`serving`, `rollback`, `in_use`, `referenced`, `declared`, `keep` or `failed`) |
| `muructl release remove` | As `muructl release prune` prints it, with `why` of `named` and an empty `kept` |
| `muructl app remove` | As `muructl release prune` prints it, with `why` of `application` and an empty `kept` |
| `muructl cache list` | `user` and `daemon`, the areas asked for, each with `directory`, `total` and `entries`, each with `kind` (`runtime`, `certs`, `uv_cache` or `python`), `name`, `path`, `version` and `tag` of a runtime library, `size` (bytes) and `in_use`, what uses it |
| `muructl cache clear` | As `muructl cache prune` prints it |
| `muructl cache prune` | `user` and `daemon`, the areas worked on, each with `directory`, `freed` (bytes), `dry_run`, `removed` and `left` (what stayed because it is in use), each as `muructl cache list` gives them, with `freed` and `error` when it could not be removed |
| `muructl logs prune` | `freed` (bytes), `dry_run` and `removed`, each with `name`, `size`, `why` (`keep`, `older_than` or `orphaned`) and `error` when it could not be removed |
| `muructl logs rotate` | `logs`, each with `name`, `size`, `archive` (relative to the log directory, before compression through a daemon) and `error` when that log could not be rotated |
| `muructl logs truncate` | `freed`, `dry_run` and `logs`, each with `name`, `size`, `kept`, `freed`, `in_place` (the log was cut in place, so lines written while it was copied are lost), `late` (the bytes writers added during the swap, carried over) and `error` when that log could not be cut |
| `muructl logs list` | `directory`, `total`, `orphans_known` (whether the state store was available to tell) and `logs`, each with `name`, relative to the directory, `kind` (`access`, `edge`, `daemon`, `host` or `release`), `app`, `release`, `hosts`, `size`, `present`, `modified_unix_ms` (when the file itself was last written, 0 when it is gone), `archives_size`, `total`, `orphaned`, and `archives`, oldest first, each with `name`, `size` and `modified_unix_ms`. A target narrows `logs`, and `total` is then of those logs alone |
| `muructl metrics` | `{"samples": [...]}`, each with `name`, `type` (as its family declares, else `untyped`), `labels` and `value`; a value JSON cannot hold (`NaN`, `+Inf`) is kept as text |
| `muructl websockets` | `open`, `endpoints` by application and pattern, and with `--list` each open `sockets` entry |
| `muructl requests` | `in_flight`, `groups` (one per worker group: `app`, `release`, `group`, `queued` and `queued_oldest_ms`, `threads` (whether the group's workers run requests on threads), `threads_busy`, `threads_of` and `threads_oldest_ms`, `event_loop`, `event_loop_busy` and `event_loop_oldest_ms`), and with `--list` each `requests` entry: `request_id`, `client`, `app`, `release`, `group`, `method`, `target`, `state` (`queued` or `running`), `kind`, `worker` (its index within its group, -1 with none), `started_unix_ms` and `age_ms` |
| `muructl logging level` | One debug answer for each logger set, as an array; with `--reset`, or with no level to list what is in force, an array of one. A listing's `result` is `changed`, each with `logger`, `level`, `previous` and `until_unix_ms` (null until a reset) |
| `muructl logging access` | `overrides`, the changes in force after the command, the newest last, each with `mode`, `app`, `paths` and `until_unix_ms`, and `reset`, how many `--reset` put back |
| `muructl debug status` | The debug answer: `{"release": ..., "workers": [...]}`, each worker with `index` (its index within its group), `group`, `pid`, `ok`, `error` and its `result`: `group`, `generation`, `socket`, `state` (`starting`, `ready`, `debugging`, `replacing` or `retiring`), `ready`, `routed`, `debugging`, `uptime_s`, `requests`, `threads` and `loop` (each `busy`, `of` and `oldest_ms`, present for the sides the worker has), `stale` (the sides are from the worker's last report, since it did not answer within a second) and its memory figures |
| `muructl debug stacks` | The debug answer |
| `muructl debug tasks` | The debug answer |
| `muructl debug inspect` | The debug answer |
| `muructl debug logging` | The debug answer, each worker's `result` with `loggers` (the root first, then every logger by name, each with `name`, `level`, `effective`, `propagate`, `disabled`, `level_from` (`muru`, `temporary`, `changed since`, `python`, `application`, or null for a logger with no level of its own) and `handlers`), `disable` (the `logging.disable` level, or null), `last_resort` (a handler, or null), `capture_warnings` and `warnings_filters`, in order, each with `action`, `message`, `category`, `module` and `lineno`. A handler has `class`, `level`, `destination` (a stream, a file path, a socket address, syslog or a mail server, any URL's user information masked, or null), `format` (the format string, or the formatter's class) and `filters` |
| `muructl debug memory` | The debug answer |
| `muructl debug profile` | The debug answer, with the samples rather than a written profile |
| `muructl debug trace` | The debug answer |
| `muructl debug exec` | The debug answer, with each worker's output and value |
| `muructl debug report` | The report: `muru` (the version), `platform` (`os`, `kernel`, `arch`), `runs` (`muructl run`, `a user daemon`, `a system daemon` or `muructl serve`), `running`, `container`, `releases` (each `release`, `state`, `serving`, `kind`, `interface`, `frameworks`, `python`, `build` and its `workers`, each `index`, `group` and `state`), `settings` (each `scope`, `key`, `value` and `source`), `daemon_settings` (each `key` and `value`; `null` for a development run and `muructl serve`), `events` (`window_hours`, `counts`, each `type`, `level` and `count`, and `recent`, each `time`, `type`, `level`, `subject` and `message`), `not_collected` (each `part` and `why`) and `replaced` (how many `applications`, `hosts`, `users`, `paths`, `addresses` and `modules` read as placeholders). A part that could not be collected is `null` |
| `muructl debug request` | A development run's record of one request: `request_id`, `in_progress` (true until the response has ended and the worker's part has arrived), `started_unix_ms`, `client`, `app`, `release`, `host`, `virtual_host`, `group`, `mount`, `snapshot`, `method`, `target`, `version`, `listener`, `status`, `reason`, `bytes`, `received`, `worker` (its index within its group, null when none served it), `retried`, `queue_ms`, `app_ms`, `total_ms`, `redirects`, `client_class` and `handoff`, as the access log's JSON has them; `connection` (`id`, `position`, 1 for the connection's first request, `idle_ms`, `tls_handshake_ms` on the first request of a TLS connection and null otherwise, and `closed_after`, true when Muru closed the connection after this response because it had served its share of requests); `request_headers` and `response_headers`, each a list of `name` and `value`, with secret values `***`; `compression` (`compressed`, `encoding` and `skipped`, the reason it was not compressed: `off`, `opted_out`, `head`, `no_body`, `encoded`, `no_transform`, `type`, `small`, `not_accepted` or `failed`); `worker_part` (`expected` until the worker's part arrives, then `arrived`, or `missing` when it did not come; empty when the worker was not asked, as for a static file) and `from_worker`, the worker's part once it arrived: `thread`, and in milliseconds from when the worker took the request `to_start_response`, `to_first_byte`, `to_last_byte` and `to_close` (0 for what did not happen), `body_bytes` (before compression), `body_source` (`iterable`, `file_wrapper`, `write`, `asgi_body` or `pathsend`), `sent_by_edge`, `outcome` (`completed`, `client_disconnected`, `failed` or `timeout`), `reason`, `error_phase` (`call` or `iteration`), `error`, `traceback`, `logs` (each `at_ms`, `level`, `logger` and `message`) and `logs_dropped`; and `panels`, the record as the dashboard's request page draws it, each with `name`, `title`, `spans` (each `name`, `start_ms` from the request's arrival, `duration_ms`, `category` and `children`), `values` (each `name` and `value`) and `tables` (each `caption`, `columns` and `rows`, each row's `cells`) |
| `muructl debug attach` | Takes the flag with the other debug commands; the session itself is pdb's or the IDE's |
