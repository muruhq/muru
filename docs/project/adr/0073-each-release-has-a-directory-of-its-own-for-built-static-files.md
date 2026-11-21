---
status: accepted
date: 2026-11-21
---

# ADR 0073: Each release has a directory of its own for built static files

## Context and problem statement

Django's `collectstatic` writes into `STATIC_ROOT`, which projects put
inside the source, and the edge served it from there through a static
entry. Under the daemon several releases may share one source
directory, so a new release's `collectstatic` overwrote the assets of
the release still draining, and a canary or a retained rollback no
longer served its own assets, though serving its own files is what a
release's static entries promise (section 4.9). The source may also not
be writable by the release's user, and under `muructl run` collected
files landed in the developer's checkout. Where do a release's built
files go, and how does a static entry name them? Sections 4.9, 9.12 and
13.8; issue 850.

## Decision drivers

- Each release serves its own built files, so a canary and a rollback
  are consistent with their code.

- Nothing is written into a source directory Muru does not own.

- A project declares the entry once and never names a physical path,
  the same under every way Muru runs.

- The edge serves the files itself, under a root daemon too.

## Considered options

- `STATIC_ROOT` inside the source, as before.

- The application's shared data directory (ADR 0072).

- A directory per release, `MURU_STATIC_DIR`, named `static:` in static
  entries and filled by a `before_fork` gate.

## Decision outcome

Chosen option: "A directory per release", because it is the only one
that keeps a release's assets with the release whatever directory it
was deployed from. The source is unsafe when releases share it and
impossible when the release's user cannot write it; the data directory
would have a canary and a rollback serve the newest release's assets.

Under the daemon it is `STATE/static/APP/RELEASE`, beside the release's
environment and bytecode, made empty when the release is created,
removed with it and counted in what `release list` shows it takes,
owned by the release's user with the edge's group alone, mode `2750`,
so what `collectstatic` writes keeps that group and the edge reads it.
Under `muructl run` it is `USERSTATE/runs/NAME-HASH/static`, beside the
run's data directory; under `muructl serve`, where the image is the
release, `SOURCE/staticfiles`, filled when the image is built and moved
by `--static-dir`. `static:` and `static:SUBDIR` name it in a static
entry's `dir` and `--static`, kept by place in the release's record and
resolved when the edge is given the entry. `collectstatic` is a
declared command with `before_fork = true`, which runs once for each
release before its workers fork. A `static:` entry whose directory is
empty or missing falls through to the application, so under `muructl
run` Django's `staticfiles_urlpatterns` serve the files until `muructl
command run collectstatic` fills it.

### Consequences

- Good: a canary, a draining release and a retained rollback each serve
  their own assets.

- Good: a project's static entry and settings are the same under the
  daemon, `muructl run` and `muructl serve`.

- Bad: every release under the daemon collects its static files anew,
  which takes disk space per release, until it is removed, and time at
  each release's first start.

- Bad: a `static:` entry that is empty serves nothing and says so only
  at a run's start, so a production release whose gate is missing falls
  through to the application rather than failing.

## More information

Issue 850, split from 847, beside 849 (ADR 0072). Sections 4.9, 9.1,
9.12, 10.9 and 13.8. `[tool.muru.serve] static_dir` replaces the
`[tool.muru] static_dir` the issue first proposed, since only
`muructl serve` reads it.
