"""Argv / config builders for the mediamtx subprocess and the ffmpeg sidecar.

Pure builders with no side effects. Both functions return data
structures the manager hands to the subprocess spawner. Keeping the
long argv lists and the YAML dict here keeps ``manager.py`` focused on
process lifecycle, restart logic, and graceful shutdown.
"""

from __future__ import annotations

from pathlib import Path

from .rtsp_config import GROUND_RTSP_PATH, GROUND_WHEP_PATH


def build_mediamtx_yaml(
    api_port: int,
    rtsp_port: int,
    webrtc_port: int,
    lan_ips: list[str],
) -> dict:
    """Return the ground-profile mediamtx YAML config as a dict.

    Same base shape as the air-side generator but the ``/main`` path is
    declared with ``source: publisher`` so ffmpeg can push into it. The
    WHEP path ``ados/whep`` is aliased to the same source.

    Caller is responsible for serialising to disk and threading the
    final path through to the mediamtx subprocess.
    """
    config: dict = {
        "logLevel": "warn",
        "api": True,
        "apiAddress": f":{api_port}",
        "rtsp": True,
        "rtspAddress": f":{rtsp_port}",
        # Per-reader write queue depth, measured in RTP packets
        # (not bytes). When a reader's queue overflows, mediamtx
        # logs "reader is too slow, discarding N frames" and the
        # reader's stream becomes corrupted — the canonical
        # browser-WebRTC "freeze on last frame, refresh restores"
        # symptom. Headroom math: at ~30 fps and worst-case
        # 50 RTP packets per frame for 1280x720 H.264 = 1500
        # packets/sec; 4096 holds ~2.7 s of buffer. Survives a
        # routine Chrome GC pause, a paint frame, a tab focus
        # change, and a brief Pi 4B swap-in stall — all the
        # things the earlier 512-packet ceiling caught and
        # turned into reader eviction. Memory cost: ~5 MB per
        # active reader. Acceptable.
        "writeQueueSize": 4096,
        # NB: do NOT override readTimeout / writeTimeout below
        # the gortsplib defaults (10 s / 10 s). A 5 s ceiling
        # under low-RAM swap pressure caught system stutters
        # the 10 s default absorbs, and produced a deterministic
        # 120 s publisher eviction cycle on Pi 4B 1 GB boards.
        # The 10 s default gives the kernel enough room to page
        # mediamtx's working set back in without tearing the
        # publisher session.
        "udpMaxPayloadSize": 1472,
        "webrtc": True,
        "webrtcAddress": f":{webrtc_port}",
        "webrtcAllowOrigin": "*",
        "webrtcIPsFromInterfaces": False,
        "webrtcIPsFromInterfacesList": [],
        "webrtcHandshakeTimeout": "15s",
        "webrtcLocalUDPAddress": (
            f"{lan_ips[0]}:8189" if lan_ips else ":8189"
        ),
        "webrtcLocalTCPAddress": (
            f"{lan_ips[0]}:8189" if lan_ips else ":8189"
        ),
        "webrtcICEServers2": [
            {"url": "stun:stun.l.google.com:19302"},
            {"url": "stun:stun1.l.google.com:19302"},
            {"url": "stun:stun2.l.google.com:19302"},
            {"url": "stun:stun.cloudflare.com:3478"},
            {"url": "stun:global.stun.twilio.com:3478"},
        ],
        # LL-HLS as a parallel transport. mediamtx serves the same
        # /main path simultaneously over WebRTC and HLS. WebRTC's
        # failure mode under load is "freeze on last frame and
        # require renegotiation"; HLS's failure mode is "drift
        # latency by a second, keep playing." Browsers that hit
        # a WebRTC freeze can fall back to the HLS endpoint at
        # http://<host>:8888/<path>/index.m3u8 and keep watching.
        # Works with the existing -c copy publish path — mediamtx
        # remuxes H.264 NAL units into fragmented MP4 without
        # re-encoding. Target latency on LAN: ~600 ms. Memory
        # cost: ~10-15 MB extra for the segmenter.
        "hls": True,
        "hlsAddress": ":8888",
        "hlsAllowOrigin": "*",
        "hlsAlwaysRemux": True,
        "hlsVariant": "lowLatency",
        "hlsSegmentCount": 7,
        "hlsSegmentDuration": "200ms",
        "hlsPartDuration": "100ms",
        "paths": {
            # ffmpeg pushes RTSP here with -c copy from udp://:5600
            GROUND_RTSP_PATH: {"source": "publisher"},
            # WHEP alias. sourceOnDemand True keeps the secondary
            # internal reader idle until a WHEP client actually
            # connects to /ados/whep. The earlier `False` setting
            # ran a permanent self-pull of /main into /ados/whep
            # even when no one was using the WHEP path (browsers
            # hit /main directly via webrtc:8889 endpoint), which
            # doubled mediamtx's internal goroutine count and
            # held a second RTSP reader open against the publisher
            # all the time. The cold-start delay on WHEP first
            # connect is ~1 GOP (~500 ms at fps/2 keyint); a fair
            # trade for half the routine load.
            GROUND_WHEP_PATH: {
                "source": f"rtsp://127.0.0.1:{rtsp_port}/{GROUND_RTSP_PATH}",
                "sourceOnDemand": True,
            },
        },
    }
    if lan_ips:
        config["webrtcAdditionalHosts"] = lan_ips
    return config


