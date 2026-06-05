"""The dual-run comparison and the JSON report it produces.

For each route the runner pulls the store rows for the table, applies the route's
row filter, and classifies every field:

* ``pass`` — the store serves the field (the column is present on the matched
  rows, or the metric name appears in the metric rows).
* ``fail`` — the store has rows for the table but a field column is absent: a
  genuine schema gap the producer must close.
* ``missing-producer`` — no rows match (no producer is emitting yet); for a
  metric, the specific metric name is absent from the metric rows.

When a route has a legacy handler, the runner also records, per field, whether
the legacy surface served the mapped legacy field, documenting that the store is
a superset of what legacy exposed. Legacy is informational: the store is the
source of truth, so a legacy field the store covers is a pass regardless.
"""

from __future__ import annotations

from pydantic import BaseModel

from .client import Fetcher
from .routes import FieldSpec, Locator, RouteSpec


class FieldResult(BaseModel):
    """The per-field verdict for one route."""

    field: str
    locator: str
    classification: str
    producer: str
    status: str  # "pass" | "fail" | "missing-producer"
    legacy_field: str | None = None
    legacy_present: bool | None = None


class RouteResult(BaseModel):
    """The aggregate verdict for one route."""

    route: str
    kind: str
    logd_reachable: bool
    logd_rows: int
    matched_rows: int
    legacy_reachable: bool | None = None
    observability_reachable: bool | None = None
    fields: list[FieldResult]


class Report(BaseModel):
    """The whole-run report."""

    ok: bool
    strict: bool
    passed: int
    failed: int
    missing_producer: int
    routes: list[RouteResult]


def _row_matches(row: dict, row_match: dict[str, str]) -> bool:
    """True when ``row`` equals every key/value in the route's filter."""
    return all(str(row.get(k)) == str(v) for k, v in row_match.items())


def _field_present(field: FieldSpec, rows: list[dict]) -> bool:
    """Whether a column / detail / signal field is present on any matched row."""
    if field.locator == Locator.ROW_KEY:
        return any(field.field in row for row in rows)
    if field.locator == Locator.DETAIL_KEY:
        return any(
            isinstance(row.get("detail"), dict) and field.field in row["detail"]
            for row in rows
        )
    if field.locator == Locator.SIGNAL:
        return any(
            isinstance(row.get("signals"), dict) and field.field in row["signals"]
            for row in rows
        )
    return False


def field_status(
    field: FieldSpec, rows_of_kind: list[dict], matched_rows: list[dict]
) -> str:
    """Classify one field against the store rows.

    A ``metric`` field is its own row, so it is either present (``pass``) or its
    producer is not emitting (``missing-producer``); there is no column-gap case.
    For column / detail / signal fields, no matched rows means the producer is
    not emitting (``missing-producer``); rows present but the field absent is a
    real schema gap (``fail``).
    """
    if field.locator == Locator.METRIC:
        names = {row.get("metric") for row in rows_of_kind}
        return "pass" if field.field in names else "missing-producer"
    if not matched_rows:
        return "missing-producer"
    return "pass" if _field_present(field, matched_rows) else "fail"


def _legacy_present(field: FieldSpec, legacy_rows: list[dict] | None) -> bool | None:
    """Whether the legacy surface served the mapped legacy field, or ``None`` when
    the field has no legacy mapping or the legacy surface was unreachable."""
    if field.legacy_field is None or legacy_rows is None:
        return None
    return any(field.legacy_field in row for row in legacy_rows)


def run_route(fetcher: Fetcher, route: RouteSpec) -> RouteResult:
    """Run conformance for a single route."""
    rows = fetcher.logd_query(route.logd_params)
    logd_reachable = rows is not None
    rows = rows or []
    matched = [r for r in rows if _row_matches(r, route.row_match)]

    legacy_rows = (
        fetcher.legacy(route.legacy_path, route.legacy_entries_key)
        if route.legacy_path
        else None
    )
    legacy_reachable = None if route.legacy_path is None else legacy_rows is not None

    observability_reachable = None
    if route.observability_path is not None:
        obs = fetcher.observability(route.observability_path, route.logd_params)
        observability_reachable = obs is not None

    field_results: list[FieldResult] = []
    for field in route.fields:
        # An unreachable store is a missing producer for every field (we cannot
        # see any rows), reported distinctly from a reachable-but-empty store.
        status = (
            "missing-producer"
            if not logd_reachable
            else field_status(field, rows, matched)
        )
        field_results.append(
            FieldResult(
                field=field.field,
                locator=field.locator.value,
                classification=field.classification,
                producer=field.producer,
                status=status,
                legacy_field=field.legacy_field,
                legacy_present=_legacy_present(field, legacy_rows),
            )
        )

    return RouteResult(
        route=route.name,
        kind=route.kind,
        logd_reachable=logd_reachable,
        logd_rows=len(rows),
        matched_rows=len(matched),
        legacy_reachable=legacy_reachable,
        observability_reachable=observability_reachable,
        fields=field_results,
    )


def run_conformance(
    fetcher: Fetcher, routes: list[RouteSpec], strict: bool = False
) -> Report:
    """Run every route and fold the per-field verdicts into one report.

    ``ok`` is true when no field failed; ``strict`` additionally requires that no
    field is missing a producer (a stricter gate for an on-rig run where every
    producer is expected to be live).
    """
    results = [run_route(fetcher, route) for route in routes]
    passed = failed = missing = 0
    for route in results:
        for field in route.fields:
            if field.status == "pass":
                passed += 1
            elif field.status == "fail":
                failed += 1
            else:
                missing += 1
    ok = failed == 0 and (not strict or missing == 0)
    return Report(
        ok=ok,
        strict=strict,
        passed=passed,
        failed=failed,
        missing_producer=missing,
        routes=results,
    )
