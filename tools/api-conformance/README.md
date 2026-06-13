# api conformance harness

A standalone, deterministic, bounded checker that issues the **same** HTTP
request to two transports — the native control front and the residual Python
handler — and asserts the two responses are byte-faithfully equal, modulo
volatile fields, per route.

It is the gate that lets a route flip from proxied-to-Python to native-in-Rust:
a ported native handler must match the Python it replaces before the cutover.

It is a developer tool. It is **not** part of the agent runtime and is not run
during install.

## What it does

For each route case it:

1. issues the identical request to the **native control front** over TCP
   (default `http://localhost:8080`);
2. issues the identical request to the **residual Python** over its internal
   unix socket (default `/run/ados/api-internal.sock`) — reached directly so the
   diff sees the Python handler's own bytes, not a proxied copy of the native
   response;
3. asserts the two responses are equal:
   - **status code** exactly equal;
   - **allowlisted headers** equal (the stable, value-bearing headers; `Date`,
     `Server`, and the hop-by-hop set are dropped);
   - **body** byte-equal after JSON-canonicalize (sorted keys, compact
     separators) and volatile-field masking — or, for a server-sent-event route,
     the **frame sequence** equal after the same masking.

A route case may carry a paired and an unpaired header variant; each present
variant is diffed and the route passes only when every variant matches.

### Volatile masking

Two independently-written handlers never return bit-identical bytes: each has its
own timestamps, its own pid, and counters that advance between the two requests.
Before comparing, the harness masks volatile-keyed values (at any depth) to a
fixed sentinel. The default set covers timestamps (`ts`, `timestamp`, `uptime`,
`uptime_seconds`, `started_at`, …), `pid`, and monotonic counters (`seq`,
`counter`, `request_id`, …). A route extends the set per-case via
`extra_volatile` (e.g. the regenerated `code` on the pairing-code route).

### Routes (initial set)

| route | method | path | notes |
|-------|--------|------|-------|
| `healthz` | GET | `/healthz` | liveness |
| `version` | GET | `/api/version` | build version string |
| `time` | GET | `/api/time` | fully volatile (proves the masking) |
| `status` | GET | `/api/status` | paired + unpaired variants |
| `telemetry` | GET | `/api/telemetry` | live snapshot, timestamp masked |
| `pairing-info` | GET | `/api/pairing/info` | unpaired LAN advertisement |
| `pairing-code` | GET | `/api/pairing/code` | masks the regenerated code |
| `commands` | GET | `/api/commands` | queued command poll |

Write routes (POST/PUT/DELETE) get listed too, flagged `require_sandbox=True` and
skipped by default, since they have side effects against a live agent. More cases
are appended per route as routes migrate — a one-line `REGISTRY` append in
`api_conformance/route_cases.py`, mirroring the per-domain registry pattern the
sibling durable-store harness uses.

## Usage

```bash
pip install -r tools/api-conformance/requirements.txt

python tools/api-conformance/main.py \
    --front-base http://localhost:8080 \
    --python-uds /run/ados/api-internal.sock
```

Flags:

- `--route NAME` (repeatable) — restrict to named routes; default all.
- `--strict` — also fail when any sandboxed route was skipped (so an on-rig run
  can demand every listed route was exercised).
- `--timeout SECONDS` — per-request bound (default 5).
- `--json` — emit the JSON report (the default output is already JSON; accepted
  for symmetry).

The report is JSON on stdout. The exit code is `0` when every diffed route passed
(and, under `--strict`, no route was skipped), `1` otherwise.

## Tests

```bash
pip install -r tools/api-conformance/requirements.txt pytest
pytest tools/api-conformance/tests
```

The self-tests run entirely against synthetic response pairs and need no live
service, so they are deterministic and run on macOS too.