def build_ffmpeg_ingest_argv(
    binary: str,
    sdp_path: Path,
    rtsp_url: str,
) -> list[str]:
    """Return the argv that drives the UDP-RTP-to-RTSP ffmpeg sidecar.

    Reads via ``-f sdp -i <path>`` so ffmpeg knows the codec without
    an RTSP DESCRIBE round-trip (wfb_rx is a one-way broadcaster, no
    RTSP server to query). ``-c copy`` keeps it zero-transcode.
    """
    return [
        binary,
        "-fflags", "nobuffer",
        "-flags", "low_delay",
        # NB: do NOT add `-max_delay 0` here. We tried it as a
        # latency micro-optimization and it broke codec discovery
        # — ffmpeg returned "Could not find codec parameters for
        # stream 0 (Video: h264, none): unspecified size" because
        # the flag overrode the probesize/analyzeduration window.
        # The codec params (width/height/profile/level) only
        # arrive inline in the first IDR, which can take a couple
        # of seconds after wfb_rx hands over the first packets.
        "-protocol_whitelist", "file,udp,rtp",
        # `-probesize 5M -analyzeduration 5M` give ffmpeg up to
        # 5 seconds (or 5 MB) to discover the H.264 SPS/PPS from
        # the incoming RTP stream. The SDP carries only the
        # encoding name + clock rate; codec config (width/height/
        # profile/level) arrives inline in the first IDR.
        # NB: bench validation surfaced a race when this was
        # tightened to 1M/1s — even with the drone encoder at
        # keyint=15 (IDR every 500 ms), the first RTP packets
        # landing in ffmpeg's parser are mid-GOP P-frames with
        # no SPS/PPS, and ffmpeg threw `decode_slice_header
        # error` + `unspecified size` before an IDR arrived.
        # 20M/20s is the safety margin: cold restarts under load
        # (Pi 4B class, swap pressure, GOP > 1 s) sometimes wait
        # past 5 s for the first IDR and the older 5M/5s caused
        # an `unspecified size` death loop with mandatory 5 s
        # backoff between each retry. 20 s is invisible on a
        # healthy boot because probe exits as soon as an IDR is
        # found, not after the full window.
        "-probesize", "20M",
        "-analyzeduration", "20M",
        # RTP demuxer reorder + max-delay window. ffmpeg's default
        # `max_delay = 500_000` (500 ms) is what produces the
        # `[sdp @ ...] max delay reached. need to consume packet`
        # + `RTP: missed N packets` cascade on any sub-second
        # system stutter (swap thrash, USB sysfs walk, kernel
        # task-group rebalance). Widening to 2 s lets the demuxer
        # absorb a brief stall and resume without dropping a
        # whole burst of packets. `reorder_queue_size` is the
        # max number of packets held for reorder; bumping from
        # the libav default 500 to a matched 256 keeps the
        # buffer bounded for a 4 Mbps live stream.
        "-max_delay", "2000000",
        "-reorder_queue_size", "256",
        "-f", "sdp",
        "-i", str(sdp_path),
        "-c:v", "copy",
        # NO h264_mp4toannexb here: rtph264depay already emits
        # Annex-B (start-code-prefixed) NAL units; the bsf was a
        # leftover from the old `-f h264 -i udp://` path that
        # received raw bytes. Applying it twice corrupts the
        # bitstream's NAL boundaries.
        # `-muxdelay 0 -muxpreload 0 -flush_packets 1` strip
        # ffmpeg's default 0.7 s mux delay + 0.5 s preload +
        # output-side packet aggregation; for live RTSP push we
        # want every packet emitted as soon as encoded.
        "-muxdelay", "0",
        "-muxpreload", "0",
        "-flush_packets", "1",
        "-f", "rtsp",
        "-rtsp_transport", "tcp",
        rtsp_url,
    ]


__all__ = [
    "build_mediamtx_yaml",
    "build_ffmpeg_ingest_argv",
]
