---
status: accepted
date: 2026-11-21
---

# ADR 0072: An application has a data directory shared by its releases

## Context and problem statement

A release is a source directory Muru points at and does not own, and
several releases may share one (ADR 0003, section 9.1). Files an
application keeps from one release to the next, a SQLite database,
files saved from uploads, generated reports, had nowhere to go: into one
release's source, where the next release cannot see them, or into a
directory the operator invented and configured for every release. An
application's working directory is its release's source, so the only
way to tell it where such files go is the environment. Where do they go
in each way Muru runs, which of them may a client reach, and how does
the application learn of it? Sections 4.9, 4.11, 8.7 and 13.8; issue
849.

## Decision drivers

- One answer that works under the daemon, `muructl run` and `muructl
  serve`, so the application's settings are written once.

- Nothing a client can reach by mistake: a database, its `-wal` and
  `-shm` files and files kept but never meant to be handed out must
  never be servable, whatever a configuration says.

- A development run never writes into the project, which is usually a
  git checkout.

- Code that runs outside Muru, without the `muru` package, keeps
  working.

- An operator can move a part to another disk without changing the
  configuration.

## Considered options

- Symlink parts of a shared directory into each release, as
  Capistrano's `shared/` does.

- One directory per application, `MURU_DATA_DIR`, with two parts Muru
  makes, `media` (served through file hints and declared static entries)
  and `uploads` (never served), named by exposure and given by
  environment variables.

- The same, with parts named `downloads` and `uploads`, by direction.

- A function in the `muru` package saying where the directory is.

## Decision outcome

Chosen option: "One directory per application with media and uploads
parts", because it is the only one that needs nothing written into a
release's source, keeps what may be served apart from what may not by
the part a file is in, and reaches the application through a variable
it reads with a fallback of its own. A release's source is not Muru's
to write into, so symlinks into it are out. An avatar is uploaded and
then downloaded, so direction does not say which files may be served;
`media` matches Django's `MEDIA_ROOT`. A function in `muru` would not
work outside Muru, where the package is often not installed.

The data directory is `STATE/data/APP` under the daemon, moved by the
application's `[app.NAME] data_dir`, `media_dir` and `uploads_dir`; a
development run's is `USERSTATE/runs/NAME-HASH/data`, the hash of the
project's resolved path keeping two checkouts apart, moved by
`--data-dir`; `muructl serve` has one only when it is named, normally a
volume, and checks it exists. `release prune` and `release remove`
never touch it, and `app remove` keeps it unless `--data`. The media
part is a file hint root of every mount by default; `media:` and
`media:SUBDIR` name it in `sendfile_roots` and static entries, `data:`
and `uploads:` are refused, and the data directory, the uploads part
and any directory containing either can never be a root or a static
directory, checked when a release is created and at each start. Under a
root daemon the media part is a root Muru provides, which must resolve
to a directory owned by the application's user (issue 847). Ownership
is the operator's once a part exists. `upload.save()` saves into the
uploads part by default. `muructl data show`, `muructl data check` and
`muructl status --app` report it.

### Consequences

- Good: a SQLite database survives every release, wherever each is
  deployed from, and a hint into media needs no configuration.

- Good: what is served is decided by the part a file is in, so a
  mistaken root or static entry cannot reach a database.

- Good: the application's settings are one line per framework, the
  variable with a fallback, under every server.

- Bad: two releases have the same database open during a drain, a
  canary or a retained rollback, so a database there needs WAL mode
  and schema changes both releases can live with, which the guide says.

- Bad: a development run's data directory is out of sight in the user
  state location, which the run prints at start to make up for it.

## More information

Issue 849, split from 847 (opening beneath a root) and beside 848 (ADR
0071, a hint opened by the worker) and 850 (the per-release static
directory). Sections 4.9, 4.11, 8.7, 9.1, 9.12, 10.9 and 13.8,
appendices C and D. Left open: a disk quota on the data directory, and
a backup the daemon offers.
