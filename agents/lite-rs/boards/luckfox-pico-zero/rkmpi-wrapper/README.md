# RKMPI subprocess wrapper

Small uclibc-built C launcher that wraps the Rockchip RKMPI hardware
H.264 encoder library on Luckfox Pico Zero (Rockchip RV1106 / RV1106G3).

The Rust lite agent ships as a musl-static binary; the vendor encoder
library is a closed `.so` linked against uclibc. Loading the vendor
library directly into the agent binary is not viable. This wrapper
puts the vendor library behind a process boundary so the agent can
talk to it over a stable wire format without dragging libc
compatibility into the parent's address space.

The Rust side is implemented at
`agents/lite-rs/crates/ados-video/src/rkmpi_subprocess.rs`. The wire
types `SubprocessRequest` and `SubprocessResponse` are the canonical
contract; this README documents the framing the C wrapper must
produce and consume.

## Wire format

The wrapper reads from stdin and writes to stdout. Both directions use
the same length-prefixed msgpack framing:

```
+-----------+----------------------+
| u32 BE    | msgpack body         |
| length    | (length bytes)       |
+-----------+----------------------+
```

- The 4-byte length prefix is the size of the msgpack body in bytes,
  big-endian. It does NOT include the length prefix itself.
- The body is a single msgpack-encoded value matching one of the
  variant shapes below.
- Maximum framed body size: 4 MiB (4 * 1024 * 1024). This caps a
  single encoded H.264 access unit; 5MP@30fps keyframes at the
  documented bitrate ceiling fit comfortably below this.

### Parent → child (stdin)

The parent sends one of:

- `Start` with the configuration to apply:
  ```json
  {
    "kind": "Start",
    "width": 1920,
    "height": 1080,
    "fps": 30,
    "bitrate_kbps": 6000,
    "keyframe_interval_secs": 2
  }
  ```
- `Stop`:
  ```json
  { "kind": "Stop" }
  ```

The first message after process start is always a `Start`. Mid-stream
reconfig is not supported at v1; the parent restarts the wrapper to
change resolution / bitrate.

### Child → parent (stdout)

The wrapper sends one of:

- `Ready` once the encoder is initialized and the first frame is about
  to flow. The parent waits for this before treating the encoder as
  live:
  ```json
  { "kind": "Ready" }
  ```
- `Frame` for each encoded access unit:
  ```json
  {
    "kind": "Frame",
    "is_keyframe": true,
    "pts_ms": 33,
    "bytes": "<binary H.264 NAL units, Annex-B framed>"
  }
  ```
  `bytes` is a msgpack `bin` value (not a string). Annex-B start codes
  (`00 00 00 01`) are included so the buffer is directly forwardable
  to an RTSP / RTP packetizer.
- `Error` on any vendor-reported fault:
  ```json
  { "kind": "Error", "message": "RK_MPI_VENC_GetStream timed out" }
  ```
  The parent typically logs and respawns the wrapper after `Error`.

### Termination

- On receipt of `Stop` the wrapper releases the encoder, flushes any
  pending frames, then exits with status 0.
- If stdin closes (the parent died) the wrapper exits within 1 second
  with status 1. This avoids a stranded child holding the encoder
  hardware.
- If the encoder library reports a non-recoverable fault the wrapper
  emits one `Error` and exits with status 2.

## Build

The wrapper must be built with the Luckfox uclibc toolchain so it can
link against the vendor `.so`. The toolchain triplet is
`arm-rockchip830-linux-uclibcgnueabihf`.

A `Makefile.template` is provided. Copy it to `Makefile`, fill in
`SDK_ROOT` to point at the Luckfox SDK install, and run `make`. The
output is a small static-linked-against-uclibc executable
`rkmpi-wrapper` that the lite agent's installer drops at
`/usr/lib/ados/rkmpi-wrapper`.

## Library binding discipline

- The wrapper links the vendor `.so` directly. Do not redistribute
  the vendor library inside the lite agent release artifact; the
  Buildroot recipe pulls it from the Luckfox SDK at image-build time.
- Vendor symbol names (`RK_MPI_VENC_*`, `RK_MPI_SYS_*`, `MB_*`) appear
  only in the wrapper's C source, not in any Rust file. The Rust side
  knows nothing about the vendor API — it only speaks the msgpack
  wire format.
- The wrapper is single-threaded; the encoder feed loop owns the
  stdout writer. No locking is needed.

## Test fixtures

A future bench-validation rig will exercise this contract end-to-end:
the parent spawns the wrapper, sends a `Start`, captures the first
`Ready`, then accumulates `Frame` messages for 60 seconds and asserts
the keyframe interval matches the requested GOP. The fixture lives at
`crates/ados-video/tests/` once the wrapper is buildable in CI.
