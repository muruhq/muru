# In a container

`muructl serve` runs one application as the container's main process: the
edge, the controller, the host and its workers, with nothing kept between
starts. In a Dockerfile it is one line:

```dockerfile
CMD ["muructl", "serve", "shop.wsgi:application"]
```

The application is served on port 8080 of every interface, and the
probes and metrics on port 9090. Reload is off, error pages carry no
tracebacks, the request body limit is 10 MB, and nothing is installed or
built when the container starts.

## An image

The image holds `muructl`, the application's environment and the runtime
library, all added when the image is built:

```dockerfile
FROM python:3.13-slim AS build
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN UV_TOOL_BIN_DIR=/usr/local/bin uv tool install muructl
WORKDIR /app
COPY pyproject.toml uv.lock ./
RUN muructl build --deployment --deps-only --no-runtime --target /app/.venv
COPY . .
RUN muructl build --deployment --target /app/.venv --runtime-dir /opt/muru

FROM python:3.13-slim
COPY --from=build /usr/local/bin/muructl /usr/local/bin/muructl
COPY --from=build /app /app
COPY --from=build /opt/muru /opt/muru
WORKDIR /app
RUN useradd --uid 10001 app && install -d -o 10001 -g 0 -m 0770 /run/muru
USER 10001
EXPOSE 8080 9090
CMD ["muructl", "serve"]
```

`uv tool install` installs `muructl` from PyPI into an environment of its
own and links the command into `/usr/local/bin`. It needs no pip, picks
the build for the base's C library, glibc or musl, and fetches a Python
for the tool's environment when the base has none. `muructl` is a single
executable, so the second stage copies the binary alone, and
`COPY --from` copies the file the link points to. `uv tool install
muructl==1.0.0` pins a version; the runtime library the build fetches
is always the one matching `muructl`'s version.

