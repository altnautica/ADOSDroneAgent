"""The route cases the harness diffs: the native-vs-Python comparison set.

Each ``RouteCase`` is one HTTP request issued identically to both transports,
plus the per-route knobs the comparison needs: the paired/unpaired header
variants, any request body, whether the response is a server-sent-event stream,
and the extra volatile fields to mask for this route on top of the default set.

The registry below is seeded with routes that are already served natively and
are safe to diff: read-only ``GET`` routes with no side effects. POST/PUT/DELETE
routes that mutate state are listed too but flagged ``require_sandbox=True`` and
skipped by default, so the registry documents the full surface without firing a
side effect against a live agent.

Adding a case as a route migrates is a one-line append to ``REGISTRY`` — the
only shared edit, mirroring the per-domain registry pattern the sibling
durable-store harness uses.
"""

from __future__ import annotations

from dataclasses import dataclass, field

# A placeholder bearer the paired-variant requests carry. The harness never
# authenticates for real; on a live run the operator supplies the agent's own
# key via the header-variant override. The default value keeps the request shape
# faithful (an Authorization header is present) without embedding a secret.
PAIRED_AUTH_PLACEHOLDER = "Bearer <api-key>"


@dataclass(frozen=True)
class RouteCase:
    """One route under conformance: the request to issue to both transports.

    * ``name`` — the stable identifier used by the ``--route`` filter and the
      report.
    * ``method`` / ``path`` — the HTTP verb and path issued verbatim to both.
    * ``paired_headers`` / ``unpaired_headers`` — the two auth variants. A route
      that behaves the same paired or not uses only ``unpaired_headers``; a route
      whose body depends on pairing supplies both and the harness diffs each
      variant.
    * ``body`` / ``content_type`` — the request payload for a write route (unused
      by the read routes seeded here).
    * ``is_sse`` — true when the response is a server-sent-event stream, compared
      as a frame sequence rather than a single body.
    * ``require_sandbox`` — true for a route with side effects (a write); skipped
      by default so the harness never mutates a live agent unless explicitly
      opted in.
    * ``extra_volatile`` — field keys to mask for this route on top of the
      default volatile set (e.g. a route-specific monotonic counter).
    """

    name: str
    method: str
    path: str
    unpaired_headers: dict[str, str] = field(default_factory=dict)
    paired_headers: dict[str, str] | None = None
    body: bytes | None = None
    content_type: str | None = None
    is_sse: bool = False
    require_sandbox: bool = False
    extra_volatile: tuple[str, ...] = ()


# The ordered registry. Seeded with the read-only routes already native and safe
# to diff; write routes are present but sandboxed (skipped by default). Append a
# line here as each route migrates.
REGISTRY: list[RouteCase] = [
    # Liveness: the smallest possible body, the first thing a native front
    # serves. No auth, no volatile fields beyond an optional uptime.
    RouteCase(
        name="healthz",
        method="GET",
        path="/healthz",
    ),
    # The agent version string. Stable between two reads of the same build.
    RouteCase(
        name="version",
        method="GET",
        path="/api/version",
    ),
    # The current time the agent reports. Entirely volatile, so this case proves
    # the masking: both bodies collapse to the sentinel and compare equal.
    RouteCase(
        name="time",
        method="GET",
        path="/api/time",
    ),
    # The composite status body. Carries timestamps and counters masked by the
    # default volatile set; the rest (profile, capabilities, link state) is the
    # contract the two handlers must agree on.
    RouteCase(
        name="status",
        method="GET",
        path="/api/status",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
    ),
    # The live telemetry snapshot. The numeric values move every read, so the
    # case masks the snapshot's own timestamp and leaves the shape to compare.
    RouteCase(
        name="telemetry",
        method="GET",
        path="/api/telemetry",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
    ),
    # Pairing info: what an unpaired agent advertises to a claimer on the LAN.
    # Read freely while unpaired (being on the LAN is the auth boundary).
    RouteCase(
        name="pairing-info",
        method="GET",
        path="/api/pairing/info",
    ),
    # The short-lived pairing code an unpaired agent shows. The code itself is
    # regenerated, so it is masked as a route-extra volatile field.
    RouteCase(
        name="pairing-code",
        method="GET",
        path="/api/pairing/code",
        extra_volatile=("code", "expires_at", "expires_in"),
    ),
    # The queued command list the agent polls. Empty on an idle agent; the entry
    # ids and enqueue timestamps are volatile when present.
    RouteCase(
        name="commands",
        method="GET",
        path="/api/commands",
        paired_headers={"authorization": PAIRED_AUTH_PLACEHOLDER},
        extra_volatile=("id", "enqueued_at"),
    ),
    # <append a RouteCase line per route as it migrates — the only shared edit>
]


def registered_cases() -> list[RouteCase]:
    """Every registered route case, in report order."""
    return list(REGISTRY)


def case_by_name(name: str) -> RouteCase | None:
    """Look up one route case by name (for the ``--route`` CLI filter)."""
    for case in REGISTRY:
        if case.name == name:
            return case
    return None
