"""Muru's analysis of an import report (design specification section 6.18).

unwrapt's profile recorder holds the facts of a load: each import call,
what it cost, and the label set in force as it ran, which
``muru._lazy_imports`` sets to the entry point being imported. unwrapt's
own analysis (``unwrapt.report``) derives the summary, the candidates
for deferral, the chains and the deferrals that were used anyway. This
module adds what only Muru knows, and writes the advice as Muru and
unwrapt configuration:

- **Who pays.** Under preload the host loads a module once, before the
  fork, and every worker shares it; deferring it moves its load into each
  worker that uses it. With per-worker imports each worker loads it as it
  starts.

- **The import check's findings.** A module that leaves a thread, a
  socket or a child process in the host makes the release fall back to
  per-worker imports, so deferring its import, which moves it after the
  fork, is a strong candidate.

- **The entry points.** Each candidate carries the target whose import
  paid for it: ``hooks``, ``entrypoint``, ``mount:NAME`` or ``module:NAME``
  for one of the ``preload_modules``, and nothing for an import outside
  any of them, by an instrumentation module, ``on_load`` or ``pre_fork``.

- **Deferrals that gain nothing.** A deferred import used before the
  fork was loaded in the host anyway.

- **The preload_modules.** The project lists a module there to have it
  loaded before the fork, so nothing loaded for an entry is advised to
  be deferred, and each entry's own time, as Muru timed it, is kept.

The result is plain data, ready to send to the controller as JSON.
"""

from __future__ import annotations

import os
from collections.abc import Iterable
from typing import Any

__all__ = ["DEFER_MIN_MS", "DEFER_SHOWN", "REWRITE_MIN_MS", "analyse"]

DEFER_MIN_MS = 1.0
"""The least a candidate must save for the analysis to advise deferring it."""

DEFER_SHOWN = 10
"""The most candidates the analysis advises deferring."""

REWRITE_MIN_MS = 10.0
"""The least a from-import in the project's own code must take for the analysis to advise it.

unwrapt cannot defer ``from X import Y``, so below Python 3.15 deferring
it means rewriting the statement as ``import X``, which is worth asking
of a project only for a large saving. A from-import in a library is
never advised: the project cannot rewrite it.
"""

CANDIDATE_MIN_MS = 0.5
"""The least an import must take to be kept as a candidate.

unwrapt's analysis of each candidate costs most of the analysis's time,
and an import of less than half a millisecond is not worth deferring:
on a load of 4,000 import calls this keeps a third of the candidates and
takes 44 ms rather than 123.
"""

PAYLOAD_VERSION = 1
"""The version of the document the analysis builds."""


def analyse(
    report: Any,
    *,
    outcome: str,
    workers: int,
    found: Iterable[Any] = (),
    root: str = "",
    targets: Iterable[dict[str, Any]] = (),
) -> dict[str, Any]:
    """Build the document Muru keeps for a load from its unwrapt report.

    `outcome` is how the load ended, as load_finished is told, `workers`
    how many workers the release starts with, `found` the import check's
    findings, `root` the directory the host runs in, under which a
    module is the project's own, and `targets` each entry point's label
    and the milliseconds its import took, timed by Muru.
    """

    from unwrapt import report as unwrapt_report

    data = report.to_dict()
    label_sets: list[dict[str, str]] = data.get("label_sets") or [{}]
    edges = {edge["id"]: edge for edge in data.get("edges", [])}

    def target(edge_id: int | None) -> str:
        edge = edges.get(edge_id) if edge_id is not None else None

        if edge is None:
            return ""

        index = edge.get("labels") or 0
        labels = label_sets[index] if index < len(label_sets) else {}
        return str(labels.get("target", ""))

    candidates: list[dict[str, Any]] = [
        dict(row)
        for row in unwrapt_report.candidates(report, min_ms=CANDIDATE_MIN_MS, packages=None)
    ]

    for candidate in candidates:
        candidate["target"] = target(candidate["edge"])

    used: list[dict[str, Any]] = [dict(row) for row in unwrapt_report.deferred_used(report)]
    preloaded = outcome in ("fork", "fallback", "refused", "check")

    findings = [
        *_fork_safety(found, candidates, root),
        *_deferrals(candidates, edges, root, preloaded, workers),
        *(_used_before_fork(used) if outcome == "fork" else ()),
    ]

    return {
        "version": PAYLOAD_VERSION,
        "labels": dict(report.labels),
        "workers": workers,
        "report": data,
        "summary": unwrapt_report.summary(report),
        "candidates": candidates,
        "chains": unwrapt_report.chains(report),
        "heavy": unwrapt_report.heavy_modules(report),
        "deferred_used": used,
        "deferred_unused": unwrapt_report.deferred_unused(report),
        "findings": findings,
        "targets": [dict(entry) for entry in targets],
    }


def _application(importer: str | None) -> bool:
    # Whether an import was made by the application rather than by Muru
    # loading an entry point.

    name = str(importer or "")
    return bool(name) and name != "__main__" and not name.startswith(("muru.", "importlib."))


def _preloaded(target: str) -> bool:
    # Whether an import was made while the host loaded one of the
    # preload_modules, which the project wants loaded before the fork.

    return target.startswith("module:")


def _own(edge: dict[str, Any] | None, root: str) -> bool:
    # Whether an import statement is in the project's own code, under the
    # directory the host runs in and not in an installed package.

    file = (edge or {}).get("file") or ""

    if not root or not file:
        return False

    root = os.path.join(os.path.realpath(root), "")
    path = os.path.realpath(file)
    return path.startswith(root) and "site-packages" not in path[len(root) :]


