---
status: accepted
date: 2026-11-27
---

# ADR 0083: The runtime location holds sockets and nothing else

## Context and problem statement

On 27 November 2026 two instances that had run on a Mac for five days
were missing from `muructl instance list` while both still served. macOS
runs `/usr/libexec/tmp_cleaner` every night, which deletes every regular
file under `/tmp` that nothing has touched for three days, and then every
directory left empty for three days. Sockets are not regular files and
are left. A user's runtime location on macOS is `/tmp/muru-UID`, chosen
because a Unix socket's path is limited to 103 bytes there, and Muru kept
regular files in it that are written once and never touched: the instance
registry's `lock` and `instance.json`, `daemon.lock`, `daemon.pid`,
`edge.toml`, the rate limits' buckets, and each release's `host.toml` and
`relay.toml`. So an instance lost its registry entry, its run directory's
lock, its pid file and the records a restarted daemon re-adopts its edge
and hosts from, while it ran (issue 948, section 13.8).

The same layout had a second fault. A worker's socket was at
`RUN/apps/APP/RELEASE/r1/worker-N.sock`, which grows with the
application's name and the release's ID. A name of 24 characters with a
commit hash as the ID comes to 108 bytes under `/tmp/muru-501`, and the
check on the run directory allowed for an ID of four characters, so the
release failed when its host tried to bind. A test confirmed it against
0.1.0a36: `binding .../r1/relay.sock` fails.

Linux is exposed to the first fault only where `XDG_RUNTIME_DIR` is unset
and the fallback `/tmp/muru-UID` is used, and to the second everywhere.

## Decision drivers

- An instance that runs for weeks must lose nothing to a cleaner, with no
  timer of Muru's own touching files to keep them alive: the maintainer
  ruled that workaround out.

- A socket's path must be short whatever an application, a release or a
  group of workers is called, so that a run directory accepted at start
  never fails later.

- One rule for every mode and platform, so that the code, the
  documentation and a diagnostic command describe one layout.

- The instance registry is found by every process of a user, whatever
  environment it was started with.

## Considered options

- Touch the files from a timer, so the cleaner never finds them old.

- Move the whole runtime location on macOS to a place nothing cleans.

- Keep sockets in the runtime location and move every regular file to
  the state location, with socket paths that carry no name.

## Decision outcome

Chosen option: "Keep sockets in the runtime location and move every
regular file to the state location", because it removes the cause on
every platform and fixes the path length at the same time. Touching the
files was ruled out. Moving the runtime location fails on the path limit:
`~/Library/Application Support/muru` alone is over 40 bytes before a user
name.

The points the issue left to settle were decided as follows.

- **The layout.** A socket's path is the run directory plus at most 30
  bytes. A host's sockets are in `RUN/h/N`, where N is the release's
  number; the edge's are in `RUN/e`; a detached command run's relay binds
  in `RUN/c/NAME`. Every regular file is in `run` under the state
  directory, in the same shape: `STATE/run/daemon.pid`, `edge.toml`,
  `edge/rate-buckets-v1`, and `h/N/` with `host.toml` or `servers.toml`,
  `relay.toml`, the secrets delivered as files and what the host and the
  daemon leave for each other (`gates`, `stop-by`, `imports.json`).

- **The release's number.** The store gives each release a number from a
  count that only goes up, so a number never names two releases of a
  store (schema version 22; the releases of an older store are numbered
  oldest first when it is opened). Two alternatives were set aside: the
  row's `rowid`, which `VACUUM` may change, and a number picked when a
  host starts, which a restarted daemon would have to find again from
  files. A development run has no store and numbers its hosts 1, 2 and
  so on, as it did.

- **Named groups.** A named group's sockets carried the group's name.
  They carry its place among the named groups instead, from 1:
  `worker-1-0.sock`. The issue's figure of 27 bytes below the run
  directory left this out; with it the figure is 30
  (`/h/99999/worker-99-99-g99.sock`), and it is the overhead the check on
  the run directory uses.

