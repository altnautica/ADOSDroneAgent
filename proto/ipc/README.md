# proto/ipc/ — Unix-socket framing

Documents the framing convention for in-process inter-service communication via Unix-domain sockets.

## Files

| File | Purpose | Status |
|---|---|---|
| `unix-sockets.md` | Length-prefix framing on `mavlink.sock`, line framing on `state.sock` | TODO |

## Sockets

| Socket | Path | Framing |
|---|---|---|
| MAVLink frame broadcast | `/run/ados/mavlink.sock` | 4-byte big-endian length prefix + raw MAVLink v2 frame |
| State pub/sub | `/run/ados/state.sock` | newline-delimited JSON |

## Source of truth

`src/ados/core/ipc.py`. Both servers run inside the Python full agent today; the Rust lite agent connects as a client and consumes the same framing.
