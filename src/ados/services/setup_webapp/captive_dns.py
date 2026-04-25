"""Captive portal DNS and HTTP probe responder.

First-boot aid. When a phone joins the ground-node AP we need the
phone's OS to open a browser window pointed at our setup webapp. OS
vendors detect captivity by probing specific URLs. This service:

1. Answers DNS A queries for those probe hostnames with the AP
   gateway address 192.168.4.1.
2. Answers HTTP GET for the probe paths with the exact status code
   the OS expects.

Probe strategy. The spec `10-setup-webapp.md` section "Captive portal
detection" says: "OS-specific probe URLs are all served with a 204
No Content to signal 'no real internet' and trigger the captive
portal UI". We follow that rule. Android/Chrome `/generate_204`,
Apple `/hotspot-detect.html`, Windows `/connecttest.txt` all receive
204. No redirects, no HTML body. That wording pins the choice.

Lifecycle per rule 26 and spec `10-setup-webapp.md`:
- If `/var/lib/ados/setup-complete` exists at start, the service
  logs and exits 0. The systemd unit runs with `Restart=no` so exit
  0 is a clean "done" signal.
- Otherwise we bind UDP 53 for DNS and TCP 80 for HTTP on all
  interfaces (the ground-station AP is typically the only route
  with inbound traffic, but we do not restrict the bind).
- On SIGTERM/SIGINT we cancel both servers and exit 0.

Dependency choice:
- DNS: stdlib sockets via `asyncio.DatagramProtocol`. No third-party
  DNS lib needed for the small answer set.
- HTTP: stdlib `http.server.BaseHTTPRequestHandler` in a background
  thread pool. `aiohttp` is not an agent dep (pyproject uses
  fastapi + uvicorn). Avoids pulling in extra transitive deps for
  a handful of 204 responses.
"""

from __future__ import annotations

import asyncio
import logging
import signal
import socket
import struct
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

from ados.core.logging import configure_logging, get_logger

log = get_logger("setup_webapp.captive_dns")

SETUP_COMPLETE_SENTINEL = Path("/var/lib/ados/setup-complete")
AP_GATEWAY_IP = "192.168.4.1"

# Hostnames that get mapped to AP_GATEWAY_IP.
CAPTURED_HOSTS: set[str] = {
    # Android and Chrome OS.
    "connectivitycheck.gstatic.com",
    "www.google.com",
    "www.gstatic.com",
    # iOS and macOS.
    "captive.apple.com",
    "www.apple.com",
    # Windows.
    "www.msftconnecttest.com",
    "dns.msftncsi.com",
    "www.msftncsi.com",
    # Samsung.
    "connectivitycheck.samsung.com",
    # Self.
    "setup.ados.local",
    "ados.local",
}

# HTTP probe paths. All receive 204 per spec.
PROBE_PATHS: set[str] = {
    "/generate_204",
    "/gen_204",
    "/hotspot-detect.html",
    "/library/test/success.html",
    "/connecttest.txt",
    "/ncsi.txt",
    "/success.txt",
}

DNS_PORT = 53
HTTP_PORT = 80


def _setup_already_complete() -> bool:
    try:
        return SETUP_COMPLETE_SENTINEL.exists()
    except OSError:
        return False


def _encode_dns_name(name: str) -> bytes:
    parts = name.rstrip(".").split(".")
    out = b""
    for p in parts:
        b = p.encode("ascii", errors="ignore")
        out += bytes([len(b)]) + b
    out += b"\x00"
    return out


def _parse_dns_question(data: bytes) -> tuple[int, str, int, int] | None:
    """Return (txid, qname, qtype, qclass) or None on malformed input."""
    if len(data) < 12:
        return None
    txid = struct.unpack("!H", data[0:2])[0]
    qdcount = struct.unpack("!H", data[4:6])[0]
    if qdcount < 1:
        return None
    i = 12
    labels: list[str] = []
    while i < len(data):
        length = data[i]
        if length == 0:
            i += 1
            break
        if length & 0xC0:
            # Pointer. Very rare in question section. Bail.
            return None
        i += 1
        if i + length > len(data):
            return None
        labels.append(data[i : i + length].decode("ascii", errors="ignore"))
        i += length
    if i + 4 > len(data):
        return None
    qtype, qclass = struct.unpack("!HH", data[i : i + 4])
    return txid, ".".join(labels).lower(), qtype, qclass


def _build_dns_answer(txid: int, qname: str, ip: str) -> bytes:
    """Build a single-A-record answer."""
    flags = 0x8180  # QR=1, RD=1, RA=1, no error.
    header = struct.pack("!HHHHHH", txid, flags, 1, 1, 0, 0)
    qname_bytes = _encode_dns_name(qname)
    question = qname_bytes + struct.pack("!HH", 1, 1)  # A, IN
    # Answer: pointer to qname (0xC00C), type A, class IN, TTL 60, rdlen 4.
    rdata = socket.inet_aton(ip)
    answer = struct.pack("!HHHIH", 0xC00C, 1, 1, 60, 4) + rdata
    return header + question + answer