- **The edge's sockets** stay in a directory of their own, `RUN/e`,
  where the issue's tree showed them at the top. Under a root daemon the
  edge binds them as the service user, which needs a directory it owns.

- **The run directory's lock** is an exclusive `flock` on the directory
  itself, opened for reading, since a lock file there is one more regular
  file under `/tmp`. The issue's tree had `daemon.lock` under
  `STATE/run`, which would lock nothing a second daemon with another
  state directory looks at. The state directory keeps its `daemon.lock`.
  A refusal over the run directory names its admin socket, which is in
  it, and the pid when the registry has it.

- **The registry** is `registry` in the account's state location
  (`~/Library/Application Support/muru` on macOS, `~/.local/state/muru`
  on Linux), where the home directory is the password database's and
  neither `HOME`, `XDG_STATE_HOME`, `MURU_STATE_DIR` nor `--state-dir`
  moves it. Before, it was independent of the environment because `/tmp`
  is; tests, and any tool that starts Muru with another `HOME`, rely on
  that. A user with no home directory, as a container given an arbitrary
  user ID has, keeps it in the runtime location as before, where nothing
  cleans by age inside a container.

- **System mode.** The same split: sockets in `/run/muru`, files in
  `/var/lib/muru/run`. `/run/muru` is not cleaned by age, so this is for
  one rule and the path length, not for the cleaner. The system registry
  alone stays where it was, `/run/muru/registry`: every user must be able
  to read it, which the system state directory does not allow, and it is
  emptied at boot as the instances it lists are.

- **`muructl serve`** keeps its files with its sockets, under `release`
  in its run directory, which is the container's and is not cleaned by
  age; its hosts' directories are numbered as everywhere else.

- **A development run** keeps its sockets in `RUN/run-ID` and its files
  in `run/run-ID` under the user state location: its pid file and its
  hosts' files. Its registry record names both. A run killed outright
  leaves the files' directory, which the state location has no cleaner
  for, so each run that starts removes those no live registry entry
  names, and `muructl instance prune` removes a stale run's.

- **Stale files after a reboot.** The state location survives a reboot
  and the runtime location does not. A daemon that starts removes the
  host directories of every release the store does not say was running,
  and the files of a release it could not take over. The edge's record
  is checked as before, by connecting to the socket it names.

- **Empty directories.** A directory under `/tmp` with nothing in it is
  removed by age too, so each is made when it is needed
  (`HostDirs::create`, the edge's at each start, a command run's when
  its relay starts) and none is assumed.

- **Upgrading from the earlier layout.** No compatibility is kept, and
  the two places that would fail badly say so instead. `muructl daemon
  upgrade` refuses a daemon whose run directory holds a `daemon.lock`
  file, which only the earlier layout makes: this version would start
  without that daemon's edge and hosts. A daemon that finds `edge.toml`
  or `apps` in its run directory refuses to start, since those are an
  edge and hosts it cannot take over. Both name `muructl daemon stop
  --shutdown`. An instance still running under the earlier layout is not
  listed, since its registry entry is in the earlier place.

### Consequences

- Good: nothing an instance needs while it runs is under `/tmp` as a
  regular file, on any platform.

- Good: a run directory that passes the check at start never fails for
  what a release is called, and the check's message no longer speaks of
  room for a name.

- Good: secrets delivered as files move from `/tmp` to the state
  directory on macOS.

- Bad: on Linux those secret files move from `/run`, which is memory, to
  the state directory, which is disk. They are removed when the host
  stops, and a start removes those a crash left.

- Bad: a path under the run directory no longer says which release it
  belongs to. `muructl doctor` (issue 950) lists each socket with what
  it is for.

- Bad: an upgrade across this change needs the daemon stopped and its
  releases with it.

## More information

Section 13.8 of the specification ("Sockets and runtime files") has the
trees and the table of path lengths. Issue 948 has the incident. This
supersedes the part of [ADR
0069](0069-instances-are-found-through-a-registry-of-entries-each-owned-by-its-instance.md)
that puts the registry in the user's runtime location; the rest of that
record stands.
