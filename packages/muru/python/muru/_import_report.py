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

The result is plain data, ready to send to the controller as JSON.
"""

from __future__ import annotations

import os
import sys
from collections.abc import Iterable
from typing import Any

__all__ = ["DEFER_MIN_MS", "DEFER_SHOWN", "analyse"]

DEFER_MIN_MS = 1.0
"""The least a candidate must save for the analysis to advise deferring it."""

DEFER_SHOWN = 10
"""The most candidates the analysis advises deferring."""

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
) -> dict[str, Any]:
    """Build the document Muru keeps for a load from its unwrapt report.

    `outcome` is how the load ended, as load_finished is told, `workers`
    how many workers the release starts with, `found` the import check's
    findings and `root` the directory the host runs in, under which a
    module is the project's own.
    """

    from unwrapt import report as unwrapt_report

    from muru import preload

    data = report.to_dict()
    label_sets: list[dict[str, str]] = data.get("label_sets") or [{}]
    edges = {edge["id"]: edge for edge in data.get("edges", [])}

    # unwrapt takes an import's file and line from the frame that called
    # its hook, which under the import check is the check's own wrapper of
    # builtins.__import__ (section 6.2). The file is the importing
    # module's, and the line is not known.

    _correct_files(edges.values(), preload.__file__)

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
    }


def _correct_files(edges: Iterable[dict[str, Any]], wrapper: str | None) -> None:
    # An edge recorded from a wrapper's frame gets its importer's file.

    for edge in edges:
        if wrapper is None or edge.get("file") != wrapper:
            continue

        module = sys.modules.get(edge.get("importer") or "")
        edge["file"] = getattr(module, "__file__", None)
        edge["line"] = None


def _application(importer: str | None) -> bool:
    # Whether an import was made by the application rather than by Muru
    # loading an entry point.

    name = str(importer or "")
    return bool(name) and name != "__main__" and not name.startswith(("muru.", "importlib."))


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

    chosen = [
        candidate
        for candidate in candidates
        if _application(candidate["importer"])
        and not candidate["already_lazy"]
        and (candidate["unwrapt"] or candidate["native"])
        and candidate["saving_ms"] >= DEFER_MIN_MS
    ]
    chosen.sort(key=lambda candidate: (-candidate["saving_ms"], candidate["edge"]))
    findings = []

    for candidate in chosen[:DEFER_SHOWN]:
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
                "saving_ms": candidate["saving_ms"],
                "pays": _pays(preloaded, workers, candidate["saving_ms"]),
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
        advice, config = (
            _advice(importer, module, False)
            if importer
            else (f"defer the import of {module} to first use", "")
        )

        findings.append(
            {
                "kind": "fork_safety",
                "module": module,
                "importer": importer,
                "target": candidate["target"] if candidate else "",
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
