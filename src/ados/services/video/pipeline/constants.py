"""Module-level constants and regexes used by the video pipeline.

Split out so the regex compile + the wfb tee tunables are reviewable
on their own without scrolling through the orchestration class. The
values are deliberately conservative; see the comments next to each
for the bench history that drove the choice.
"""

from __future__ import annotations

import re

_HEALTH_CHECK_INTERVAL = 5.0

# Local UDP socket the wfb-ng radio reads from on the air side. The radio
# subprocess (`wfb_tx -u 5600 ...`) listens on this port and broadcasts
# each UDP datagram as a single 802.11 frame with FEC, per the wfb-ng
# protocol contract: every UDP datagram going in must be a self-contained
# unit that survives single-packet loss. We therefore wrap the encoded
# H.264 in RTP (RFC 6184) before handing it to wfb_tx — a lost RTP packet
# costs at most one NAL fragment, instead of corrupting the byte stream
# until the next start code (which is what raw-H.264-over-UDP does).
# Receiver wraps with rtph264depay; SDP at /etc/ados/wfb/video.sdp.
# pkt_size keeps each datagram under the 802.11 MTU after wfb-ng overhead.
_WFB_TEE_HOST = "127.0.0.1"
_WFB_TEE_PORT = 5600
_WFB_TEE_PKT_SIZE = 1316
_WFB_TEE_PAYLOAD_TYPE = 96
# Output-progress watchdog: if wfb_tee's ffmpeg stderr stops emitting
# progress tokens (frame= / size= / time= / bitrate=) for this many
# seconds, the process is considered a zombie (alive but not pushing
# UDP packets) and we force a restart via the regular run-loop
# health-check path. 15 s is the practical floor: ffmpeg's RTSP
# handshake (DESCRIBE / SETUP / PLAY) + first IDR wait can take 5-10 s
# on bench cold-start; setting the threshold below that triggers
# false-positive restart cascades during install + reload races.
_WFB_TEE_PROGRESS_TIMEOUT_S = 15.0
# Pattern matched in ffmpeg stderr lines to detect forward progress.
# We spawn ffmpeg with `-progress pipe:2` which writes structured
# key=value lines to stderr once per second. Recognise both the
# structured form (out_time_ms=, total_size=, etc.) and the legacy
# status line tokens for completeness.
_FFMPEG_FRAME_PROGRESS_RE = re.compile(r"\bframe=\s*(\d+)\b")
_FFMPEG_PROGRESS_TOKEN_RE = re.compile(
    r"\b(?:frame|size|time|bitrate|out_time_ms|out_time_us|"
    r"out_time|total_size|fps|dup_frames|drop_frames|"
    r"speed|progress)=",
)
_WFB_TEE_SSRC = "0xCAFE"
