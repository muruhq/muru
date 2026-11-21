---
status: accepted
date: 2026-11-21
---

# ADR 0071: A path hint from a worker is opened by the worker

## Context and problem statement

An `X-Muru-Sendfile` path hint (section 4.9) was opened by the edge, as
the edge's user. Under a root daemon the edge runs as the service user
and each application as its own (ADR 0026), so a hint worked only for
files the edge's user could read: a file written `0644` was readable by
every user on the machine, and one written `0600`, as anything writing
through `mkstemp` and a rename does, could not be served at all. The
edge also read files on behalf of applications whose own user might not
be able to read them, which made the race of issue 847 matter. The
security model's aim, that the edge never needs application file access
(section 12), held for descriptor hints alone. Where should a worker's
path hint be opened, and how does the edge then serve it as a static
file?

## Decision drivers

- The edge should need no access to an application's files, whatever
  user each runs as.

- A path hint keeps full static-file behaviour: Range, conditional
  requests, ETag and Last-Modified, HEAD, the content type and
  precompressed variants, which a descriptor hint leaves to the
  application.

- What a hint may name stays one rule, `sendfile_roots`, whatever
  answers the route, and still catches a hint built from a request
  parameter.

- Applications change nothing: Flask's `USE_X_SENDFILE`,
  django-sendfile2's backend and a bare header keep working under WSGI
  and ASGI alike.

## Considered options

- Keep opening in the edge, and give it access through a group shared
  with the edge and a worker umask of `0027`, or a default ACL.

- Open in the worker and send the descriptor with the head's
  outcome and a FILE frame flagged as a path hint's, the edge applying
  static-file behaviour to it.

- Open in the worker and drop `sendfile_roots` there, since a worker's
  code can open any file it can read anyway.

## Decision outcome

Chosen option: "Open in the worker", because only it lets the edge read
nothing of an application's files while keeping the static-file
behaviour and the one rule for what a hint may name. A shared group or
an ACL is defeated by any library that creates a file `0600`, since the
ACL is capped by the mode the file is created with. Dropping the roots
in the worker would lose the protection against a traversal bug and
make what a hint may name depend on what answers the route.

The host is started with each mount's roots and second header. The
worker's writer takes the hint from the response head, removes every
hint header, opens the file beneath the mount's roots with the
resolution the edge uses for its own files (`muru_common::beneath`, issue
847), and sends the head with a `PathHint` saying what became of it. An
opened file follows on a FILE frame with the `PATH_HINT` flag, and its
precompressed variant, when there is one, on a second; a refused or
missing file is the head alone, which the edge answers with
`sendfile_refused` or `sendfile_missing` as before. The worker picks the
variant from the encodings the request head lists, which the edge fills
from its compression policy for the client, so the choice is the one
the edge would make. The edge's snapshot no longer carries a release's
or a mount's file hint keys. Upstreams and managed servers are unchanged:
their hints are opened by the edge, beneath their roots, with the same
resolution.

### Consequences

- Good: under the daemon a hinted file needs to be readable by the
  application's user alone, and the edge's user needs nothing; a file
  written `0600` is served.

- Good: a hint from a worker gets precompressed variants, which the
  edge's own hints never had.

- Good: under `muructl run` and `muructl serve`, where the edge and the
  application are one user, the behaviour is the same, through one code
  path.

- Bad: the request head grows by the list of encodings for GET and
  HEAD requests with an `Accept-Encoding` the policy serves, used only
  by the responses that carry a hint.

- Bad: each worker holds its own open roots, a descriptor each, and
  opens a variant beside every hinted file it serves.

## More information

Section 4.9 (static files and file hints), section 5.7 (the FILE frame
and its `PATH_HINT` flag), section 6.6 and section 12 (file access).
Issues 847 (opening beneath a root, the resolution shared by edge and
worker) and 848 (this decision). The data directory whose media part is
a default root (issue 849) and the per-release static directory (issue
850) build on both.
