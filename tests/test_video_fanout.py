"""Tests for the UDP video fan-out subprocess module.

The fan-out reads RTP H.264 datagrams from a single source UDP port
(where ``wfb_rx`` writes) and re-emits each datagram to two downstream
ports — one for the mediamtx-gs ffmpeg ingest sidecar, one for the LCD
udpsrc. Both consumers must see every packet (SO_REUSEPORT would
load-balance, breaking RTP).

These tests run the fanout in a thread (not a subprocess) so the loop
runs against a controllable local socket pair without spawning Python
inside the test runner.
"""

from __future__ import annotations

import socket
import threading
import time

from ados.services.ground_station.video_fanout import run_fanout


def _free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _recv_exact(sock: socket.socket, n: int, timeout: float = 1.0) -> list[bytes]:
    """Receive up to n datagrams or until timeout, whichever first."""
    sock.settimeout(timeout)
    out: list[bytes] = []
    deadline = time.monotonic() + timeout
    while len(out) < n and time.monotonic() < deadline:
        try:
            data, _addr = sock.recvfrom(65536)
        except OSError:
            break
        out.append(data)
    return out


def test_fanout_duplicates_packets_to_both_targets() -> None:
    """A datagram sent to the listen port must appear at BOTH targets,
    byte-for-byte identical."""
    listen_port = _free_port()
    target_a_port = _free_port()
    target_b_port = _free_port()

    target_a = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target_a.bind(("127.0.0.1", target_a_port))
    target_b = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target_b.bind(("127.0.0.1", target_b_port))

    # Run the fanout in a daemon thread so the test can stop it via
    # SIGTERM-equivalent (we just don't wait for it; the daemon thread
    # exits when the test process does).
    t = threading.Thread(
        target=run_fanout,
        kwargs={
            "listen_host": "127.0.0.1",
            "listen_port": listen_port,
            "fanout_targets": [
                ("127.0.0.1", target_a_port),
                ("127.0.0.1", target_b_port),
            ],
        },
        daemon=True,
    )
    t.start()
    # Give the fanout a brief moment to bind its socket.
    time.sleep(0.1)

    sender = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    payloads = [
        b"hello world",
        bytes(range(256)),
        b"\x80\x60" + b"\x00" * 18 + b"x" * 200,  # mock RTP header + payload
    ]
    for p in payloads:
        sender.sendto(p, ("127.0.0.1", listen_port))

    received_a = _recv_exact(target_a, len(payloads), timeout=2.0)
    received_b = _recv_exact(target_b, len(payloads), timeout=2.0)

    assert received_a == payloads, (
        f"target A got {len(received_a)} of {len(payloads)} packets; "
        f"expected exact byte match"
    )
    assert received_b == payloads, (
        f"target B got {len(received_b)} of {len(payloads)} packets; "
        f"expected exact byte match"
    )

    sender.close()
    target_a.close()
    target_b.close()


def test_fanout_rejects_no_targets() -> None:
    import pytest

    with pytest.raises(ValueError, match="no fanout targets"):
        run_fanout(
            listen_host="127.0.0.1",
            listen_port=_free_port(),
            fanout_targets=[],
        )


def test_fanout_handles_empty_payload() -> None:
    """Zero-length datagrams arrive in some pathological cases (close
    + reopen sockets); make sure the loop tolerates them without
    blowing up. We don't assert they propagate (the impl skips empties
    by design), just that the loop continues to function."""
    listen_port = _free_port()
    target_port = _free_port()

    target = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target.bind(("127.0.0.1", target_port))

    t = threading.Thread(
        target=run_fanout,
        kwargs={
            "listen_host": "127.0.0.1",
            "listen_port": listen_port,
            "fanout_targets": [("127.0.0.1", target_port)],
        },
        daemon=True,
    )
    t.start()
    time.sleep(0.1)

    sender = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sender.sendto(b"", ("127.0.0.1", listen_port))  # empty
    sender.sendto(b"real", ("127.0.0.1", listen_port))  # real payload after

    # Real payload should still arrive; empty was dropped.
    received = _recv_exact(target, 1, timeout=2.0)
    assert b"real" in received

    sender.close()
    target.close()
