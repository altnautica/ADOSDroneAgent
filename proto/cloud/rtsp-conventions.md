# RTSP path conventions — cloud relay video

Documents the RTSP push surface every backend uses when sending hardware-encoded H.264 video to the cloud relay. Source of truth: `src/ados/services/video/` (pipeline, mediamtx orchestration).

## Local RTSP server

The agent runs an embedded RTSP server (mediamtx) on localhost.

| Field | Value |
|---|---|
| Bind address | `localhost:8554` |
| Stream path | `/{stream_name}` where `stream_name` ∈ `{main, thermal, ...}` |
| Default stream | `main` (primary camera) |
| Bind timeout | 10 s after server start |
| Port probe interval | 50 ms |

The local server lets on-device clients (the WebRTC bridge, future plugins) consume the same encoded stream the cloud relay pushes.

## Cloud RTSP push

The agent pushes the encoded stream to whatever URL is set in `config.video.cloud_relay_url`. The agent does NOT impose a path structure on the push target; the cloud relay (or whatever endpoint the operator configures) decides the URL shape.

| Field | Value |
|---|---|
| Push target | full RTSP URL from `config.video.cloud_relay_url`; push is disabled when unset |
| Transport | TCP (not UDP) — failures surface as TCP errors |
| Codec | H.264 passthrough (`-c copy`) — no re-encoding in the push path |
| Authentication | none at the RTSP layer; the operator includes any required credentials directly in the URL or relies on the cloud relay's pairing-key authentication on the control plane |
| Connect timeout | 5 s (passed to ffmpeg via `-timeout 5000000` µs) |

A typical cloud-relay endpoint terminates the RTSP push, multiplexes the H.264 stream onto WHEP (WebRTC-HTTP Egress Protocol), and serves it to ground control stations as a low-latency WebRTC track. The Altnautica reference relay uses paths derived from the device's pairing identity, but the agent does not assume that shape — it pushes to the configured URL verbatim.

## Reconnect policy

| Field | Value |
|---|---|
| Base delay | 5 s |
| Max delay | 300 s (5 min) |
| Backoff | exponential |
| Reset on success | yes — successful push for ≥30 s resets the delay to base |

A backend pushing video must implement a reconnect loop with these properties. The cloud relay tolerates short disconnects and resumes ingestion on the next push without a teardown sequence.

## Codec parameters

| Field | Value |
|---|---|
| Codec | H.264 |
| Profile | Constrained Baseline (recommended) or Main |
| Level | 4.0 (max for Baseline at 1080p30) |
| Bitrate ceiling | not enforced by RTSP layer; encoder sets the ceiling |
| Keyframe interval | ≤2 s (encoder responsibility — cloud relay assumes regular IDRs) |

H.265 is reserved for future use; the cloud relay path is H.264-only at this version.

## Conformance

A backend producing video must:

1. Read the push target from `config.video.cloud_relay_url`. Treat empty / unset as "cloud push disabled" and serve the local RTSP path only.
2. Use TCP transport.
3. Pass H.264 unchanged from the encoder to the RTSP push (no re-encoding in this hop).
4. Honor the reconnect-with-exponential-backoff policy.
5. Tee the stream to the local RTSP server at `localhost:8554` so on-device consumers see the same bytes the cloud sees.