`/run/muru` is where `muructl serve` writes at run time, and only root
can create it, so the image makes it for the application's user, and
for group 0, under which OpenShift runs an arbitrary user ID. With a
read-only root filesystem, a volume mounted there takes its place
([files it writes](#files-it-writes)).

The two `muructl build` steps install from `uv.lock` without resolving
anything:

- The first installs the dependencies alone, from `pyproject.toml` and
  `uv.lock`, before the source is copied in, so the layer is reused for
  as long as the dependencies stay the same. `--no-runtime` leaves the
  runtime library to the second step.

- The second installs the project itself into the same environment,
  compiles its bytecode, and unpacks the runtime library matching
  muructl's version and the environment's interpreter into `/opt/muru`,
  which the second stage copies.

A `.dockerignore` beside the Dockerfile keeps what the build makes, and
what it does not need, out of the copy of the source:

```
.venv
.git
__pycache__
*.pyc
muru.local.toml
```

`.venv` matters most: without it, a virtual environment on the machine
running the build is copied over the one the image built.

With no argument, `muructl serve` finds the application the way
`muructl run .` does, from the `[tool.muru]` table of the project in the
working directory, or in `--chdir`. Its interpreter is the first of:

1. the `python` setting;
2. the virtual environment `VIRTUAL_ENV` names;
3. `/app/.venv/bin/python`;
4. `python3` on the `PATH`.

The runtime library is the environment's own `muru` when it matches
muructl's version, or else the directory `muructl build --runtime-dir`
filled, `/opt/muru` by default.

### What muructl build runs

`muructl build` runs `uv sync` and prints the command it runs. The two
steps above run these, with `UV_PROJECT_ENVIRONMENT=/app/.venv`:

```
uv sync --frozen --no-dev --no-build --compile-bytecode --no-install-project
uv sync --frozen --no-dev --no-build --compile-bytecode
```

| Option | From |
| --- | --- |
| `--frozen` | always: the environment is installed from `uv.lock` as it is, never resolved |
| `--no-dev` | `--deployment`: the dev group is left out |
| `--no-build` | `--deployment`, unless `source_builds = true`: no package is built from an sdist; the project itself still is |
| `--compile-bytecode` | `--deployment`, unless `compile_bytecode = false` |
| `--no-install-project` | `--deps-only` |
| `--no-editable` | `--deployment` with a `--target` outside the project directory |
| `--extra`, `--group` | the `extras` and `groups` of `[tool.muru.build]`, and the `--extra` and `--group` flags |
| `--python` | the `python` setting of `[tool.muru]`, or the `--python` flag |

Around `uv sync` it adds three things:

- it stops before running uv when the project has no `uv.lock`;
- it probes the interpreter of the environment it built, and stops if
  Muru does not support it;
- it fetches the runtime library for that interpreter, with its
  published hash checked, and unpacks it into `--runtime-dir`.

The daemon runs the same build for a release, so the environment in an
image is the one a release on a server gets.

uv runs in the project directory with `muructl`'s environment, so
everything that configures `uv sync` typed by hand configures it here:
the `[tool.uv]` table of `pyproject.toml`, a `uv.toml` beside it, and
`UV_` environment variables. A private package index, for one, is set
in the project, and its credentials come from the environment uv reads
them from:

```toml
[[tool.uv.index]]
name = "internal"
url = "https://pypi.internal.example.com/simple"
```

```dockerfile
RUN --mount=type=secret,id=index,env=UV_INDEX_INTERNAL_PASSWORD \
    UV_INDEX_INTERNAL_USERNAME=build \
    muructl build --deployment --deps-only --no-runtime --target /app/.venv
```

The password is passed to the build as a secret, which stays out of the
image: `docker build --secret id=index,env=INDEX_PASSWORD .`. The
runtime library still comes from PyPI, whatever index the project names;
a build with no access to PyPI gives it a directory of wheels in
`MURU_WHEEL_DIR`.

### With uv alone

For a `uv sync` option that has no setting, the build stage runs uv
itself, and `muructl runtime install` adds the runtime library for the
environment's interpreter:

```dockerfile
FROM python:3.13-slim AS build
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN UV_TOOL_BIN_DIR=/usr/local/bin uv tool install muructl
ENV UV_PROJECT_ENVIRONMENT=/app/.venv
WORKDIR /app
COPY pyproject.toml uv.lock ./
RUN uv sync --frozen --no-dev --no-build --compile-bytecode --no-install-project
COPY . .
RUN uv sync --frozen --no-dev --no-build --compile-bytecode
RUN muructl runtime install --python /app/.venv/bin/python --target /opt/muru
```

The second stage is the same as above. `muructl runtime install` probes
the interpreter and fetches the library as `muructl build` does, from
PyPI, or from a directory of wheels given with `--wheel-dir` (or
`MURU_WHEEL_DIR`). It works the same for an environment pip or Poetry
made.

## Nothing installed at start

`muructl serve` never builds an environment, installs an interpreter or
fetches the runtime library, and anything it runs has `UV_OFFLINE=1` and
`UV_PYTHON_DOWNLOADS=never` set. A missing interpreter, package or
runtime library stops the start with a message naming it and the build
step that provides it:

```
muru: error: no runtime library for /app/.venv/bin/python (cp313): build the image with muructl build --runtime-dir /opt/muru, or add muru 1.0.0 to the environment
```

## Settings

Every setting is a flag. The same setting can come from three other
places, each taking precedence over the one before:

1. the project's `[tool.muru]` table, as for a deployment;
2. its `[tool.muru.serve]` table (`[serve]` at the top level of a
   `muru.toml`), whose keys are the flags in snake case;
3. an environment variable per flag, `MURU_SERVE_` and the flag's name in
   upper snake case: `MURU_SERVE_LISTEN` for `--listen`,
   `MURU_SERVE_PROCESSES` for `--processes`.

A flag takes precedence over all of them. A list given at one level
replaces the list below it, and an environment variable holds a list
separated by commas. So an image sets its defaults in the project or in
its command, and a deployment changes one setting with an environment
variable:

```toml
[tool.muru.serve]
processes = 4
access_log_exclude = ["/static/*"]
```

```
docker run -e MURU_SERVE_PROCESSES=8 registry.example.com/shop
```

`muructl serve --print-config` prints the settings it would use, each
with where it came from, and exits. The output has the shape of the
project's file, the application's settings and then a `[serve]` table:

```text
$ MURU_SERVE_PROCESSES=8 muructl serve --print-config
# The settings this muructl serve would use, and where each came from.
# The project's settings are read from /app/pyproject.toml.

entrypoint = "shop.wsgi:application"    # [tool.muru] in /app/pyproject.toml
processes = 8                           # MURU_SERVE_PROCESSES
threads = 4                             # default
...

[serve]
listen = "0.0.0.0:8080"                 # default
access_log_exclude = ["/static/*"]      # [tool.muru.serve] in /app/pyproject.toml
...
```

The `[run]` table is `muructl run`'s and is not read here, so a setting
both commands have, such as `access_log` or `slow`, is set again under
`[serve]`.

### A settings file from outside the image

`--config FILE`, or `MURU_SERVE_CONFIG`, names a project settings file
that is read in place of the `pyproject.toml` or `muru.toml` beside the
code, which is then not read at all. A deployment mounts one to change
the settings without building an image:

```toml
# /etc/shop/muru.toml, mounted from a ConfigMap
entrypoint = "shop.wsgi:application"

[commands.migrate]
run = ["python", "manage.py", "migrate", "--noinput"]

[serve]
processes = 4
trusted_proxy = ["10.0.0.0/8"]
```

```
muructl serve --config /etc/shop/muru.toml
```

The file has the form of a `muru.toml`, with serve's settings under
`[serve]`; a file named `pyproject.toml` is read for its `[tool.muru]`
table. It holds everything the project's own file would, the entrypoint
and the declared commands included, since the two are not merged.
Relative paths in it, such as a static directory, are taken from the
project directory. An environment variable or a flag still changes one
setting over it.

| Flag | Default | Effect |
| --- | --- | --- |
| `ENTRYPOINT` or `PATH` | the working directory | a `module:callable`, or the project directory |
| `--chdir DIR` | | the directory to find the project in |
| `--config FILE` | | a project settings file read in place of the one beside the code |
| `--listen ADDRESS` | `0.0.0.0:8080` | where the application is served |
| `--admin-listen ADDRESS` | `0.0.0.0:9090` | where the probes and metrics are served, or `off` |
| `--admin-status` | off | serve `/status` on the admin port too |
| `--min-ready N` | 1 | the workers ready in each required [group](#worker-groups-and-mounts) for `/readyz` to pass |
| `--processes N` | 1 | worker processes in the default group; see [container limits](#container-limits) |
| `--threads N` | 4 | threads per WSGI worker |
| `--python PATH` | as above | the interpreter |
| `--access-log WHAT` | `on` | `on`, `errors` (4xx and 5xx responses and requests slower than `--slow`) or `off` |
| `--access-log-exclude PATTERN` | | paths left out of the access log, repeatable |
| `--log-format FORMAT` | `text` | `text`, or `json` for a log collector |
| `--timestamps` | off | the full UTC time in front of every line |
| `--run-dir DIR` | `/run/muru` | where everything written at run time goes (also `MURU_RUN_DIR`) |
| `--runtime-dir DIR` | `/opt/muru` | where the image holds the runtime library |
| `--spool-quota SIZE` | 1GB | the most request bodies spooled to disk at once |
| `--shutdown-delay DURATION` | `0s` | how long the listener keeps accepting after SIGTERM; see [stopping](#stopping) |
| `--shutdown-budget DURATION` | `8s` | how long the whole stop may take, the delay included |
| `--trusted-proxy ADDRESS` | loopback peers' headers | a proxy whose forwarding headers are applied: an address, a CIDR range, or `loopback`, `private` or `unix`, repeatable; see [behind a proxy](behind-a-proxy.md) |

`--interface`, `--factory`, `--sync-threads`, `--concurrency`,
`--preload`, `--hooks`, `--timeout`, `--max-body-size`, `--max-requests`,
`--max-lifetime`, `--memory`, the connection limits, `--static` and
`--no-compression` work as they do for [`muructl run`](run-settings.md).

## The application's name

The application is named after `[project] name` in `pyproject.toml` when
there is one, with underscores and dots as hyphens, else after the
entrypoint's top-level module with underscores as hyphens. `--name`, or
`MURU_SERVE_NAME`, sets it. `muructl status`, the `app` label on the
[metrics](metrics.md) and the `app` field of the logs use it, and
`muructl run` names an application the same way.

## Container limits

On Linux with cgroup v2, `muructl serve` reads the container's limits
from its own cgroup: the CPU quota in `cpu.max`, the CPUs it may run on
in `cpuset.cpus.effective`, and the memory limit in `memory.max`. They
size nothing. The worker count is `--processes`, 1 unless it is set, and
each [group's](#worker-groups-and-mounts) own, and a worker has a hard
memory limit only when one is set with `--memory hard_limit=...`. The
limits are used to:

- warn at start when the configuration does not fit them;
- replace the largest worker when the container runs short of memory,
  with the memory guard below;
- decide whether a worker replaced for memory stops before its
  replacement starts;
- report a worker the kernel killed for memory;
- show what is used and what is left, in `muructl status` and `/status`.

On macOS, and in a container with no limits, there is nothing to read,
and none of this applies.

### Where the memory goes

Everything in the container counts against `memory.max`:

```
memory.max
├── headroom: 10% of the limit, at least 128MB, for muructl, the edge and the page cache
├── the host: the interpreter and the application it imported, counted once
├── each worker's own memory
└── files on memory-backed mounts: spooled request bodies, and /tmp on a tmpfs
```

Each worker is forked from the host once the application has been
imported, so at first it shares all of the host's memory and adds only
what it allocates itself. A worker's memory is measured as its unique
set size (`uss`, the [memory policy's](memory.md#what-is-measured)
default), which counts only its own pages, so what the host holds is
counted once, under the host.

The sharing wears away as the worker runs. CPython keeps a reference
count in every object and writes to it whenever the object is used, and
the first write to a shared page gives the worker its own copy of that
page. Over hours a worker's own memory climbs towards the size of what
the host holds, even if the application never changes that memory.
Muru calls `gc.freeze()` before each fork, which stops the garbage
collector's writes, but not the reference counts'. So plan for each
worker to end up holding a copy of the host's memory, as well as what it
allocates itself.

### The memory guard

At every memory check, every `check_interval` (5 seconds by default),
muructl reads what is left under `memory.max`, counting inactive file
pages as free, since the kernel reclaims those first. When less than the
headroom is left, it replaces the largest worker, by the memory policy's
`measure`, through the usual drain, so no request is lost:

- **One worker at a time.** The next worker is chosen once the one being
  replaced has gone and its replacement is ready.
- **Only when it helps.** When the workers' own memory, added up, is less
  than the shortfall, or a replacement leaves no more room than there was
  before it, replacing workers cannot bring the container back. muructl
  logs a warning that says what holds the memory, the host or the memory
  outside the workers and the host (muructl, the edge, the page cache and
  files on memory-backed mounts), and replaces no more workers. It warns
  again only after the container has been back above the headroom.
- **Reported.** A worker the guard replaces is a `worker.recycled` event
  that says the container was short of memory, counted in
  `muru_worker_recycles_total` with the reason `container_memory`, apart
  from a worker over its own limit, whose reason is `memory`.

The headroom is also how far the workers may grow, together, between
two checks before the kernel acts. A workload that grows faster than
that sets a shorter interval with `--memory check_interval=1s`.

### With a hard limit

A `hard_limit` is the user's own limit on one worker, set with `--memory
hard_limit=...` or in a group's `memory` table, and it is checked as the
[memory policy](memory.md) describes. Both apply: the hard limit to a
worker that grows too large on its own, and the guard to workers that
together use more than the container has.

| Container memory limit | `hard_limit` | A worker is replaced for its size |
| --- | --- | --- |
| none | not set | only by the soft limit, when one is set |
| none | set | by its hard limit |
| `memory.max` | not set | by the guard, when the container is short of memory |
| `memory.max` | set | by its hard limit, or by the guard when the workers together leave the container short |

A soft limit applies in every case when it is set. With a `memory.max`,
the kernel's out-of-memory killer remains the last resort.

### Replacing a worker near the limit

A worker replaced for memory, by the guard or by its own limits,
normally stays until its replacement is ready. When less is left under
`memory.max` than the worker's current size, by the policy's measure, it
leaves routing and stops first, and its replacement is forked once it
has gone. The requests it holds finish, and for that moment the
container has one worker fewer. With a single worker, requests wait for
the replacement up to the [queue timeout](timeouts.md).

A worker the kernel kills anyway is reported as a `worker.oom_killed`
event and counted once in `muru_worker_exits_total` with the reason
`oom_killed`, whatever it was doing, including being replaced, and the
host starts another.

### An example

A container with 2 CPUs and 1GB of memory runs two workers of an
application that holds 250MB once it has been imported:

```
docker run --cpus 2 --memory 1g -e MURU_SERVE_PROCESSES=2 registry.example.com/shop
```

It starts with these lines:

```
muru: container limits: 2 CPUs from cpu.max 200000 100000, memory.max 1.0GB with 128MB of headroom
muru: the host holds 250MB once the application is imported
```

The guard acts once more than 896MB is used, the limit less the
headroom. Just after start the container uses about 320MB: the host's
250MB, about 15MB of each worker's own, and about 40MB for muructl and
the edge. Over the following hours the workers' copies grow. When
worker 0 has 330MB of its own and worker 1 has 280MB, the container uses
900MB, so 124MB is left, less than the headroom. The guard replaces
worker 0, the larger, and its event says:

```
worker 0 (pid 52) replaced by pid 61: the container was short of memory, 124MB left under memory.max 1.0GB, less than the headroom of 128MB; it was the largest worker, uss 330MB
```

Since 124MB is less than worker 0's 330MB, worker 0 stops first, while
worker 1 serves, and its replacement starts at about 15MB, which leaves
about 440MB.

The same application with three workers would start with a warning,
since three full copies of the host's 250MB, with the host and the
headroom, come to more than 1GB:

```
muru: warning: the host holds 250MB, and 3 workers that each come to copy it need 1.1GB with the headroom, more than memory.max 1.0GB; 2 workers fit; see https://docs.muruhq.dev/en/latest/howto/containers.html#container-limits
```

The workers start, but as their copies grow the guard, or a hard limit,
keeps replacing them. An application that holds a lot once imported
runs fewer workers, with more threads in each, since a worker's threads
share one copy, or is given more memory.

### Warnings at start

Each of these is a warning, with a link to this section. muructl starts
as configured.

- **Workers over the CPUs.** When the workers configured, across every
  group, a group with a [scaling policy](#scaling-inside-the-container)
  at its `max_workers`, are more than the CPU quota rounded up: `muru: warning: 6
  workers over 3 groups is more than the 2 CPUs cpu.max allows`.
- **A preload that would not fit when copied.** When what the host holds
  once the application is imported, times the workers plus one, plus the
  headroom, is more than `memory.max`, as in the example above. The
  warning says how many workers would fit.
- **Hard limits over the limit.** When a group's `hard_limit`, times its
  workers, added up over the groups, is more than `memory.max`.
- **Request bodies in memory.** When the run directory is on a
  memory-backed filesystem, as below.

### Request bodies and /tmp in memory

Request bodies too large to hold in memory are spooled to files under
the run directory, up to `--spool-quota`. When the run directory is a
tmpfs, as in the [Compose example](#compose), or an `emptyDir` with
`medium: Memory` in Kubernetes, those files count against `memory.max`.
With a memory limit, muructl finds this at start, and then:

- the spool's quota is the headroom, rather than 1GB, unless
  `--spool-quota` is set, and the start line says so;
- a `--spool-quota` larger than the headroom is kept, with a warning.

Files an application writes to a memory-backed `/tmp` count against the
limit in the same way. When it is memory outside the workers that
leaves the container short, the guard's warning says so rather than
replacing workers.

### Changing the limits while it runs

The limits are read when they are used, so a change made while the
container runs, by `docker update` or a Kubernetes in-place resize,
applies at the next memory check:

- the guard works from the new limit, and its headroom from it;
- muructl logs the old and the new value, and gives the warnings above
  again against the new limits;
- a spool quota taken from the headroom follows the new headroom;
- `muructl status` and `/status` show the limits as they are now.

Raising the memory gives the container more room at once. Lowering it
is safe while the workers fit, and Kubernetes holds a decrease below
what the container uses until its use drops. A change to the CPUs
changes the time the same workers get. The worker count and a
`hard_limit` are configuration, so changing either is a restart of the
container, in Kubernetes a rollout of the Deployment.

### What can be seen

`muructl status` inside the container shows the limits as they are now,
what is used and left, and what the guard is doing:

```
$ docker exec shop muructl status
...
cpu      2 CPUs from cpu.max
memory   memory.max 1.0GB, 586MB used, 438MB left, headroom 128MB
         workers 296MB, host 250MB
guard    watching; it last replaced shop@r1 worker 0 (pid 52) at 07:40:12 UTC
```

The guard is `watching`, `replacing` a worker, or `holding` when
replacing workers would not help, with what it is doing after the
state. `/status`, with `--admin-status`,
gives the same as its `limits` field, and so do `muructl status --json`
and `muructl debug bundle`.

## Worker groups and mounts

A project's [worker groups](worker-groups.md) and [mounts](mounts.md)
work under `muructl serve` as they do under `muructl run` and the
daemon, from the `groups` and `mounts` tables of its `[tool.muru]`
table, or of the file `--config` names:

```toml
[mounts.reports]
path = "/reports"
entrypoint = "shop.reports:application"

[groups.reports]
paths = ["/reports/*"]
mounts = ["reports"]
processes = 1
```

`--processes` sizes the default group. A named group's own `processes`, `threads`, `sync_threads` and
`concurrency` size its workers, and a group that leaves one out takes
the release's.

`/readyz` counts each group: every group needs `--min-ready` workers
ready, except a group marked `optional = true`, which never holds
readiness back. While a group is short, `/readyz` answers 503 and says
which, for example `group reports has 1 of 2 workers ready`. `/status`,
with `--admin-status`, lists each group with its workers ready.

## Scaling inside the container

A worker group keeps the count it is configured with unless it has a
[scaling policy](scaling.md). With a `[tool.muru.scaling]` table, or a
group's own `scaling` table, `muructl serve` scales the group as the
daemon does, with the same modes, signals and bounds:

```toml
[tool.muru]
entrypoint = "shop.wsgi:application"
processes = 2
threads = 4

[tool.muru.scaling]
mode = "spare"
min_workers = 2
max_workers = 6
min_spare = 1
max_spare = 3
```

`min_workers` and `max_workers` are the only bounds on the count. The
start line warns when `max_workers`, added up over the groups, is more
than the CPUs, as it does for a fixed count.

**Memory holds growth back.** With a memory limit, a scale-out goes
ahead only when what is left under `memory.max`, less the headroom, has
room for one more worker. A worker counts at its group's `hard_limit`
when one is set, so `max_workers` can be reached only when that many
hard limits fit; otherwise at what the group's workers measure (their
proportional set size), else the group's soft limit, else the mean of
the container's workers. A scale-out held back is shown as `held at` in
`muructl scale status` and is a `scaling.denied_by_budget` event, with
the room it needed and what was left. More room, as after a resize,
lets a scale-out the policy asks for go ahead; it never adds workers on
its own.

**Short of memory, a group with a policy shrinks.** When the [memory
guard](#the-memory-guard) would replace the largest worker, and that
worker's group has a policy and more than `min_workers` workers, the
group is scaled in by that worker instead: it drains, and no replacement
starts. It is a `scaling.decision` event that says it was for memory,
with the room left. At `min_workers`, or in a group without a policy,
the worker is replaced as above. So lowering a pod's memory in place
shrinks it, never below `min_workers`.

While the guard is acting, or the room is under the headroom, no
scale-out is admitted, and the policy grows the group again only once
the room is a worker's cost above the headroom, so the count does not
swing back and forth. A worker the guard replaces is not a scale-in, and
a scale-in the policy asks for goes ahead as usual.

`muructl scale status` works inside the container through the admin
socket, and `/readyz` and `--min-ready` count the workers ready, however
many the policy is bringing the group to. A `--min-ready` above a
group's `min_workers` is refused at start. `muructl scale pin` and
`unpin` are not offered under `muructl serve`: a container's count is
its configuration, and a different one is a new image or a changed
Deployment.

## Probes and metrics

The admin port answers the questions a container platform asks, apart
from the application's port, so a route to the application never
reaches it and a busy application never delays it:

| Path | Answers 200 when | Use it for |
| --- | --- | --- |
| `/livez` | the edge responds and the host is alive; a pod whose workers are all busy is alive | a liveness probe |
| `/readyz` | at least `--min-ready` workers are ready in each worker group that is not optional, and the container is not stopping | a readiness probe |
| `/startupz` | the application has loaded and the first workers are ready | a startup probe |
| `/metrics` | always, with the [metrics](metrics.md) in Prometheus text format | scraping |
| `/status` | with `--admin-status`: the release, its workers and groups, and the settings worked out, as JSON | looking at a running container |

The port answers anyone who can reach it. In Kubernetes a NetworkPolicy
limits who that is. It must listen on an address the kubelet can reach,
so `127.0.0.1` stops the probes from working.

`muructl probe ready` asks the same question inside the container and
exits 0 or 1, so an image with no curl has a health check, with the
admin port open or closed (`live` and `startup` ask the other two):

```dockerfile
HEALTHCHECK --interval=5s CMD ["muructl", "probe", "ready"]
```

In a Kubernetes container spec:

```yaml
ports:
  - {name: http, containerPort: 8080}
  - {name: admin, containerPort: 9090}
startupProbe:
  httpGet: {path: /startupz, port: admin}
  periodSeconds: 2
  failureThreshold: 60
livenessProbe:
  httpGet: {path: /livez, port: admin}
readinessProbe:
  httpGet: {path: /readyz, port: admin}
  periodSeconds: 2
```

## Logs

Everything goes to standard output: the application's output tagged with
its worker, Muru's own lines, and the access log. Lines have no time in
front, since the container runtime records one for each line; access log
lines start with the request's time. `docker logs -t` and `kubectl logs
--timestamps` show the runtime's. `--timestamps` puts the full UTC time
in front of every line, for a supervisor that adds none.

`--log-format json` writes one JSON object per line instead, for a log
collector, each with its `time`, a `kind` (`access`, `app` for the
application's output, `command` for a [command's](commands.md) output
with its name in `command`, `build` for an environment build's lines,
`muru`, `serve.stopping` and `serve.draining`
for the lines that mark a [stop](#stopping), or `panic` for a panic in
Muru itself, with its `thread`, `location` and, when `RUST_BACKTRACE`
asks for one, a `backtrace` array), and the line's fields:

```json
{"time":"2026-11-02T05:42:22.836Z","kind":"app","source":"muru.worker[0]","message":"worker 0 is ready"}
```

With it, Python logging writes [JSON records](logging.md#python-logging)
too, unless the project names a format. Each record becomes the line's
object, with its level, logger, message and request id as fields, and an
exception logged with it as one `exception` field:

```json
{"time":"2026-11-02T05:42:23.104Z","kind":"app","source":"muru.worker[0]","level":"ERROR","logger":"shop.views","message":"checkout failed","request_id":"01M6CY84YP8J7V9K0QN8ABV14V","exception":"Traceback (most recent call last):\n  ...\nKeyError: 'sku'","app":"shop","release":"r1","group":"default","worker":0}
```

An exception the application did not catch is logged on the `muru`
logger at `ERROR`, so it is one such record, with the request's id and
the traceback as its `exception` field. A traceback written to standard
error rather than logged, such as one an application prints itself, is
gathered into a single record too, from `Traceback (most recent call
last):` to the exception's own line. So a log collector that splits
records at newlines keeps each traceback whole.

Each JSON line also carries the pod it came from, when the Deployment
passes it in with the Downward API, and the OpenTelemetry service and
resource attributes when they are set:

| Variable | Field |
| --- | --- |
| `POD_NAME` | `pod` |
| `POD_NAMESPACE` | `namespace` |
| `NODE_NAME` | `node` |
| `OTEL_SERVICE_NAME` | `service` |
| `OTEL_RESOURCE_ATTRIBUTES` | `resource`, an object of its attributes |

```yaml
env:
  - name: POD_NAME
    valueFrom: {fieldRef: {fieldPath: metadata.name}}
  - name: POD_NAMESPACE
    valueFrom: {fieldRef: {fieldPath: metadata.namespace}}
  - name: NODE_NAME
    valueFrom: {fieldRef: {fieldPath: spec.nodeName}}
```

## Files it writes

Everything `muructl serve` writes goes under one directory, `/run/muru`
unless `--run-dir` or `MURU_RUN_DIR` names another: the admin socket, the
workers' sockets, and request bodies too large to hold in memory, up to
`--spool-quota`, beyond which an upload is refused with 507. It assumes no
user account for its user ID and creates the directory, the admin and
probe sockets, the `release` directory and `serve.json` group-writable, so it runs under
whatever user ID the platform assigns with group 0, as OpenShift does, and
`muructl` run under another user ID with group 0 can use the socket. The
`spool` directory holds request bodies and is private to the user ID that
serves. With a read-only root filesystem, that directory and `/tmp` are
the only writable mounts it needs:

```yaml
securityContext:
  runAsNonRoot: true
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  capabilities: {drop: ["ALL"]}
  seccompProfile: {type: RuntimeDefault}
volumeMounts:
  - {name: run, mountPath: /run/muru}
  - {name: tmp, mountPath: /tmp}
```

If the directory cannot be written, `muructl serve` stops at once and says
which volume it needs.

## Inside the container

`muructl` inside the container talks to `muructl serve` over the admin
socket in the run directory, so the commands that read or adjust a
running application work through `docker exec` or `kubectl exec`:

```
docker exec shop muructl status
docker exec shop muructl logs --access --follow
docker exec shop muructl debug stacks
docker exec shop muructl restart --all
```

The commands that name an application or a release take the one being
served when given none. Only someone who may exec into the container can
use them. The debugging commands need no added capability: with every
capability dropped, `no-new-privileges` and the runtime's default
seccomp profile, the host still reads its workers' stacks and profiles,
as their parent, as the [Kubernetes
guide](kubernetes.md#looking-inside-a-pod) describes. Commands that
manage releases, hosts or the daemon say they need the daemon.

The project's declared [commands](commands.md#the-projects-own-commands)
run the same way, through the `muructl serve` in the container, with
their output in the container's log:

```
docker exec shop muructl command list
docker exec shop muructl command run backup
docker exec shop muructl exec -- python manage.py showmigrations
```

A command also runs in a container of its own, where nothing is serving,
which is how a migration is run before the new image serves. `muructl`
runs it itself, with the image's interpreter and the project's
environment variables, and exits with its status:

```
docker run --rm --env-file shop.env registry.example.com/shop:1.0.0 \
    muructl command run migrate
```

## Stopping

muructl is the container's process 1 and runs as an init of its own, as
`tini` would, so nothing in the container is left a zombie. An image that
brings `tini` or runs under `docker run --init` sets `MURU_NO_INIT=1`.

SIGTERM, which `docker stop` and Kubernetes send, stops the container in
order:

1. `/readyz` starts failing at once.
2. For `--shutdown-delay`, the listener keeps accepting and serving, with
   every connection closing after its response (`Connection: close`,
   GOAWAY on HTTP/2) and at once when idle, so clients reconnect to
   another replica.
3. The listener closes and the requests in flight finish.
4. The workers stop, closing their WebSockets with 1001, and the
   container exits 0.

Two lines mark the sequence, with `kind` values of their own in JSON logs:

```
muru: stopping: readiness now fails; serving for the shutdown delay of 5s, then draining for up to 25s
muru: stopped accepting connections; 3 still open, draining
```

The first, `serve.stopping`, comes at SIGTERM, and the second,
`serve.draining`, when the listener closes.

All of it fits inside `--shutdown-budget`; whatever has not finished by
then is killed. A budget has to cover the requests, then the workers'
`exit_grace` for their shutdown work (`--timeout exit_grace=`), then a
second for the edge. The edge stops waiting for requests `exit_grace`
plus 1 second before the end of the budget, and each worker is told how
long it has. With the defaults, a stuck request gets 5 seconds, the
workers 2 and the edge 1, inside the budget of 8. A project command running through the container, as
`kubectl exec` starts one, gets SIGTERM when the stop begins and SIGKILL
if it is still running when the budget ends, and the `muructl command run`
that started it says it was stopped. A second SIGTERM, or a second Ctrl-C,
ends the delay early.

The delay is 0 by default, which suits Docker and Compose, where a
stopping container stops receiving connections at once. Under Kubernetes
a pod goes on receiving connections for a few seconds after SIGTERM,
while the Service's endpoints and the ingress catch up, so a Deployment
sets a delay, and a budget under its `terminationGracePeriodSeconds` (30
seconds unless set):

```yaml
env:
  - {name: MURU_SERVE_SHUTDOWN_DELAY, value: "5s"}
  - {name: MURU_SERVE_SHUTDOWN_BUDGET, value: "25s"}
```

No `preStop` hook is needed. The budget is 8 seconds by default, under the
10 that `docker stop` gives before SIGKILL; an application with long
requests or a long shutdown raises both the budget and the platform's
grace period, with
`stop_grace_period` in Compose or `terminationGracePeriodSeconds` in
Kubernetes.

## Compose

A Compose service built from the image above:

```yaml
services:
  shop:
    build: .
    ports: ["8080:8080"]
    environment:
      MURU_SERVE_PROCESSES: "4"
    read_only: true
    tmpfs: [/run/muru, /tmp]
    healthcheck:
      test: ["CMD", "muructl", "probe", "ready"]
      interval: 5s
    stop_grace_period: 10s
```

The root filesystem is read-only, with the run directory and `/tmp` as
memory-backed mounts. Files in them count against the container's memory
limit when it has one, as [request bodies and /tmp in
memory](#request-bodies-and-tmp-in-memory) describes. The health check asks `muructl probe ready` inside
the container, so `docker compose ps` shows the service healthy once its
workers are ready. `docker compose exec shop muructl status` and the
other commands of [inside the container](#inside-the-container) work as
they do under `docker exec`. The admin port is not published here; publish
9090 as well for a Prometheus outside the Compose network.

For a Kubernetes Deployment, see [a Kubernetes
Deployment](kubernetes.md).
