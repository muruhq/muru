# Settings for muructl run

`muructl run` takes what the application is from the project's
`[tool.muru]` table and how it serves from the flags. The flags a developer
types every time, the port, `--https`, `--host`, the reload patterns, the
access log, can live in the project too, in a table of their own, and a
developer can keep a copy of that table beside the project for one
machine alone.

## The run table

```toml
[tool.muru]
entrypoint = "shop.wsgi:application"
processes = 2

[tool.muru.run]
port = 9000                     # or "auto"
https = true
https_only = false
https_port = 9443
tls_cert = "certs/dev.pem"      # relative to the project, as the flags are
tls_key = "certs/dev-key.pem"
hosts = ["shop.localhost"]
reload = true                   # the default in development mode
reload_dirs = ["shop", "templates"]
reload_include = ["**/*.html"]
reload_exclude = ["**/*.py"]
access_log = "errors"           # on (the default), errors or off
access_log_file = "errors.jsonl" # JSON lines in place of the terminal
access_log_exclude = ["/static/*"]
slow = "500ms"
dashboard = "127.0.0.1:8082"    # or "off"
compression = false
toolbar = true                  # the debug toolbar on HTML pages
tracing = true                  # tracing with wrapture, for runs alone
timestamps = true               # the full UTC time in front of every line
env_file = ".env"               # variables the run starts with
data_dir = "/srv/shop-dev"      # the data directory, kept between runs
```

In a `muru.toml` the same keys go in a `[run]` table at the top level.
Each key is a flag of `muructl run`, named in snake case and taking the
same values; `compression = false` stands for `--no-compression`,
`--no-toolbar` turns a table's `toolbar = true` off for one run,
`--no-tracing` turns a table's `tracing` off for one run, and
`--no-timestamps` turns a table's `timestamps = true` off for one run. With
the table above, `muructl run` on its own is the whole command in place
of:

```
muructl run shop.wsgi:application --https --host shop.localhost --port 9000 --reload-include '**/*.html' --access-log errors
```

A key belongs in the table when it describes how this checkout serves
rather than what the application is. `processes`, `threads`, the
timeouts, the memory policy and the static directories describe the
application and stay in `[tool.muru]`. `--bind` and `--allow-public` stay
on the command line, so that serving beyond loopback is something typed
each time; so do `--python` and `--chdir`, which describe the machine and
the invocation, `--single` and the link flags, and `--max-body-size`,
which has its production counterpart in the host limits.

The table applies whenever the project is found: as the argument, with
`--chdir`, or in the current directory, with an entrypoint on the command
line too. The daemon ignores it, so `muructl deploy` of the project
neither reads nor rejects it, and so does `muructl serve`, which has its
own `[serve]` table.

`muructl run --print-config` prints the settings the run would use and
exits: the application's settings, then the `[run]` table, each with
where it came from, in the shape of the project's file. It ends with the
interpreter the run would use, and an interpreter the run could not use
is refused here as the run would refuse it.

```text
$ muructl run --print-config --threads 2
# The settings this muructl run would use, and where each came from.
# The project's settings are read from ./pyproject.toml.

entrypoint = "shop.wsgi:application"    # [tool.muru] in ./pyproject.toml
processes = 1                           # default
threads = 2                             # command line
...

[run]
port = 8123              # [tool.muru.run] in ./pyproject.toml
access_log = "errors"    # [run] in ./muru.local.toml
reload = true            # default
...

# The interpreter is Python 3.13.1 (cp313) at ./.venv/bin/python (the project's .venv).
```

While the run serves, `muructl config show --effective` from the project
directory lists the settings it runs with, each with where it came from,
the `[run]` table's as `run.port` and so on. `muructl serve` answers the
same way, with its own table's as `serve.listen`.

## Your own machine: muru.local.toml

A `muru.local.toml` beside the project's `pyproject.toml` or `muru.toml`
holds a `[run]` table with the same keys, for the machine it is on:

```toml
[run]
port = 9100
dashboard = "off"
```

