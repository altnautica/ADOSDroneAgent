#!/usr/bin/env bash
# Does this compositor give a fullscreen video client its own hardware plane?
#
# The question decides whether a native video player can be composited UNDER the
# on-screen cockpit by the display hardware, or whether the compositor will fold
# it into the surface it already scans out — in which case going native buys far
# less than it looks like it should, because the compositor stays in the path.
#
# It is asked by measurement rather than by reading documentation, because the
# answer is a property of the compositor's policy on this hardware and not of
# what it says it supports. The method is the same before/during/after count
# that produced the first answer:
#
#   1. count the planes bound to a CRTC with nothing playing
#   2. start a fullscreen client and count again while it renders
#   3. stop it and count a third time
#
# A compositor that promotes the client to its own plane shows a HIGHER count
# during step 2. One that does not shows the same count three times — and a
# candidate that does not move the count is disqualified, however well it
# renders. Rendering correctly is not the property being measured.
#
# THE CLIENT'S BUFFER TYPE IS PART OF THE QUESTION. A wl_shm client hands the
# compositor CPU memory, and a DRM backend can never scan that out directly — so
# a "no plane" result from a wl_shm client says nothing about the underlay. Only
# a client whose buffers are dmabufs can be promoted at all. `--client testsrc`
# is the wl_shm baseline (kept so the first answer stays reproducible byte for
# byte); `glsrc` and `rtsp` are the dmabuf clients that actually test promotion.
#
# Usage:
#   sudo scripts/probe-display-planes.sh                    # 10s clip, wl_shm baseline
#   sudo scripts/probe-display-planes.sh --seconds 20
#   sudo scripts/probe-display-planes.sh --client glsrc      # dmabuf, no decoder needed
#   sudo scripts/probe-display-planes.sh --client rtsp --under-cage
#   sudo scripts/probe-display-planes.sh --client rtsp --under-cage --renderer gles2
#
# Options:
#   --seconds <n>            how long to sample while the client renders (default 10)
#   --client <testsrc|glsrc|rtsp>
#                            testsrc = videotestsrc ! waylandsink   (wl_shm, baseline)
#                            glsrc   = videotestsrc ! glimagesink   (dmabuf via Mesa EGL)
#                            rtsp    = live H.264 ! v4l2h264dec ! waylandsink (dmabuf)
#   --url <rtsp-url>         source for --client rtsp (default rtsp://127.0.0.1:8554/main)
#   --under-cage             run the client inside its own `cage` instead of an
#                            existing compositor. Stop the kiosk first so cage can
#                            take DRM master; this script never touches services.
#   --renderer <pixman|gles2>
#                            WLR_RENDERER for the --under-cage launch. Unset means
#                            inherit whatever wlroots picks on its own. A pixman
#                            (software) renderer is a plausible structural reason
#                            for no promotion, so the effective value is reported.
#   --baseline <n>           the plane count "during" must beat. REQUIRED with
#                            --under-cage: there the compositor starts inside the
#                            measurement window and binds its own plane, so the
#                            pre-client count is not a valid comparison. Run the
#                            wl_shm control first (--under-cage --client testsrc)
#                            and pass its "during" value here.
#
# Needs root: the DRM state this reads is root-only, and reading it without
# privilege returns nothing at all — which is indistinguishable from "no planes
# are bound" and has been misread that way before.

set -uo pipefail

SECONDS_TO_PLAY=10
CLIENT=testsrc
RTSP_URL="rtsp://127.0.0.1:8554/main"
UNDER_CAGE=0
RENDERER=""
BASELINE=""
MEDIAMTX_API="http://127.0.0.1:9997"
while [ $# -gt 0 ]; do
    case "$1" in
        --seconds) SECONDS_TO_PLAY="${2:-10}"; shift 2 ;;
        --client) CLIENT="${2:-testsrc}"; shift 2 ;;
        --url) RTSP_URL="${2:-}"; shift 2 ;;
        --under-cage) UNDER_CAGE=1; shift ;;
        --renderer) RENDERER="${2:-}"; shift 2 ;;
        --baseline) BASELINE="${2:-}"; shift 2 ;;
        -h|--help) sed -n '2,62p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

case "$CLIENT" in
    testsrc|glsrc|rtsp) ;;
    *) echo "probe: unknown --client '$CLIENT' (want testsrc|glsrc|rtsp)" >&2; exit 2 ;;
