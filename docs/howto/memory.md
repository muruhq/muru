# Memory limits

A worker that grows too large is replaced through the same path as
[worker recycling](recycling.md), so no request is lost. Each worker
measures its own memory at `check_interval` and reports it to the host,
which holds it to two limits:

| Setting | Default | Meaning |
| --- | --- | --- |
| `measure` | `uss` | what is measured: `uss`, `pss` or `rss` |
| `soft_limit` | off | over this for `soft_for`, the worker reclaims and then, still over, is recycled |
| `soft_for` | `2m` | how long a worker may stay over the soft limit |
| `reclaim_first` | `true` | run a garbage collection and trim the heap before recycling |
| `hard_limit` | off | over this the worker leaves routing at once and is replaced |
| `check_interval` | `5s` | how often memory is measured |

```
muructl run app:application --memory soft_limit=800MB --memory hard_limit=1200MB
muructl deploy shop /srv/shop --memory soft_limit=800MB --memory measure=rss
```

```toml
[tool.muru.memory]
measure = "uss"
soft_limit = "800MB"
soft_for = "2m"
reclaim_first = true
hard_limit = "1200MB"
check_interval = "5s"
```

The same table goes in a declared release for `muructl apply`. Sizes are `800MB`, `1.5GB` or a
number of bytes, with KB, MB and GB as powers of 1024.

The node-wide memory budget is in [scaling](scaling.md), and a [worker
group](worker-groups.md#timeouts-memory-and-recycling-per-group) can have limits
of its own.

## What is measured

The unique set size (USS) counts only the pages no other process shares,
which is the memory a worker really adds, since the pages it inherited
from the preloaded host are shared copy-on-write until written. The
proportional set size (PSS) splits shared pages between the processes
that share them, and the resident set size (RSS) counts every resident
page. On Linux the figures come from the kernel's own accounting; on
macOS the resident size is reported, and the task's physical footprint
stands in for USS and PSS. The resident size on macOS leaves out memory
the system has compressed, so a worker that holds memory without
touching it can measure well under what it has allocated; the physical
footprint counts compressed memory.

## What happens

A short spike over the soft limit does nothing: only a worker still over
it after `soft_for` is acted on. With `reclaim_first` the host asks the
worker to collect garbage and trim its heap (with `malloc_trim` on
glibc) and measures once more; a worker still over is recycled, with a
`worker.recycled` event that names the measure, the figure and the
limit. Over the hard limit a worker is treated as the hard timeout
treats a stuck one: it leaves routing at once, a replacement is forked,
and it is told to stop after `hard_drain_grace`.

A worker index recycled for memory three times within an hour raises a
`memory.leak_suspected` warning, once an hour.

## Under a container's memory limit

The limits above are each worker's own. When the workers run in a
cgroup with a memory limit, as in a container, a memory guard also
watches the limit itself: when less than the headroom is left under it
(10% of the limit, at least 128MB), the largest worker is replaced, one
at a time, whether or not a hard limit is set. Both can apply, the hard
limit to one worker that grows too large, and the guard to workers that
together use more than the container has. A worker replaced for memory,
by either, stops before its replacement starts when less is left under
the limit than the worker's current size.

[Container limits](containers.md#container-limits) describes the guard,
where a container's memory goes, and what to set for an application that
holds a lot once it is imported. The kernel's out-of-memory killer
remains the last resort. On Linux each worker raises its own
`oom_score_adj` to 500 as it starts, so when memory runs out the kernel
kills a worker, which its host replaces, before a host, the edge or
another service on the machine.

## Metrics

Under the daemon each worker's last report is served on `/metrics`:
`muru_worker_memory_bytes` by measure, `muru_worker_requests_total` and
`muru_worker_requests_in_flight`, labelled with the application, release,
group and worker index; the [metrics guide](metrics.md) lists them.
