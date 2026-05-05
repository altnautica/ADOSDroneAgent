# proto/state/ — state IPC wire format

Documents the newline-delimited JSON wire format published on the state IPC socket at `/run/ados/state.sock`.

## Files

| File | Purpose | Status |
|---|---|---|
| `state-wire-format.md` | JSON shape, field semantics, framing | TODO |

## Source of truth

The Python full agent at `src/ados/core/ipc.py` is the reference implementation. State messages are emitted as newline-delimited JSON at ~10 Hz. Backends consume the stream by connecting as Unix-socket clients and reading line-buffered records.

## No aggregated state file

The state surface is socket-only. There is no aggregated `/run/ados/state.json` file on disk; consumers connect to the socket and process the stream live. This avoids stale-snapshot races and keeps the state topology simple.
