---
status: accepted
date: 2026-11-17
---

# ADR 0066: Deferring imports is a key of Muru's own

## Context and problem statement

PEP 810's `__lazy_modules__` defers imports natively from Python 3.15,
and earlier versions ignore it. unwrapt emulates it from Python 3.11 to
3.14, and on every version defers imports a configuration lists for code
the project does not own. A project could already use it through an
instrumentation module of its own (section 6.15) or unwrapt's bootstrap
and its environment variables. Issue 740 asks whether Muru should offer
it as a setting, and how it fits the instrumentation list and the fork
(section 6.17).

## Decision drivers

- Deferral has to be in place before anything of the application is
  imported, and before the other instrumentation modules, so their
  imports can be deferred too.

- Under preload what counts is what was deferred and what of it was
  used before the fork, which only Muru knows the moment of.

- Muru's own view of a request, the modules it loaded on first use
  (section 6.16), should say which loads were deferrals.

- unwrapt stays the project's dependency, and its configuration stays
  its own, so the same file serves outside Muru.

## Considered options

- A key of Muru's own, `lazy_imports`, that puts a module of Muru's at
  the head of the instrumentation list, as `tracing` does with
  `muru._tracing`.

- Leave it to the project: an instrumentation module it writes, or
  unwrapt's bootstrap, with nothing in Muru.

- A Muru table holding unwrapt's settings, the lazy modules and the
  file, which Muru passes to unwrapt.

## Decision outcome

Chosen option: "a key of Muru's own", because the module can be placed
first, ahead of tracing, where a project's own module or a bootstrap
cannot be ordered against Muru's; because it can report at the fork and
mark the workers' loads; and because it leaves unwrapt's configuration
in unwrapt's own file, found by unwrapt's own rules, rather than a
second form of it in Muru's settings. The key is a flag. The release
record keeps it as a flag and the host is given the module when the
release starts, so an export shows `lazy_imports` rather than an entry
in the instrumentation list.

### Consequences

- Good: one key turns deferral on, in a project, on one machine or for
  a declared release, and `--print-config` names the file unwrapt
  finds.

- Good: the log line at the fork and the deferred mark in the Timing
  panel show whether a deferral saved anything.

- Bad: the line and the mark know only unwrapt's deferrals, so from 3.15
  the interpreter's own deferrals for `__lazy_modules__` are not
  reported as such.

- Bad: a project that wants a different unwrapt file under the daemon
  names it with `UNWRAPT_CONFIG` in its environment, since Muru has no
  key for it.

## More information

Section 6.17 of the specification, issue 740, and ADR 0064 for the
instrumentation layer the module belongs to. Issue 741 adds the import
report on top of it.