esac
case "$RENDERER" in
    ""|pixman|gles2) ;;
    *) echo "probe: unknown --renderer '$RENDERER' (want pixman|gles2)" >&2; exit 2 ;;
esac

if [ "$(id -u)" -ne 0 ]; then
    echo "probe: must run as root; the DRM state is root-only and reads empty otherwise" >&2
    exit 2
fi

# Pick the DRM node that actually has planes. Taking the first readable `state`
# is wrong on a board that exposes both a display node and a render-only node
# (a Pi 4 has vc4 KMS on one card and the v3d render node on another): the render
# node's state has no planes at all, so the probe would measure a device that
# structurally cannot answer the question and report a confident zero.
STATE=""
for candidate in /sys/kernel/debug/dri/*/state; do
    [ -r "$candidate" ] || continue
    if [ "$(grep -c '^plane\[' "$candidate")" -gt 0 ]; then
        STATE="$candidate"
        break
    fi
    [ -z "$STATE" ] && STATE="$candidate"   # fall back to something readable
done
if [ -z "$STATE" ]; then
    echo "probe: no readable DRM state under /sys/kernel/debug/dri/*/state" >&2
    echo "probe: is debugfs mounted? (mount -t debugfs none /sys/kernel/debug)" >&2
    exit 3
fi

# The card node matching the DRM state being measured, so `--under-cage` points
# cage at the SAME device the plane count is read from. Letting cage autodetect
# can land it on the render-only node, where it either fails outright or drives a
# device this probe is not watching — either way the number would be meaningless.
dri_index="${STATE#/sys/kernel/debug/dri/}"
dri_index="${dri_index%/state}"
CARD="/dev/dri/card$dri_index"
[ -e "$CARD" ] || CARD=""

# A plane is "bound" when its state names a CRTC. The alternative reading —
# counting plane objects — answers a different and useless question: the display
# engine exposes far more planes than any compositor uses, so that number is a
# hardware constant and never moves.
bound_planes() {
    awk '
        /^plane\[/   { plane = $0; crtc = "" }
        /crtc=/      { if (plane != "" && $0 !~ /crtc=\(null\)/) { bound++; plane = "" } }
        END          { print bound + 0 }
    ' "$STATE"
}

# Same count, but listing which ones, so a changed total can be attributed.
bound_plane_ids() {
    awk '
        /^plane\[/ { split($0, a, "["); split(a[2], b, "]"); id = b[1]; plane = id }
        /crtc=/    { if (plane != "" && $0 !~ /crtc=\(null\)/) { printf "%s ", plane; plane = "" } }
        END        { print "" }
    ' "$STATE"
}

compositor() {
    for p in labwc cage sway weston mutter kwin_wayland; do
        pgrep -x "$p" >/dev/null 2>&1 && { echo "$p"; return; }
    done
    echo "none detected"
}

# mediamtx's own cumulative byte counters for the path being pulled. A single
# snapshot proves nothing; the caller compares two samples over a window. This
# is the reliable source — tcpdump and /proc/net/udp have both produced false
# zeros against this exact pipeline.
#
# `bytesReceived` is what the PUBLISHER pushed in, so it advances whether or not
# this probe ever connected — using it as the connectivity guard would pass
# vacuously. `bytesSent` is what mediamtx served to READERS, which is the number
# that only moves because our client is pulling. Both are reported; only
# `bytesSent` (plus a non-empty reader list) gates the measurement.
mediamtx_field() {
    local path="$1" field="$2"
    curl -s --max-time 3 "$MEDIAMTX_API/v3/paths/get/$path" 2>/dev/null \
        | tr ',' '\n' | grep -o "\"$field\":[0-9]*" | head -1 | cut -d: -f2
}

# Number of readers mediamtx currently has on the path. Zero while the client is
# supposed to be pulling means the client is not the thing being measured.
mediamtx_readers() {
    local path="$1"
    curl -s --max-time 3 "$MEDIAMTX_API/v3/paths/get/$path" 2>/dev/null \
        | grep -o '"readers":\[[^]]*\]' | head -1 | grep -c '"type"'
}

# The path component of an rtsp:// URL (mediamtx names its paths without the
# leading slash), used only for the connectivity guard.
rtsp_path() {
    echo "${1##*/}"
}

# The client argv, as an array so nothing is re-split by the shell.
client_argv() {
    case "$CLIENT" in
        # Unchanged wl_shm baseline: reproduces the committed first measurement
        # byte for byte, so the old result stays checkable against a new box.
        testsrc)
            CLIENT_ARGV=(gst-launch-1.0 -q videotestsrc pattern=smpte is-live=true
                '!' video/x-raw,width=800,height=480,framerate=30/1
                '!' waylandsink fullscreen=true)
            CLIENT_BUFFERS="wl_shm (CPU memory — cannot be scanned out; baseline only)"
            ;;
        # A genuine dmabuf client that needs no hardware decoder: glimagesink
        # renders through Mesa EGL on Wayland, so its buffers are dmabufs. This
        # is what answers the promotion question when the V4L2 decoder is absent.
        glsrc)
            # No `fullscreen` property here: `glimagesink` does not have one
            # (that is `waylandsink`), and passing it makes gst-launch reject the
            # pipeline — which reads as "the client never displayed anything"
            # rather than as an answer. Under cage the single client is fullscreen
            # anyway, so nothing is lost.
            CLIENT_ARGV=(gst-launch-1.0 -q videotestsrc pattern=smpte is-live=true
                '!' video/x-raw,width=800,height=480,framerate=30/1
                '!' glimagesink)
            CLIENT_BUFFERS="dmabuf (Mesa EGL via glimagesink)"
            ;;
        # The real stream, hardware-decoded straight into dmabufs. On a Pi 4 the
        # decoder is the V4L2 stateful `v4l2h264dec` (/dev/video10). There is
        # deliberately no software-decode fallback: `avdec_h264 ! videoconvert`
        # emits system memory, a DMABuf caps filter after it cannot negotiate,
        # and the resulting broken pipeline would read as a measured "no plane"
        # when in fact nothing was measured. No decoder means UNTESTABLE.
        rtsp)
            CLIENT_ARGV=(gst-launch-1.0 -q rtspsrc "location=$RTSP_URL" latency=0 protocols=tcp
                '!' rtph264depay '!' h264parse
                '!' v4l2h264dec capture-io-mode=dmabuf
                '!' waylandsink fullscreen=true)
            CLIENT_BUFFERS="dmabuf (v4l2h264dec capture-io-mode=dmabuf)"
            ;;
    esac
}

