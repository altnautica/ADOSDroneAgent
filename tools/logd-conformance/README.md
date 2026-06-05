# logd conformance harness

A standalone, deterministic, bounded checker that asserts the durable
logging-and-telemetry store serves a **superset** of the legacy on-box
log/telemetry surface, route by route and field by field.

It is a developer tool. It is **not** part of the agent runtime and is not run
during install.

## What it does

For each route in the first observability set it:

1. queries the legacy on-box handler (when one exists), e.g. `GET /api/logs`;
2. queries the durable store's query API directly — the on-box unix socket
   first (`/run/ados/logd-query.sock`, no key, works even when the HTTP front
   door is down), the LAN TCP port as the fallback;
3. queries the store's observability proxy on the legacy base (when wired);
4. classifies every field the store should serve:
   - `pass` — the store serves it (the column is present on the matched rows, or
     the metric name appears among the metric rows);
   - `fail` — the store has rows for the table but a field column is absent (a
     real schema gap a producer must close);
   - `missing-producer` — no rows match (no producer is emitting yet); for a
     metric, the specific metric name is absent.

Every field is also tagged as durable **history** or a **live** read, and, for
routes with a legacy handler, whether the legacy surface served the mapped
legacy field (so the report documents that the store is a true superset). The
legacy field map for the log route mirrors the on-box legacy entry mapping
one-for-one: `seq←id`, `timestamp←ts_us`, `level←level`, `logger←target|source`,
`message←msg`.

## Routes (initial set)

| route | table | legacy handler | fields |
|-------|-------|----------------|--------|
| `logs` | logs | `/api/logs` | id, ts_us, level, source, msg |
| `link-metrics` | metrics | — | link.rssi_dbm, link.snr_db, link.fec_uncorrected |
| `video-metrics` | metrics | — | video.encoder_bitrate_kbps, video.framerate_hz, video.queue_depth_frames, video.dropped_frames_cumulative |
| `hw-summary` | metrics | — | cpu.utilization_pct, mem.available_pct, disk.used_pct, thermal.primary_c |
| `service-events` | events | — | from_state, to_state, reason (on `service.transition`) |

## Usage

```bash
pip install -r tools/logd-conformance/requirements.txt

python tools/logd-conformance/main.py \
    --legacy-base http://localhost:8080 \
    --logd-base http://localhost:8090 \
    --socket /run/ados/logd-query.sock
```

Flags:

- `--route NAME` (repeatable) — restrict to named routes; default all.
- `--strict` — also fail when any producer is missing (no rows), not just on a
  field gap.
- `--no-socket` — skip the unix socket and use only the TCP base.
- `--timeout SECONDS` — per-request bound (default 5).

The report is JSON on stdout. The exit code is `0` when no field failed (and,
under `--strict`, no producer is missing), `1` otherwise.

## Tests

```bash
pip install -r tools/logd-conformance/requirements.txt pytest
pytest tools/logd-conformance/tests
```

The self-tests run entirely against mocked transports (`httpx.MockTransport`),
so they are deterministic and need no live service. They run on macOS too.
