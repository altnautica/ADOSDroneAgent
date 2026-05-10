"""UDP fan-out for the ground-side video stream.

`wfb_rx` outputs the FEC-decoded RTP H.264 stream to a single UDP port.
Two consumers want to read it:

1. The mediamtx-gs ffmpeg ingest sidecar — for the browser WHEP stream.
2. ``LocalVideoTap`` (the LCD video pipeline) — for the on-device LCD.

Only one process can bind a UDP port at a time, and SO_REUSEPORT
load-balances rather than duplicating, so we run a tiny fan-out: read
each datagram from the wfb_rx output port and re-emit it to two
downstream localhost ports. Per-packet relay cost is sub-millisecond.

Architecture notes:

* This is the same pattern OpenHD uses (their ground daemon is a
  stateless RTP forwarder; viewers read UDP directly). The ADOS
  monolithic agent embeds the fan-out as a subprocess so the lifecycle
  is supervised by ``WfbRxManager`` alongside ``wfb_rx`` itself.
* Datagrams are RTP packets; we don't parse them. Just memcpy + sendto.
* Single thread, blocking ``recvfrom``. No queueing, no reordering, no
  drop policy beyond what the kernel UDP socket buffer enforces.
* Drop counter on send failure surfaces via stderr so the supervisor
  log captures sustained problems.
"""

from __future__ import annotations

import argparse
import logging
import os
import select
import signal
import socket
import sys
from collections.abc import Iterable

log = logging.getLogger("video_fanout")


def _build_input_socket(host: str, port: int) -> socket.socket:
    """Bind the source socket. ``SO_REUSEADDR`` so a quick agent restart
    doesn't run into a TIME_WAIT lingering on the port."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    # 4 MB receive buffer matches the kernel's typical max for the
    # 5 GHz video stream's burst size; default 208 KB drops packets
    # when the consumer is slow even briefly.
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
    except OSError:
        pass
    sock.bind((host, port))
    return sock


def _build_output_socket() -> socket.socket:
    """One UDP socket for all sendto destinations. Larger send buffer
    so the kernel can queue a burst of relay packets without blocking
    the recv loop."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4 * 1024 * 1024)
    except OSError:
        pass
    return sock


def run_fanout(
    *,
    listen_host: str,
    listen_port: int,
    fanout_targets: Iterable[tuple[str, int]],
    buf_size: int = 65536,
) -> None:
    """Block forever forwarding UDP datagrams from listen_port to each target.

    Designed to be invoked from ``WfbRxManager`` as a subprocess. The
    parent supervises lifecycle via the same restart loop it uses for
    ``wfb_rx``; this function does not implement its own retry.
    """
    targets = list(fanout_targets)
    if not targets:
        raise ValueError("no fanout targets configured")
    in_sock = _build_input_socket(listen_host, listen_port)
    out_sock = _build_output_socket()
    stop = False

    def _handle_signal(signum: int, frame: object) -> None:  # noqa: ARG001
        nonlocal stop
        stop = True

    # signal.signal() only works on the main thread; calling it from a
    # non-main thread raises ValueError. That's the case in tests (we
    # run the fanout in a daemon thread for test isolation). Tolerate
    # the install failure — the loop still respects the stop flag if
    # set externally, and the subprocess entry point on production is
    # always main-thread.
    for _sig in (signal.SIGTERM, signal.SIGINT):
        try:
            signal.signal(_sig, _handle_signal)
        except (ValueError, OSError):
            pass

    log.info(
        "fanout_started listen=%s:%d targets=%s",
        listen_host,
        listen_port,
        targets,
    )

    forwarded = 0
    drops = 0
    while not stop:
        # Use select so SIGTERM unblocks recvfrom promptly.
        try:
            ready, _, _ = select.select([in_sock], [], [], 0.5)
        except (InterruptedError, OSError):
            continue
        if not ready:
            continue
        try:
            payload, _addr = in_sock.recvfrom(buf_size)
        except (OSError, BlockingIOError):
            continue
        if not payload:
            continue
        for target in targets:
            try:
                out_sock.sendto(payload, target)
            except OSError:
                drops += 1
        forwarded += 1
        # Periodic counter log so a long-run drift in drop rate is
        # visible without flooding the journal.
        if forwarded % 5000 == 0:
            log.info(
                "fanout_progress forwarded=%d drops=%d",
                forwarded,
                drops,
            )

    log.info("fanout_stopping forwarded=%d drops=%d", forwarded, drops)
    in_sock.close()
    out_sock.close()


def _parse_target(spec: str) -> tuple[str, int]:
    """Parse a `host:port` string. Reject malformed inputs."""
    host, _, port = spec.rpartition(":")
    if not host or not port:
        raise ValueError(f"invalid target {spec!r}; expected host:port")
    return host, int(port)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="ados-video-fanout")
    parser.add_argument("--listen-host", default="127.0.0.1")
    parser.add_argument("--listen-port", type=int, required=True)
    parser.add_argument(
        "--target",
        action="append",
        default=[],
        help="Fan-out destination as host:port. Repeat for each.",
    )
    args = parser.parse_args(argv)

    # Inherit log level from the supervising parent's environment.
    level = os.environ.get("LOG_LEVEL", "INFO").upper()
    logging.basicConfig(
        level=level,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        stream=sys.stderr,
    )

    targets = [_parse_target(t) for t in args.target]
    if not targets:
        parser.error("at least one --target is required")

    try:
        run_fanout(
            listen_host=args.listen_host,
            listen_port=args.listen_port,
            fanout_targets=targets,
        )
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