client_argv

# Under cage the client gets its own compositor with DRM master, which is the
# configuration the kiosk actually runs — so the launch mirrors the kiosk unit's
# environment rather than hoping cage guesses right.
#
#  * XDG_RUNTIME_DIR is MANDATORY: wlroots refuses to create its socket without
#    one and cage exits immediately with "XDG_RUNTIME_DIR is not set". Under
#    `sudo` the caller's value is stripped, so the probe supplies the same
#    `/run/user/0` the `ados-kiosk` unit sets, creating it if it is absent.
#  * WLR_DRM_DEVICES is pinned to the card whose plane state this probe reads.
#    Autodetection can pick the render-only node, where cage drives a device the
#    measurement is not watching — a silent wrong answer rather than an error.
LAUNCH_ARGV=("${CLIENT_ARGV[@]}")
LAUNCH_ENV=()
CAGE_RUNTIME_DIR=""
if [ "$UNDER_CAGE" -eq 1 ]; then
    if ! command -v cage >/dev/null 2>&1; then
        echo "probe: --under-cage requested but cage is not installed" >&2
        exit 3
    fi
    CAGE_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    if [ ! -d "$CAGE_RUNTIME_DIR" ]; then
        mkdir -p "$CAGE_RUNTIME_DIR" 2>/dev/null
        chmod 700 "$CAGE_RUNTIME_DIR" 2>/dev/null
    fi
    if [ ! -d "$CAGE_RUNTIME_DIR" ]; then
        echo "probe: cannot provide cage an XDG_RUNTIME_DIR ($CAGE_RUNTIME_DIR)" >&2
        echo "probe: cage exits immediately without one; nothing would be measured" >&2
        exit 3
    fi
    LAUNCH_ENV+=("XDG_RUNTIME_DIR=$CAGE_RUNTIME_DIR")
    [ -n "$CARD" ] && LAUNCH_ENV+=("WLR_DRM_DEVICES=$CARD")
    if [ -n "$RENDERER" ]; then
        LAUNCH_ENV+=("WLR_RENDERER=$RENDERER")
        # Mirrors the kiosk's own cage environment: a software renderer cannot
        # drive a hardware cursor plane, and asking it to leaves a black screen.
        [ "$RENDERER" = "pixman" ] && LAUNCH_ENV+=("WLR_NO_HARDWARE_CURSORS=1")
    fi
    LAUNCH_ARGV=(cage -- "${CLIENT_ARGV[@]}")
