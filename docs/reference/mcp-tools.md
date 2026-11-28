# MCP tools

The tools `muructl mcp serve` offers a coding agent (see
[coding agents](../howto/coding-agents.md)). Each runs the `muructl` command
shown with `--json` against the same daemon and returns what it prints, as
the [JSON output reference](json-output.md) describes; a command that fails
makes the tool return its error's sentence as a tool error. `--read-only`
leaves out every tool that changes something, and `--allow` keeps only the
ones it names.

Every tool except `docs`, `skill_status` and `instances` also takes an
`instance` argument, any value `--instance` takes (see [reaching an
instance](../howto/instances.md#reaching-an-instance)), which reaches that
instance for the one call in place of the server's own. The table leaves
it out.

| Tool | Kind | Runs | Arguments |
| --- | --- | --- | --- |
| `status` | reads | `muructl status` | none |
| `processes` | reads | `muructl status --processes` | `app` (an application, or `app@release`), `all` |
| `instances` | reads | `muructl instance list`, on this machine rather than the instance the server reaches | `orphans`, `label` |
| `doctor` | reads | `muructl doctor`, `doctor store` or `doctor timeline`, on the files of the daemon the server reaches: Muru itself checked, with nothing changed | `part` (installation, store, timeline), `limit` |
| `urls` | reads | `muructl open --list`: every URL the application can be reached at, best first, opening nothing | `app` |
| `list` | reads | `muructl app list`, `release list`, `host list`, `route list`, `upstream list`, `rollout status` or `overlay list` | `what` (apps, releases, hosts, routes, upstreams, rollouts, overlays), `app` |
| `release` | reads | `muructl release show` | `release` (app@release) |
| `app_settings` | reads | `muructl app show` | `app` |
| `commands` | reads | `muructl command list` | `target` |
| `conditions` | reads | `muructl condition list`, or with `name` `muructl condition show` | `name` |
| `command_runs` | reads | `muructl command runs` | `app`, `name` |
| `command_run` | reads | `muructl command status`, or with `output` `muructl command logs --lines`, as an array of lines | `run`, `output` (how many of the last lines) |
| `events` | reads | `muructl events` | `after` (a sequence number) |
| `logs` | reads | `muructl logs`, the lines returned as an array | `target`, `lines` (default 100, at most 1000), `access` |
| `cache` | reads | `muructl cache list` | none |
| `log_files` | reads | `muructl logs list` | none |
| `debug` | reads | `muructl debug status`, `stacks`, `tasks`, `memory` or `inspect`, never with `--env`; or `debug request`, a development run's record of one request | `what` (status, stacks, tasks, memory, inspect, request), `target`, `worker`, `request_id` (for request) |
| `imports` | reads | `muructl debug imports`: the import report of a release's load, with its advice and the modules the workers loaded since the fork | `target`, `worker`, `load_target` (as `--target`), `top`, `min_ms`, `package`, `module`, `all` (as `--all`) |
| `scale_status` | reads | `muructl scale status` | `target` |
| `deployment` | reads | `muructl deployment show` or `deployment diff` | `action` (show, diff), `deployment`, `app`, `effective`, `file` |
| `daemon_config` | reads | `muructl daemon config show` or `daemon config check`: the daemon's own configuration file | `action` (show, check), `full` |
| `metrics` | reads | `muructl metrics`, the samples kept to those whose name starts with `prefix` | `prefix` |
| `websockets` | reads | `muructl websockets` | `app`, `list` |
| `requests` | reads | `muructl requests` | `app`, `list` |
| `overlay_preview` | reads | `muructl overlay preview` | `name` |
| `detours` | reads | `muructl detour list` | `action` (list) |
| `preview` | reads | `muructl preview --print`: a one-time entry link to the preview of a release, a detour or any release the daemon routes to, valid for a minute or for `valid_for`, for an address whose name reaches this machine, with the `Muru-Preview` header value, which holds the release's preview secret and is given when there is no link too, with why | `target` (`APP@RELEASE`, an application or a detour), `host`, `name`, `valid_for` |
| `skill_status` | reads | `muructl mcp skill status` | none |
| `docs` | reads | The user guide embedded in `muructl`: the list of pages, or one page's Markdown | `page` |
| `set_app_settings` | changes | `muructl app set` | `app`, `env`, `secrets`, `processes`, `threads`, `sync_threads`, `concurrency`, `max_requests`, `max_lifetime`, `max_surge`, `max_unavailable`, `timeouts`, `memory`, `replace` |
| `unset_app_settings` | changes | `muructl app unset` | `app`, `keys`, `all` |
| `deploy` | changes | `muructl deploy` | `app`, `source`, `entrypoint`, `python`, `hold`, `wait` (default true), `dry_run` |
| `promote` | changes | `muructl rollout bluegreen` | `release` |
| `rollback` | changes | `muructl rollback` | `app` |
| `drain` | changes | `muructl drain` | `release` |
| `rollout` | changes | `muructl rollout canary`, `pause`, `resume`, `promote` or `abort` | `action`, `release` (canary), `app` (the others), `steps`, `hold` |
| `scale` | changes | `muructl scale` | `target`, `group`, `processes`, `unpin` |
| `run_command` | changes | `muructl command run`, the output returned as an array of lines (the last 500) with `exit_code`, `ok` and `message`, as a tool error unless `ok`: the command exited with 0 and was not stopped; with `detach`, `muructl command run --detach`, answering with the `run` id at once | `target`, `name`, `inputs` (an object), `secret_inputs` (an object of STORE/KEY references), `detach` |
| `stop_command_run` | changes | `muructl command stop` | `run` |
| `detour_traffic` | changes | `muructl detour traffic` | `detour`, `traffic` (preview, all, share=N, class=NAME), `for` (a duration, after which the detour takes preview again), `paths` (the client URL paths it takes requests under alone), `all_paths` (every path again) |
| `restart` | changes | `muructl restart`: one worker, or with `all` or `group` every worker in turn, and `cancel` ends such a restart | `target`, `worker`, `all`, `group`, `cancel`, `no_wait`, `force` |
| `overlay` | changes | `muructl overlay activate`, `end` or `promote` | `action`, `name`, `for` |
| `maintenance` | changes | `muructl maintenance on` or `off` | `app` or `host`, `on` |
| `apply` | changes | `muructl apply` | `file`, `dry_run`, `hold`, `wait` (default true) |
| `log_level` | changes | `muructl logging level` | `target`, `levels`, `for`, `reset` |
| `access_log` | changes | `muructl logging access` | `mode`, `app`, `paths`, `for`, `reset` |
| `reopen_logs` | changes | `muructl logs reopen` | `workers` |
| `prune_releases` | changes | `muructl release prune`, refused unless `yes` is true or `dry_run` asked for | `app`, `keep`, `force`, `keep_logs`, `dry_run`, `yes` |
| `remove_release` | changes | `muructl release remove`, refused unless `yes` is true or `dry_run` asked for | `release` (app@release), `keep_logs`, `force`, `dry_run`, `yes` |
| `remove_app` | changes | `muructl app remove`, refused unless `yes` is true or `dry_run` asked for | `app`, `dry_run`, `yes` |
| `prune_cache` | changes | `muructl cache prune`, refused unless `yes` is true or `dry_run` asked for | `area`, `version`, `tag`, `dry_run`, `yes` |
| `clear_cache` | changes | `muructl cache clear`, refused unless `yes` is true or `dry_run` asked for | `area`, `certs`, `dry_run`, `yes` |
| `prune_logs` | changes | `muructl logs prune`, refused unless `yes` is true or `dry_run` asked for | `keep`, `older_than`, `orphaned`, `dry_run`, `yes` |
| `rotate_logs` | changes | `muructl logs rotate` | `target` (app@release), `access`, `host`, `all` |
| `truncate_logs` | changes | `muructl logs truncate`, refused unless `yes` is true | `target` (app@release), `access`, `host`, `all`, `keep`, `yes` |
| `reload_config` | changes | `muructl daemon reload` | none |
| `set_daemon_config` | changes | `muructl daemon config set`, or `daemon config unset` when no `value` is given: one key of the daemon's own configuration file, checked, written and reloaded; with `restart`, the daemon restarted in place when the change needs it | `key`, `value`, `restart` |

Every tool is annotated for the client as reading or changing things, and
none reaches beyond the daemon. Stopping, shutting down and upgrading the
daemon, `muructl exec`, and `debug exec` and `debug attach`, are not
offered: `run_command` runs only what an application declares. Of the
commands that change the daemon's own file, `edit` and `replace` are not
offered either: a tool changes one key at a time.
`run_command` refuses a secret input given in `inputs`, so a secret's
value never passes through the agent; give it in `secret_inputs` as an
entry of one of the daemon's [secret stores](../howto/secrets.md).
