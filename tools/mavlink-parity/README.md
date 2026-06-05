# MAVLink parity harness

Side-by-side comparison of the Python MAVLink service
(`python -m ados.services.mavlink`) and the Rust router (`ados-mavlink-router`).
It runs both at the same time, drives them from one telemetry source, and
checks that they produce the same vehicle-state snapshot, frame fan-out,
direct-GCS proxy outputs, and (when the harness is the FC) the same outbound
behaviour toward the flight controller.

The two processes are isolated: each gets its own `ADOS_RUN_DIR` (so the unix
sockets do not collide with each other or a running agent) and its own set of
proxy ports.

## Prerequisites

- Build the Rust router once:

  ```
  cargo build --manifest-path crates/Cargo.toml -p ados-mavlink-router
  ```

- A Python environment with the agent importable plus `pymavlink` and
  `websockets`.

## Run

From the repo root:

```
python tools/mavlink-parity/parity.py --source demo
python tools/mavlink-parity/parity.py --source shared
python tools/mavlink-parity/parity.py --source sitl --sitl tcp:127.0.0.1:5760
```

Exit code is `0` when every non-skipped check passes, `1` otherwise. Add
`--json report.json` (or `--json -` for stdout) to capture the machine-readable
report.

### Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `--source` | `demo` | telemetry source: `demo`, `shared`, or `sitl` |
| `--duration` | `6.0` | collection window in seconds |
| `--warmup` | `2.0` | wait after launch before collecting |
| `--python` | current interpreter | interpreter used for the Python service |
| `--rust-bin` | auto-detected | path to `ados-mavlink-router` |
| `--sitl` | — | external SITL connection (`tcp:host:port`) for `--source sitl` |
| `--workdir` | `/tmp/ados-mavlink-parity` | scratch dir for sockets and configs |
| `--json` | — | write the JSON report to a path (`-` for stdout) |

## Source modes

- **demo** (default, hardware-free, runs on macOS): each side runs its own
  synthetic FC (Rust `--demo` / Python `--demo`). Compares state snapshots,
  fan-out, and proxy outputs. The outbound-to-FC checks are not applicable
  (there is no FC) and are reported as skipped.
- **shared** (hardware-free, runs on macOS): the harness is the FC. Both sides
  connect to it over TCP, it feeds identical telemetry to both, and it records
  each side's outbound frames. This unlocks the full matrix: byte-exact
  fan-out, companion-heartbeat / parameter-sweep / stream-interval behaviour,
  and command pass-through.
- **sitl**: both sides connect to an external SITL (`--sitl tcp:host:port`).
  Compares state, fan-out, and proxies; the outbound-to-FC checks are skipped
  because the harness does not sit between the agent and the SITL.

## Checks

| Check | Compares |
| --- | --- |
| `state_schema` | the 10 Hz snapshot key/type skeleton (core vehicle fields) |
| `state_static` | constant snapshot fields (mode, type, GPS, RC, throttle, …) |
| `state_model_*` | each side's time-varying fields against the shared flight model |
| `fanout_coverage` | both fan-outs carry all eight telemetry message ids |
| `fanout_heartbeat` | the HEARTBEAT fields match across fan-outs |
| `proxy_tcp/udp/ws` | each proxy streams telemetry on both sides |
| `fanout_exact` | (shared) each fan-out frame is byte-exact one the FC sent |
| `outbound_heartbeat` | (shared) both sides emit a companion heartbeat |
| `outbound_param_sweep` | (shared) both sides send `PARAM_REQUEST_LIST` |
| `outbound_stream_requests` | (shared) both request the expected stream intervals |
| `command_passthrough` | (shared) a command sent to a proxy reaches the FC |

The demo flight matches `src/ados/services/mavlink/demo.py`: a slow circle over
a fixed point with a gentle attitude wobble, draining battery, and steady GPS
lock. Float fields are compared against the model with tolerances that absorb
the small clock skew between the two independently-launched processes; static
fields and frame coverage are compared exactly.