fi

effective_renderer() {
    if [ "$UNDER_CAGE" -eq 1 ]; then
        [ -n "$RENDERER" ] && { echo "$RENDERER (forced)"; return; }
        echo "${WLR_RENDERER:-<unset — wlroots picks>}"
        return
    fi
    echo "${WLR_RENDERER:-<unset — the existing compositor picks>}"
}

echo "compositor:        $(compositor)"
echo "client:            $CLIENT"
echo "drm card:          ${CARD:-<none matched — cage autodetects>}"
echo "client buffers:    $CLIENT_BUFFERS"
echo "under cage:        $([ "$UNDER_CAGE" -eq 1 ] && echo yes || echo no)"
echo "WLR_RENDERER:      $(effective_renderer)"
[ "$CLIENT" = "rtsp" ] && echo "source:            $RTSP_URL"
echo "drm state:         $STATE"
echo "planes exposed:    $(grep -c '^plane\[' "$STATE")"
echo

before_n="$(bound_planes)"; before_ids="$(bound_plane_ids)"
echo "before:            $before_n bound  [$before_ids]"

if ! command -v gst-launch-1.0 >/dev/null 2>&1; then
    echo "probe: gst-launch-1.0 not found; cannot start a client" >&2
    exit 3
fi

# Own process group, and torn down as a group. Killing only the launcher PID
# leaves `cage` running and holding DRM master: the probe then blocks forever in
# `wait`, the operator's kiosk cannot restart, and the box is left with an
# orphaned compositor on the panel. That is the orphaned-child failure this
# codebase has a standing rule about, and a measurement script is not exempt.
env "${LAUNCH_ENV[@]}" setsid "${LAUNCH_ARGV[@]}" >/tmp/probe-client.log 2>&1 &
client=$!
# `setsid` makes the child a session leader, so its process-group id IS its pid.
# Do NOT read the pgid back with `ps`: setsid() only takes effect after the exec,
# so a read that wins the race returns THIS script's own group — and killing that
# group kills the probe, its parent shell and the operator's SSH session.
client_pgid="$client"
own_pgid="$(ps -o pgid= -p $$ 2>/dev/null | tr -dc '0-9')"
if [ -n "$own_pgid" ] && [ "$client_pgid" = "$own_pgid" ]; then
    echo "probe: refusing to target this script's own process group" >&2
    client_pgid=""
fi

stop_client() {
    if [ -z "${client_pgid:-}" ]; then
        kill -TERM "$client" 2>/dev/null
        return 0
    fi
    kill -TERM -- "-$client_pgid" 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        kill -0 -- "-$client_pgid" 2>/dev/null || return 0
        sleep 0.5
    done
    # cage does not always go on TERM; the group must not survive this script.
    kill -KILL -- "-$client_pgid" 2>/dev/null
}
trap stop_client EXIT

# For the live stream, prove the client actually pulled data before believing
# any plane count. A probe that never connected measures nothing, and reporting
# its "no plane" as a verdict is exactly the wrong-conclusion failure mode.
#
# Polled rather than a single window: an rtspsrc DESCRIBE/SETUP/PLAY over TCP
# plus V4L2 decoder init routinely takes several seconds, and a fixed 2 s sample
# would fail a perfectly healthy client. The poll only costs latency when the
# client really is not connecting.
RTSP_CONNECT_TIMEOUT=15
if [ "$CLIENT" = "rtsp" ]; then
    path="$(rtsp_path "$RTSP_URL")"
    pub_a="$(mediamtx_field "$path" bytesReceived)"
    tx_a="$(mediamtx_field "$path" bytesSent)"
    pulled=0
    tx_b="$tx_a"; pub_b="$pub_a"; readers=0
    waited=0
    while [ "$waited" -lt "$RTSP_CONNECT_TIMEOUT" ]; do
        sleep 2; waited=$((waited + 2))
        pub_b="$(mediamtx_field "$path" bytesReceived)"
        tx_b="$(mediamtx_field "$path" bytesSent)"
        readers="$(mediamtx_readers "$path")"
        if [ -n "$tx_a" ] && [ -n "$tx_b" ] && [ "$tx_b" -gt "$tx_a" ] \
           && [ "${readers:-0}" -ge 1 ]; then
            pulled=1
            break
        fi
    done
    echo "publisher bytes:   ${pub_a:-?} -> ${pub_b:-?} (mediamtx bytesReceived, path '$path', ${waited}s)"
    echo "reader bytes:      ${tx_a:-?} -> ${tx_b:-?} (mediamtx bytesSent, ${readers:-0} reader(s))"
    if [ "$pulled" -ne 1 ]; then
        echo
        echo "probe: the client never pulled the stream; nothing was measured." >&2
        echo "probe: mediamtx bytesSent for '$path' did not advance within ${RTSP_CONNECT_TIMEOUT}s," >&2
        echo "probe: and/or the path has no reader attached. A 'no plane' number" >&2
        echo "probe: taken here would be a measurement of nothing." >&2
        tail -5 /tmp/probe-client.log >&2
        exit 3
    fi
