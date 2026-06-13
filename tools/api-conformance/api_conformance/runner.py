"""The dual-run comparison and the JSON report it produces.

For each route case the runner issues the identical request to the native
control front and to the residual Python handler, then asserts the two responses
are byte-faithfully equal after volatile masking and JSON canonicalization:

* the status code is exactly equal;
* the allowlisted response headers are equal;
* the body is byte-equal after JSON-canonicalize + volatile masking — or, for a
  server-sent-event route, the frame sequence is equal after the same masking.

A route case may carry a paired and an unpaired header variant; the runner diffs
each present variant and the route passes only when every variant matches. A
case flagged ``require_sandbox`` is skipped by default (it has side effects).

The report mirrors the sibling durable-store harness: a per-route verdict folded
into a whole-run ``Report``, emitted as JSON, with ``ok`` true only when every
diffed variant on every route passed.
"""

from __future__ import annotations

from pydantic import BaseModel

from .client import Clients, Probe
from .normalize import compare_headers, normalize_body, normalize_sse_frames
from .route_cases import RouteCase


class Mismatch(BaseModel):
    """One concrete difference between the native and Python responses."""

    kind: str  # "status" | "header" | "body" | "frames" | "reachability"
    detail: str


class VariantResult(BaseModel):
    """The verdict for one header variant (paired or unpaired) of a route."""

    variant: str  # "paired" | "unpaired"
    status: str  # "pass" | "fail" | "skipped"
    native_reachable: bool
    python_reachable: bool
    native_status: int | None = None
    python_status: int | None = None
    mismatches: list[Mismatch] = []


class CaseResult(BaseModel):
    """The aggregate verdict for one route case."""

    route: str
    method: str
    path: str
    is_sse: bool
    sandboxed: bool
    status: str  # "pass" | "fail" | "skipped"
    variants: list[VariantResult]


class Report(BaseModel):
    """The whole-run report."""

    ok: bool
    strict: bool
    passed: int
    failed: int
    skipped: int
    routes: list[CaseResult]


def assert_response_equal(
    native: Probe, python: Probe, case: RouteCase
) -> list[Mismatch]:
    """Diff two responses for one route case; return the list of mismatches.

    An empty list means the two responses are byte-faithfully equal (modulo
    volatile fields). The checks run in order — reachability, then status, then
    headers, then body/frames — and an unreachable side short-circuits to a
    single reachability mismatch (there is nothing to diff against a side that
    never answered).
    """
    mismatches: list[Mismatch] = []

    # Reachability is the precondition for every later check: a side that could
    # not be reached has no status/headers/body to compare.
    if not native.ok or not python.ok:
        detail = (
            f"native_reachable={native.ok} (err={native.error}), "
            f"python_reachable={python.ok} (err={python.error})"
        )
        mismatches.append(Mismatch(kind="reachability", detail=detail))
        return mismatches

    if native.status != python.status:
        mismatches.append(
            Mismatch(
                kind="status",
                detail=f"native={native.status} != python={python.status}",
            )
        )

    header_diffs = compare_headers(native.headers, python.headers)
    for diff in header_diffs:
        mismatches.append(Mismatch(kind="header", detail=diff))

    if case.is_sse:
        nframes = normalize_sse_frames(native.frames)
        pframes = normalize_sse_frames(python.frames)
        if nframes != pframes:
            mismatches.append(
                Mismatch(
                    kind="frames",
                    detail=(
                        f"frame sequence differs: native={len(nframes)} frames, "
                        f"python={len(pframes)} frames"
                    ),
                )
            )
    else:
        nbody = normalize_body(native.body, case.extra_volatile)
        pbody = normalize_body(python.body, case.extra_volatile)
        if nbody != pbody:
            mismatches.append(
                Mismatch(
                    kind="body",
                    detail=(
                        f"normalized body differs: native={nbody!r} != "
                        f"python={pbody!r}"
                    ),
                )
            )

    return mismatches


def _run_variant(
    clients: Clients, case: RouteCase, variant: str, headers: dict[str, str]
) -> VariantResult:
    """Issue one header variant of a route to both transports and diff them."""
    if case.is_sse:
        native = clients.stream_native(
            case.method, case.path, headers, case.body, case.content_type
        )
        python = clients.stream_python(
            case.method, case.path, headers, case.body, case.content_type
        )
    else:
        native = clients.request_native(
            case.method, case.path, headers, case.body, case.content_type
        )
        python = clients.request_python(
            case.method, case.path, headers, case.body, case.content_type
        )
    mismatches = assert_response_equal(native, python, case)
    return VariantResult(
        variant=variant,
        status="pass" if not mismatches else "fail",
        native_reachable=native.ok,
        python_reachable=python.ok,
        native_status=native.status if native.ok else None,
        python_status=python.status if python.ok else None,
        mismatches=mismatches,
    )


def run_case(clients: Clients, case: RouteCase) -> CaseResult:
    """Run conformance for a single route case (every present header variant).

    A sandboxed case is reported ``skipped`` without issuing any request, so a
    write route never mutates a live agent during a default run.
    """
    if case.require_sandbox:
        return CaseResult(
            route=case.name,
            method=case.method,
            path=case.path,
            is_sse=case.is_sse,
            sandboxed=True,
            status="skipped",
            variants=[],
        )

    variants: list[VariantResult] = []
    # The unpaired variant always runs (the request shape every route supports);
    # the paired variant runs only when the case supplies its own header set, so
    # a route whose body is identical paired-or-not is diffed once.
    variants.append(
        _run_variant(clients, case, "unpaired", dict(case.unpaired_headers))
    )
    if case.paired_headers is not None:
        variants.append(
            _run_variant(clients, case, "paired", dict(case.paired_headers))
        )

    status = "pass" if all(v.status == "pass" for v in variants) else "fail"
    return CaseResult(
        route=case.name,
        method=case.method,
        path=case.path,
        is_sse=case.is_sse,
        sandboxed=False,
        status=status,
        variants=variants,
    )


def run_conformance(
    clients: Clients, cases: list[RouteCase], strict: bool = False
) -> Report:
    """Run every route case and fold the per-case verdicts into one report.

    ``ok`` is true when no diffed route failed. By default a skipped (sandboxed)
    route does not affect ``ok``; under ``strict`` a skipped route also fails the
    run, so an on-rig run can demand that every listed route was actually
    exercised.
    """
    results = [run_case(clients, case) for case in cases]
    passed = sum(1 for r in results if r.status == "pass")
    failed = sum(1 for r in results if r.status == "fail")
    skipped = sum(1 for r in results if r.status == "skipped")
    ok = failed == 0 and (not strict or skipped == 0)
    return Report(
        ok=ok,
        strict=strict,
        passed=passed,
        failed=failed,
        skipped=skipped,
        routes=results,
    )
