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
timestamps = true               # the full UTC time in front of every line
env_file = ".env"               # variables the run starts with
```

In a `muru.toml` the same keys go in a `[run]` table at the top level.
Each key is a flag of `muructl run`, named in snake case and taking the
same values; `compression = false` stands for `--no-compression`,
`--no-toolbar` turns a table's `toolbar = true` off for one run, and
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
start-up line says so. `muructl deploy` does not read it.

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