fi

# Let the surface actually reach the screen. A count taken before the first
# frame is committed measures nothing and would read as "no plane".
sleep 3
if ! kill -0 "$client" 2>/dev/null; then
    echo
    echo "probe: the client exited immediately; it never displayed anything." >&2
    echo "probe: this is NOT an answer to the plane question — fix the client first." >&2
    tail -5 /tmp/probe-client.log >&2
    exit 4
fi

samples=""
for _ in $(seq 1 "$SECONDS_TO_PLAY"); do
    samples="$samples $(bound_planes)"
    sleep 1
done
during_max="$(echo "$samples" | tr ' ' '\n' | grep -v '^$' | sort -n | tail -1)"
during_ids="$(bound_plane_ids)"
echo "during:            $during_max bound (max over ${SECONDS_TO_PLAY}s)  [$during_ids]"
echo "  per-second:     $samples"

stop_client
trap - EXIT
sleep 2
after_n="$(bound_planes)"; after_ids="$(bound_plane_ids)"
echo "after:             $after_n bound  [$after_ids]"
echo

# Which number "during" has to beat.
#
# WITHOUT --under-cage the compositor is already running when `before` is taken,
# so `before` is the right comparison: any extra plane appeared because of the
# client.
#
# WITH --under-cage it is NOT. `before` is sampled with no compositor at all, and
# cage then binds its OWN primary (and cursor) plane inside the measurement
# window — so EVERY arm reads at least +1, including a wl_shm client that a DRM
# backend physically cannot scan out. Comparing against `before` there would
# print "DOES promote" for a client holding CPU memory, which is a false pass on
# the one question this script exists to answer. The honest comparison is against
# a wl_shm CONTROL run on the same box and renderer: promotion is
# `during_dmabuf > during_wl_shm`. Pass that control's `during` as --baseline.
if [ -n "$BASELINE" ]; then
    compare_to="$BASELINE"
    compare_label="--baseline $BASELINE (a control run's 'during')"
elif [ "$UNDER_CAGE" -eq 1 ]; then
    echo "VERDICT: INDETERMINATE — no baseline to compare against."
    echo "  $before_n -> $during_max -> $after_n"
    echo "  client: $CLIENT ($CLIENT_BUFFERS)"
    echo "  Under --under-cage the compositor starts INSIDE the window, so it binds"
    echo "  its own plane and 'before' is not a valid comparison: every client,"
    echo "  including a wl_shm one that cannot be scanned out, reads higher."
    echo "  Run the wl_shm control first and feed its 'during' back in:"
    echo "    $0 --under-cage --client testsrc            # note the 'during' value"
    echo "    $0 --under-cage --client $CLIENT --baseline <that value>"
    exit 2
else
    compare_to="$before_n"
    compare_label="the pre-client count"
fi

if [ "$during_max" -gt "$compare_to" ]; then
    echo "VERDICT: the compositor DOES promote a fullscreen client to its own plane."
    echo "  $before_n -> $during_max -> $after_n, against $compare_label"
    echo "  client: $CLIENT ($CLIENT_BUFFERS)"
    echo "  A video underlay with the cockpit composited above it is available here."
    exit 0
fi

echo "VERDICT: the compositor does NOT give the client a plane."
echo "  $before_n -> $during_max -> $after_n, against $compare_label"
echo "  client: $CLIENT ($CLIENT_BUFFERS)"
echo "  It is compositing that surface into the plane it already scans out."
if [ "$CLIENT" = "testsrc" ]; then
    echo "  NOTE: this client is wl_shm. A DRM backend can never scan out CPU"
    echo "  memory, so this result is the baseline, NOT an answer about the"
    echo "  underlay. Re-run with --client glsrc or --client rtsp."
else
    echo "  This candidate is disqualified for the underlay; rendering correctly is"
    echo "  not the property being measured."
fi
exit 1
