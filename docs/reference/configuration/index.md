# Configuration

Every key of every configuration file Muru reads, with its type and what it
does. The pages are generated from the definitions `muructl` parses the
files with, so they list exactly the keys it accepts; an unknown key is
refused, with its line.

| File | Where it is | Page |
| --- | --- | --- |
| The daemon's configuration | `/etc/muru/muru.toml` for the system daemon; for a user's, `~/.config/muru/muru.toml` on Linux or `~/Library/Application Support/muru/muru.toml` on macOS (see [the daemon](../../howto/daemon.md)) | [The daemon's configuration](daemon.md) |
| Applied files | Any file `muructl apply` is given; `muru.deploy.toml` when it is kept beside a project's code | [Applied files](deployment.md) |
| The project's settings | `[tool.muru]` in the project's `pyproject.toml`, or the same keys at the top of a `muru.toml` beside it; or the file `muructl serve --config` names in their place | [Project](project.md) |
| A developer's own settings | `muru.local.toml` beside the project | [muru.local.toml](local.md) |

Two of these are called `muru.toml` and hold different keys. The
daemon's is in its configuration directory, and a project's is beside
its code. A file for `muructl apply` that is kept beside a project's
code is named `muru.deploy.toml`, so it is never read as the project's
settings.

The daemon's configuration says how the daemon runs, and applied files
say what it serves. Each has its own tables, and a table of one in the
other is refused: a listener in an applied file, or an application in
the daemon's file. Client classes, rate limits and conditions may be in
either: those the daemon's file defines are the node's, and an applied
file may use them and may not define one of the same name.

A project with both a `muru.toml` and a `[tool.muru]` table in its
`pyproject.toml` uses the `muru.toml`. The table is not read and the two
are not merged, which `muructl run`, `muructl serve`, `muructl check`
and `muructl deploy` say when they find both.

A relative path in a file given to `muructl apply` is taken from that
file's directory, whichever key it is in, except a release's own static
directories and error pages, which are taken from the release's source
directory. Apply sends the daemon the absolute paths, which
`muructl deployment show`, `muructl overlay preview` and the plans show. In
a project's settings (`[tool.muru]` or `muru.toml`) a relative path is
taken from the source directory, which under `muructl run` is the
project directory. In the daemon's own file the client classes' list
files are taken from its directory, and every other path is absolute.

A duration is a number of seconds, or a number with a unit, `ms`, `s`,
`m`, `h`, `d` or `y`: `250ms`, `1.5s`, `10m`. A size is a number of bytes,
or a number with `KB`, `MB` or `GB` (or `K`, `M` or `G`), which count in
powers of 1024: `512KB`, `10MB`. `NAME`, `ID` and `STATUS` in a table's path stand for the key you
choose: an application's name, a release's identifier, an HTTP status.

## JSON Schema

Each file has a JSON Schema, for an editor that checks TOML against one,
such as one using the Taplo language server:

| File | Schema |
| --- | --- |
| The daemon's configuration | `https://docs.muruhq.dev/en/latest/schema/daemon.schema.json` |
| Applied files | `https://docs.muruhq.dev/en/latest/schema/deployment.schema.json` |
| `[tool.muru]` and `muru.toml` | `https://docs.muruhq.dev/en/latest/schema/project.schema.json` |
| `muru.local.toml` | `https://docs.muruhq.dev/en/latest/schema/local.schema.json` |

A comment on a file's first line points Taplo at its schema:

```toml
#:schema https://docs.muruhq.dev/en/latest/schema/daemon.schema.json
```

```{toctree}
:hidden:

daemon
deployment
project
local
```
