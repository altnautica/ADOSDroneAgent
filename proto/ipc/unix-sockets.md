# Unix-socket framing — in-process IPC

Documents the framing convention for the two Unix-domain sockets the agent exposes for in-process inter-service communication. Source of truth: `MavlinkIPCServer` / `MavlinkIPCClient` and `StateIPCServer` / `StateIPCClient` in `src/ados/core/ipc.py`.

## Sockets

| Socket | Path | Framing | Direction |
|---|---|---|---|
| MAVLink frame broadcast | `/run/ados/mavlink.sock` | length-prefixed binary | bidirectional |
| Vehicle state pub/sub | `/run/ados/state.sock` | newline-delimited JSON | server → clients |

Both sockets are Unix-domain stream sockets created with permissions `0o666`. Local-only access is enforced by the socket file's directory permissions, not by an in-band auth handshake.

## MAVLink socket — `/run/ados/mavlink.sock`

### Framing

Each message is a length-prefixed MAVLink v2 frame.

```
[ length (4 bytes, big-endian unsigned) ][ MAVLink v2 frame (length bytes) ]
```

The 4-byte prefix is `struct.pack("!I", len(frame))` in Python, equivalent to a big-endian unsigned 32-bit integer in any language (`htonl`, `to_be_bytes`, etc.).

Example writer:

```python
header = struct.pack("!I", len(frame))
writer.write(header + frame)
```

Example reader:

```python
header = await reader.readexactly(4)
(length,) = struct.unpack("!I", header)
frame = await reader.readexactly(length)
```

### Direction and routing

The MAVLink socket is bidirectional:

- **Server → clients (broadcast):** when the agent receives a MAVLink frame from the FC, it broadcasts the frame to every connected IPC client. Use case: cloud relay, plugins, ground-station bridges that need the live FC frames.
- **Clients → server (commands):** an IPC client may write frames to the server. The server forwards inbound frames to the FC serial port. Use case: a cloud-relay client forwarding inbound GCS commands.

### Backpressure

The server maintains a per-client send queue (default depth 256). When a client's queue fills, the server disconnects that client. The disconnected client must reconnect to resume — the server does not buffer for them.

There is no explicit ACK mechanism. The protocol assumes clients are local and fast.

### Connection lifecycle

| Step | Detail |
|---|---|
| Server start | Binds `/run/ados/mavlink.sock` with mode `0o666`. |
| Client connect | Retry with exponential backoff (default 10 attempts, 1 s delay). |
| Frame exchange | Length-prefix framing both directions. |
| Slow-client disconnect | Server closes when send queue fills. |
| Client error | Sets `_connected=False`; client must call `connect()` again. |
| Graceful close | `writer.close()` then `await writer.wait_closed()`. |

## State socket — `/run/ados/state.sock`

Documented in detail at `proto/state/state-wire-format.md`. Summary:

- Newline-delimited JSON (no length prefix).
- Server broadcasts at 10 Hz to all connected clients.
- Per-client queue depth 32; slow client is disconnected.
- Initial snapshot delivered immediately on connect.

The two sockets use different framing because their workloads differ: MAVLink frames are binary and arrive at FC frame rates (tens to hundreds per second), while state snapshots are JSON-shaped at human-friendly intervals (10 Hz). Length-prefix on MAVLink avoids JSON parsing overhead on the hot path; newline-JSON on state keeps the snapshot human-readable for debugging.

## Conformance

A backend implementing either socket must:

1. Use the exact socket path and mode `0o666`.
2. Honor the documented framing (length-prefix big-endian for MAVLink, newline JSON for state).
3. Maintain a per-client send queue and disconnect slow clients rather than blocking the publish path.
4. Accept the bidirectional flow on the MAVLink socket; reject writes from clients on the state socket (it is server-publish only).
5. Make no assumption about client process identity — clients may be Python services, the lightweight Rust backend, plugins, future implementations.
