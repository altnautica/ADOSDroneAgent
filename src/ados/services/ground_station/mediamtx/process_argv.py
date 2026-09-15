"""Argv builder for the ffmpeg sidecar that publishes the radio stream.

A pure builder with no side effects: it returns the argv the manager hands to
the subprocess spawner, keeping the long flag list out of ``manager.py``.

The mediamtx YAML is deliberately NOT built here. There is exactly one
mediamtx config generator for the whole fleet — the Rust
``ados_video::mediamtx`` renderer — and the ground station reaches it through
``ados-groundlink --emit-mediamtx-config``. The second generator this module
used to carry had drifted from that one on ``writeQueueSize``,
``udpMaxPayloadSize``, the WebRTC handshake and STUN-gather timeouts, the STUN
server list, the TCP ICE candidate and ``rtspTransports``, with no gate able to
see the divergence.
"""

from __future__ import annotations

from pathlib import Path


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
        # Force the periodic status report to stderr as plain newline-
        # terminated `key=value` lines once per second. THIS IS THE LIVENESS
        # SIGNAL, not decoration.
        #
        # ffmpeg suppresses the status line entirely when stderr is not a tty,
        # and it block-buffers stderr behind a subprocess pipe. That is why the
        # ingest's stall watchdog was disabled: its `frame=` parser saw nothing
        # for many seconds on a perfectly healthy ffmpeg because the buffer had
        # not flushed yet, and its other signal (`/proc/<pid>/io` wchar) barely
        # advanced because Linux io accounting does not consistently count the
        # small recurring socket write()s of a per-frame RTSP push. Both
        # false-positived, the watchdog reaped ffmpeg every ~10 s, and the
        # operator saw "video freezes after a few seconds".
        #
        # `-progress pipe:2` removes both failure modes at once: ffmpeg emits
        # and FLUSHES a structured block including `total_size=N`, the
        # cumulative count of bytes the muxer has written to the RTSP output.
        # A wedged ffmpeg keeps printing `progress=continue` with a FROZEN
        # `total_size`, so keying liveness off that counter ADVANCING is direct
        # proof the publish is still moving bytes. Same mechanism the air-side
        # wfb tap already uses.
        "-progress", "pipe:2",
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
        # RTP demuxer reorder + max-delay window.
        #
        # These sit in front of an ALREADY-IN-ORDER stream: wfb_rx's aggregator
        # emits fragments strictly in block/fragment order and FEC-recovers a
        # gap before forwarding, so the demuxer never sees genuine reordering —
        # only genuine loss. A deep reorder queue therefore buys nothing and
        # costs the one thing this path cannot afford: on any packet gap the
        # demuxer held up to 256 packets (~170 ms at 4 Mbps) and, in the
        # pathological case, the full 2 s `max_delay` before flushing. That is
        # exactly the "video is a second behind after a link blip" symptom, and
        # it was the single largest configured buffer in the whole chain.
        #
        # 200 ms / 16 packets is sized to absorb a scheduler hiccup and nothing
        # more. The stutter problem the old 2 s widening was reaching for is
        # caught directly now by the delta-counter watchdogs (`total_size` here
        # and mediamtx's `bytesReceived` on the far side of the socket), which
        # is a detector rather than a buffer.
        "-max_delay", "200000",
        "-reorder_queue_size", "16",
        "-f", "sdp",
        "-i", str(sdp_path),
        "-c:v", "copy",
        # Re-insert SPS/PPS NAL units inline before every IDR frame, so a
        # WebRTC depacketizer that lost sync after a transient RTP loss
        # re-bootstraps its decoder context on the next keyframe (~0.5 s at the
        # drone's 30 fps + GOP 15) instead of freezing on the last decoded frame
        # while the PeerConnection still reads "connected".
        #
        # The drone encoder now repeats the parameter sets in-band per IDR on
        # every encode path, so on a current pair this filter is a no-op — the
        # bsf skips a packet that already starts with its extradata. It stays
        # because the republish here is the LAST place the property can be
        # guaranteed for the ground station's own WHEP readers, and because an
        # older drone on the far side of the link is not this node's to fix.
        # Zero re-encode cost (it operates on the NAL stream, not pixels) and no
        # added latency.
        "-bsf:v", "dump_extra=freq=keyframe",
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


__all__ = ["build_ffmpeg_ingest_argv"]