Beside the `[run]` table it takes only the `when` key of `[build]`,
[`[[instrumentation]]`](instrumentation.md#listing-the-modules) entries,
which come before the project's, and [`tracing`](tracing.md#tracing-on-one-machine),
which takes the place of the project's. Any other key is refused with a
message naming the committed file where the application's settings
belong. It is never committed: `muructl run` says once per run that it is
using the file, and if the project's `.gitignore` does not cover it, the
start-up line says so. Its `instance` key names the daemon the
checkout's other commands reach (see [running more than one
daemon](named-instances.md#reaching-an-instance-from-a-checkout)), which
is the only key `muructl deploy` reads from it.

## Opening the application in the browser

`--open` opens the application in your browser once the run first
answers, and not again when it reloads:

```console
$ muructl run --https --open
```

From another terminal, `muructl open` in the project directory opens the
application of the run serving it, and `muructl open --dashboard` its
dashboard:

```console
$ muructl open
muru: opened https://localhost:8443/
muru: 1 other address: muructl open --list
$ muructl open /admin/
$ muructl open --dashboard
$ muructl open --dashboard --request 01J9Z6T2W3
```

The URL is https when the run serves HTTPS, under its first `--host`
name, else `localhost`, on the port it is bound to. `--https` and
`--http` choose the scheme, and refuse one the run does not serve.
`--print` prints the URL and opens nothing, `--list` prints every URL
the application can be reached at, best first, with how each was
chosen, and `--json` with either prints them as JSON. `--request ID`
opens a request's page on the dashboard, as the `Muru-Request-Link`
header names it.

`muructl open APP` opens an application on a daemon instead, with the
URL worked out from the daemon's routes, virtual hosts and listeners:
the application's default route first, a virtual host's canonical name
before its other names, and HTTPS before HTTP. `--route`, `--host` and
`--listener` choose among them. A virtual host's name may be another
machine's, as `shop.example.com` usually is the production server's, so
a name ending in `.localhost` comes first, a name that resolves to an
address the listener is bound on, or to loopback for a listener on every
address, is kept, and any other is left out; `--any-name` keeps them.
When none is left, `muructl open` says which names it left out and why,
and offers `http://127.0.0.1:PORT/`, which reaches the listener's
default host. With no APP, the application is the project's, by its
`[project]` name.

The browser is the program `$BROWSER` names, then `open` on macOS, and
on Linux `wslview` under WSL, `xdg-open` or `gio open`, given the URL as
an argument. On Linux with no display, in an SSH session with no
`$BROWSER`, or with no launcher found, `muructl open` prints the URL and
says it could not open a browser; in an SSH session the address is the
remote machine's, so opening it from your own needs port forwarding.

## Variables from a .env file

`muructl run --env-file FILE`, or `env_file` in the run table or in
`muru.local.toml`, gives the run the variables of a `.env` file. A
relative path is the project's. A file is only read when one of these
names it: a `.env` beside the project is not picked up by itself.

```sh
# Lines starting with # and blank lines are skipped.
DEBUG=1
export DATABASE_URL=postgres://localhost/shop
SECRET_KEY='not a real one'     # a comment
GREETING="line one\nline two"
```

- A line is `NAME=VALUE`, with an optional `export` in front.

- An unquoted value ends at the end of the line, or at a `#` with a
  space before it.

- A single-quoted value is taken literally.

- A double-quoted value takes the escapes `\"`, `\\`, `\$`, `\n`, `\r`
  and `\t`.

- A value is never expanded, so `$HOME` stays as it is written. A quoted
  value ends on its own line.

A line that cannot be read stops the run, naming the file and the line
number. The file's values override `[tool.muru.env]`, which overrides
the environment `muructl run` was started with. The file is read when
the run starts, so a change to it takes a restart.

`muructl run --print-config` shows the file as the layer it came from
and names the variables it sets, without their values:

```text
[run]
env_file = ".env"    # [run] in ./muru.local.toml
...

# .env sets DEBUG, DATABASE_URL, SECRET_KEY. Over [tool.muru.env]: DEBUG.
```

`muructl check --env-file FILE` reads the same format, for settings the
import needs. `muructl deploy` and `muructl serve` do not read one.

## The data directory

A run gives the application a [data directory](data-directory.md) for
the files it keeps from one run to the next, such as a SQLite database,
as `MURU_DATA_DIR`, with `MURU_MEDIA_DIR` and `MURU_UPLOADS_DIR` beneath
it, and a directory for built static files as `MURU_STATIC_DIR`. Both
are in your user state directory, `runs/NAME-HASH` under
`~/.local/state/muru` on Linux and `~/Library/Application Support/muru`
on macOS (or under `MURU_STATE_DIR` when it is set), where `NAME` is the
application's name and `HASH` comes from the project's path, so two
checkouts of one project keep apart. Nothing is put in the project,
which a database could otherwise be committed from by mistake. The run
prints where they are when it starts, and `muructl status` shows them
while it runs.

`--data-dir DIR`, or `data_dir` in the run table or `muru.local.toml`,
puts the data directory somewhere else, a relative one in the project;
the static directory stays in the run's own. The run reads it at start,
so a change takes a restart. A project's command run with no run up,
`muructl command run` or `muructl exec`, reads `data_dir` from the run
table and `muru.local.toml` too, and uses the same directory.

## What wins

Highest first: a flag, then `muru.local.toml`, then `[tool.muru.run]`,
then the defaults. Lists add across the layers, so `hosts` in the table,
`hosts` in the local file and `--host` on the command line are all
served; the reload patterns and the access log excludes add the same way.
A boolean a file turns on has a flag to turn it off for one run:
`--no-reload`, `--no-https` and `--no-https-only`.

## Reload

A development run reloads on change without a flag: it watches the
project and starts a new release when a source file changes, and the
start-up line says so beside the directories and patterns watched.
`--no-reload` turns it off for one run, and `reload = false` in the run
table or in `muru.local.toml` for good; `--reload` turns it back on over
a file that turned it off. Under `--allow-public` the run does not reload
unless `--reload` is given, and the daemon never reloads.

Files that `.gitignore` names are not watched, including what the
`.gitignore` files of a repository the project sits in name. A project
kept in a directory that the repository around it ignores as a whole,
such as another repository's scratch directory, is watched by its own
`.gitignore` alone, and the start-up line says so. When no file in the
watched directories matches the patterns, the start-up line says that no
change reloads the run.

The settings files are watched whatever the patterns say:
`pyproject.toml`, `muru.toml`, `muru.local.toml` and the file
`--settings-file` names. A change to one reads the project's settings
again, and the new release starts with them: `preload_modules`,
`import_report`, `lazy_imports`, `tracing`, `preload`, the hooks, the
mounts and groups, the worker timeouts and the rest of what a release
takes. Settings that are not usable, such as a file that does not parse,
leave the current release serving, and the terminal says why.

The run table's settings (the port, HTTPS, the dashboard, the reload
patterns, the access log, the toolbar and the rest, apart from
`tracing`), `python` and the limits on a request (`max_body_size` and the
request timeouts) apply only as the run starts. A change to one of them is named on a line of its own,
once, against the file it changed in, and the run keeps the value it
started with until it is restarted:

```
muru: reloading, ./pyproject.toml changed
muru: pyproject.toml changed port; restart the run for it to take effect
muru: switched to release r2
```

A setting given on the command line wins over the files, so a change to
it in a file changes nothing and is not named.
