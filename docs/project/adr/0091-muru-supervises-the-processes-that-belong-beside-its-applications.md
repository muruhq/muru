---
status: accepted
date: 2026-11-28
---

# ADR 0091: Muru supervises the processes that belong beside its applications

## Context and problem statement

An application rarely runs alone. Beside it are a cache, a queue's
workers, a scheduler, a tunnel, an exporter, and where Muru serves the
application something else has to keep those running: systemd units, a
supervisord beside the daemon, or a second container. Section 1.3 of
the specification put "general management of non-Python servers"
outside Muru's scope, and managed HTTP server releases (ADR 0014) were
admitted in a limited form for cutovers. The supervisor written for
those is general (issue 483), and a design for services that take no
requests from the edge followed (issue 484). Should Muru supervise such
processes, where is the line, and what are the choices the design left
open (issue 485)?

## Decision drivers

- Whoever runs an application on Muru should not need a second
  supervisor for what the application cannot run without.

- Every process manager grows. Each of schedules, dependencies between
  services, socket activation, resource limits, health checks with
  actions, templated instances and services run from images is
  reasonable alone, and together they are systemd.

- One supervisor and one policy: what is decided for managed servers
  (ADR 0090) is what services get.

- A service is an arbitrary command run for ever as a chosen user, so
  how one comes to be defined matters.

## Considered options

- Supervise nothing but HTTP servers, and leave the rest to the
  system's manager.

- Application services alone, the smallest thing that is plainly
  Muru's: processes a project declares, fixed with each release.

- Node services and application services both, with a stated line for
  what is left out.

- A general process runner for any project, on the lines of foreman or
  honcho.

## Decision outcome

Chosen option: "Node services and application services both, with a
stated line", because a node's cache or tunnel is as much a part of
serving its applications as a worker is, and a line that is written
down can be held. The line: **Muru supervises the processes that belong
beside the applications it serves, with the smallest policy that makes
them dependable, and a node's general system services stay with the
system's own manager.** Section 1.3 of the specification now says so.
Node services are built first (section 9.17), since they need none of
the release machinery.

The choices the design left open, as taken:

1. **Both kinds**, node services first.

2. **Never give up by default**, back off to a minute, make the loop
   loud; `give_up_after` for those who want it (ADR 0090).

3. **`always` is the default policy** (ADR 0090).

4. Logs go through the log relay managed servers have (ADRs 0059 and
   0060), which overtook the question of a file first and a relay
   later.

5. **A stop by hand outlasts a reboot.** A service stopped whole is
   held until it is started by hand. The alternative, as systemd has
   it, is that a stop lasts until the next boot.

6. **A changed definition restarts the service on apply**, and
   `--dry-run` says so first. The alternative records the change as
   pending until a restart, which is safer and makes an apply less
   declarative.

7. **A deploy does not wait for the application's services** unless
   asked to with `--wait-services`. A worker that will not start
   should not keep a fix to the web side from going out.

8. **A service is addressed as `APP/NAME`**, and a node's by its name
   alone.

9. **`command`, not `run`**, as a managed server release has. A `call`
   form for a Python callable can come later either way.

10. **The name is `service`**, though "service user", gRPC services,
    systemd's and Kubernetes' all use the word for something else.
    It is what people look for.

11. **`required` under `muructl serve`** is kept, off by default: a
    required service that fails makes the container unready.

12. **Services under `muructl run`** are in, as the last step.

13. Managed servers moved to the shared supervisor before services
    existed (issue 483).

14. **A node service is declared in applied files only**, kept in the
    state store as `[upstream.NAME]` is. A service is something the
    daemon runs, so it is a deployment (ADR 0079), and the daemon's own
    file refuses it.

15. **A copy of a service is chosen with `--index`**, numbered by
    `MURU_SERVICE_INDEX` and `{index}`, since `--instance` and
    `MURU_INSTANCE` choose a Muru instance (ADR 0074). The key stays
    `instances`.

16. **An application's services keep running while its host is stopped
    for idleness**, with `on_idle = 'stop'` for one that should stop
    with it.

Two forms were considered and left out: a services-only mode of
`muructl serve`, which would be a second supervisor under Kubernetes
with nothing to gate, and a process runner for any project, which is
outside the line.

### Consequences

- Good: a node that serves applications with Muru needs no second
  supervisor for what they depend on, and those processes have the
  restart policy, logs, events, metrics and adoption across daemon
  restarts that the applications' own have.

- Good: the line gives an answer to the requests that will come, and
  section 9.17 lists what was left out on purpose.

- Bad: Muru now runs arbitrary commands for ever, as users an operator
  names, including root. Anyone who can reach the admin socket could
  already deploy a release that does, but a service is standing where a
  release is tied to a deploy.

- Bad: services are outside every accounting Muru has. They are not in
  the node's budget, and the memory guard, which reads the whole
  cgroup, can replace only Python workers (issue 996).

- Bad: what cgroups would give, a limit on a service's memory and the
  finding of a process that left its group, is not there, and on macOS
  cannot be.

- Bad: nothing supervises while no daemon runs. A service that exits
  between a detach and the next start stays down until then, as a
  managed server's instance and a host do.

## More information

Issues 484 (the design and its map), 485 (the decisions), 486 and 487
(node services), 488 to 490 (application services, `muructl serve` and
`muructl run`) and 491 (liveness). ADR 0090 is the restart policy, ADR
0014 managed HTTP server releases. Section 9.17 of the specification
has the details fixed by the implementation, and section 1.3 the line.