def _advice(importer: str | None, imported: str, own: bool) -> tuple[str, str]:
    # How to defer one import, as a sentence and as the configuration to
    # write: __lazy_modules__ in the project's own module, else an entry
    # of unwrapt's lazy_modules table.

    if own and importer:
        return (
            f"add {imported!r} to __lazy_modules__ in {importer}",
            f"__lazy_modules__ = [{imported!r}]",
        )

    entry = f'"{importer}" = ["{imported}"]'

    return (
        f"defer it in {importer} with an entry of [tool.unwrapt.lazy_modules]",
        f"[tool.unwrapt.lazy_modules]\n{entry}",
    )


def _pays(preloaded: bool, workers: int, ms: float) -> str:
    # Who pays for a module's load, as the release loads.

    if preloaded and workers == 1:
        return (
            "the host, once before the fork, shared with its worker; deferred, the worker "
            "loads it on first use"
        )

    if preloaded:
        return (
            f"the host, once before the fork, shared by the {workers} workers; deferred, "
            "each worker that uses it loads it on first use"
        )

    if workers == 1:
        return "the worker as it starts"

    return f"each of the {workers} workers as it starts, {ms * workers:.1f} ms in all"


def _deferrals(
    candidates: list[dict[str, Any]],
    edges: dict[int, dict[str, Any]],
    root: str,
    preloaded: bool,
    workers: int,
) -> list[dict[str, Any]]:
    # The candidates deferring which saves the most, each with how.

    chosen: list[tuple[float, dict[str, Any]]] = []

    for candidate in candidates:
        if (
            not _application(candidate["importer"])
            or candidate["already_lazy"]
            or _preloaded(candidate["target"])
        ):
            continue

        if candidate["unwrapt"]:
            if candidate["saving_ms"] >= DEFER_MIN_MS:
                chosen.append((candidate["saving_ms"], candidate))

            continue

        # A form unwrapt cannot defer, a from-import above all, which
        # unwrapt gives no saving: in the project's own code, and when
        # nothing else imports the module eagerly, deferring it saves
        # its whole load (issue 788).

        own = _own(edges.get(candidate["edge"]), root)
        saving = 0.0 if candidate["shared"] else candidate["cum_ms"]

        if candidate["native"] and own and saving >= REWRITE_MIN_MS:
            chosen.append((saving, candidate))

    chosen.sort(key=lambda pair: (-pair[0], pair[1]["edge"]))
    findings = []

    for saving, candidate in chosen[:DEFER_SHOWN]:
        importer, imported = candidate["importer"], candidate["imported"]
        advice, config = _advice(importer, imported, _own(edges.get(candidate["edge"]), root))

        # A form unwrapt cannot defer is deferred natively from 3.15
        # only, and needs a rewrite below it.

        if not candidate["unwrapt"]:
            advice += (
                f"; it defers natively from Python 3.15, and below it only once "
                f"rewritten as import {imported} ({candidate['reason']})"
            )

        findings.append(
            {
                "kind": "defer",
                "module": imported,
                "importer": importer,
                "target": candidate["target"],
                "ms": candidate["cum_ms"],
                "saving_ms": saving,
                "pays": _pays(preloaded, workers, saving),
                "advice": advice,
                "config": config,
            }
        )

    return findings


def _fork_safety(
    found: Iterable[Any], candidates: list[dict[str, Any]], root: str
) -> list[dict[str, Any]]:
    # The import check's findings, each a strong candidate for deferral:
    # moved after the fork, its import no longer leaves anything in the
    # host.

    by_module = {candidate["imported"]: candidate for candidate in candidates}
    findings = []

    for finding in found:
        module = getattr(finding, "module", None)

        if not module or getattr(finding, "instrumentation", None):
            continue

        candidate = by_module.get(module)
        importer = candidate["importer"] if candidate else None
        target = candidate["target"] if candidate else ""

        # A module loaded for a preload_modules entry is to be loaded
        # before the fork, so deferring it is no advice (issue 785).

        if _preloaded(target):
            advice, config = (
                f"it is loaded before the fork for preload_modules; {finding.advice}",
                "",
            )
        elif importer:
            advice, config = _advice(importer, module, False)
        else:
            advice, config = (f"defer the import of {module} to first use", "")

        findings.append(
            {
                "kind": "fork_safety",
                "module": module,
                "importer": importer,
                "target": target,
                "ms": candidate["cum_ms"] if candidate else 0.0,
                "saving_ms": candidate["saving_ms"] if candidate else 0.0,
                "pays": (
                    f"its import left {finding.detail} in the host, which cannot cross the fork"
                ),
                "advice": advice,
                "config": config,
            }
        )

    return findings


def _used_before_fork(used: list[dict[str, Any]]) -> list[dict[str, Any]]:
    # Deferred imports the host's load used, and so loaded before the
    # fork anyway: deferring them gains nothing under preload.

    findings = []

    for deferral in used:
        where = deferral["used_in"] or "the load"
        line = f" line {deferral['line']}" if deferral["line"] else ""

        findings.append(
            {
                "kind": "used_before_fork",
                "module": deferral["module"],
                "importer": deferral["importer"],
                "target": "",
                "ms": deferral["load_ms"],
                "saving_ms": 0.0,
                "pays": f"the host, before the fork: {where}{line} used it",
                "advice": (
                    f"already deferred in {deferral['importer']} but used before the fork, "
                    "so deferring it gains nothing; stop deferring it, or defer what uses it"
                ),
                "config": "",
            }
        )

    return findings