def _build_dns_nxdomain(txid: int, qname: str, qtype: int, qclass: int) -> bytes:
    flags = 0x8183  # QR=1, RD=1, RA=1, RCODE=3 (NXDOMAIN).
    header = struct.pack("!HHHHHH", txid, flags, 1, 0, 0, 0)
    qname_bytes = _encode_dns_name(qname)
    question = qname_bytes + struct.pack("!HH", qtype, qclass)
    return header + question


class _DnsProtocol(asyncio.DatagramProtocol):
    """UDP DNS responder answering only the captured host set."""

    def __init__(self) -> None:
        self._transport: asyncio.DatagramTransport | None = None

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        assert isinstance(transport, asyncio.DatagramTransport)
        self._transport = transport
        log.info("captive_dns_bound", port=DNS_PORT)

    def datagram_received(self, data: bytes, addr: Any) -> None:
        parsed = _parse_dns_question(data)
        if parsed is None:
            return
        txid, qname, qtype, qclass = parsed
        if qtype == 1 and qclass == 1 and qname in CAPTURED_HOSTS:
            reply = _build_dns_answer(txid, qname, AP_GATEWAY_IP)
            log.debug("dns_answered", qname=qname, ip=AP_GATEWAY_IP)
        else:
            reply = _build_dns_nxdomain(txid, qname, qtype, qclass)
            log.debug("dns_nxdomain", qname=qname, qtype=qtype)
        try:
            if self._transport is not None:
                self._transport.sendto(reply, addr)
        except OSError as exc:
            log.debug("dns_send_failed", error=str(exc))


class _ProbeHandler(BaseHTTPRequestHandler):
    """HTTP 1.1 handler that returns 204 to every probe path."""

    # Silence BaseHTTPRequestHandler's default stderr logging.
    def log_message(self, format: str, *args: Any) -> None:
        return

    def _respond_204(self) -> None:
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()

    def _respond_root_redirect(self) -> None:
        # Unknown path. Redirect to the setup landing so phones that
        # open the probe URL in a browser still land somewhere sensible.
        self.send_response(302)
        self.send_header("Location", f"http://{AP_GATEWAY_IP}/")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self) -> None:  # noqa: N802 (stdlib contract)
        path = self.path.split("?", 1)[0]
        if path in PROBE_PATHS:
            self._respond_204()
            return
        self._respond_root_redirect()

    def do_HEAD(self) -> None:  # noqa: N802
        self.do_GET()


def _run_http_server(stop_evt: threading.Event) -> None:
    try:
        httpd = ThreadingHTTPServer(("0.0.0.0", HTTP_PORT), _ProbeHandler)
    except OSError as exc:
        log.error("captive_http_bind_failed", port=HTTP_PORT, error=str(exc))
        return
    log.info("captive_http_bound", port=HTTP_PORT)
    httpd.timeout = 0.5
    while not stop_evt.is_set():
        httpd.handle_request()
    try:
        httpd.server_close()
    except Exception:
        pass


async def _amain() -> int:
    configure_logging()
    logging.getLogger("asyncio").setLevel(logging.WARNING)

    if _setup_already_complete():
        log.info(
            "captive_dns_inactive",
            reason="setup already complete, exiting cleanly",
            sentinel=str(SETUP_COMPLETE_SENTINEL),
        )
        return 0

    loop = asyncio.get_event_loop()
    stop = asyncio.Event()

    def _on_signal(*_a: Any) -> None:
        log.info("captive_dns_signal_stop")
        stop.set()

    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, _on_signal)
        except NotImplementedError:
            signal.signal(sig, _on_signal)

    try:
        transport, _proto = await loop.create_datagram_endpoint(
            _DnsProtocol,
            local_addr=("0.0.0.0", DNS_PORT),
            reuse_port=False,
        )
    except OSError as exc:
        log.error("captive_dns_bind_failed", port=DNS_PORT, error=str(exc))
        return 2

    http_stop = threading.Event()
    http_thread = threading.Thread(
        target=_run_http_server, args=(http_stop,), name="captive-http", daemon=True
    )
    http_thread.start()

    log.info("captive_dns_service_ready", captured=len(CAPTURED_HOSTS))
    try:
        await stop.wait()
    finally:
        http_stop.set()
        try:
            transport.close()
        except Exception:
            pass
        http_thread.join(timeout=2.0)

    log.info("captive_dns_service_stopped")
    return 0


def main() -> None:
    try:
        rc = asyncio.run(_amain())
    except KeyboardInterrupt:
        rc = 0
    sys.exit(rc)


if __name__ == "__main__":
    main()
