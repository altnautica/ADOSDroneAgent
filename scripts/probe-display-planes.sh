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
# Usage:
#   sudo scripts/probe-display-planes.sh              # 10s clip, autodetect
#   sudo scripts/probe-display-planes.sh --seconds 20
#
# Needs root: the DRM state this reads is root-only, and reading it without
# privilege returns nothing at all — which is indistinguishable from "no planes
# are bound" and has been misread that way before.

set -uo pipefail

SECONDS_TO_PLAY=10
while [ $# -gt 0 ]; do
    case "$1" in
        --seconds) SECONDS_TO_PLAY="${2:-10}"; shift 2 ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "probe: must run as root; the DRM state is root-only and reads empty otherwise" >&2
    exit 2
fi

STATE=""
for candidate in /sys/kernel/debug/dri/*/state; do
    [ -r "$candidate" ] && { STATE="$candidate"; break; }
done
if [ -z "$STATE" ]; then
    echo "probe: no readable DRM state under /sys/kernel/debug/dri/*/state" >&2
    echo "probe: is debugfs mounted? (mount -t debugfs none /sys/kernel/debug)" >&2
    exit 3
fi

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

echo "compositor:        $(compositor)"
echo "drm state:         $STATE"
echo "planes exposed:    $(grep -c '^plane\[' "$STATE")"
echo

before_n="$(bound_planes)"; before_ids="$(bound_plane_ids)"
echo "before:            $before_n bound  [$before_ids]"

if ! command -v gst-launch-1.0 >/dev/null 2>&1; then
    echo "probe: gst-launch-1.0 not found; cannot start a client" >&2
    exit 3
fi

# A synthetic source rather than the live video feed: the question is whether a
# fullscreen client gets a plane, and tying the answer to whether the radio
# happens to be delivering frames would make a quiet link look like a failed
# probe.
gst-launch-1.0 -q videotestsrc pattern=smpte is-live=true \
    ! video/x-raw,width=800,height=480,framerate=30/1 \
    ! waylandsink fullscreen=true >/tmp/probe-client.log 2>&1 &
client=$!
trap 'kill "$client" 2>/dev/null' EXIT

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

kill "$client" 2>/dev/null
wait "$client" 2>/dev/null
trap - EXIT
sleep 2
after_n="$(bound_planes)"; after_ids="$(bound_plane_ids)"
echo "after:             $after_n bound  [$after_ids]"
echo

if [ "$during_max" -gt "$before_n" ]; then
    echo "VERDICT: the compositor DOES promote a fullscreen client to its own plane."
    echo "  $before_n -> $during_max -> $after_n"
    echo "  A video underlay with the cockpit composited above it is available here."
    exit 0
fi

echo "VERDICT: the compositor does NOT give the client a plane."
echo "  $before_n -> $during_max -> $after_n, unchanged while a client rendered."
echo "  It is compositing that surface into the plane it already scans out."
echo "  This candidate is disqualified for the underlay; rendering correctly is"
echo "  not the property being measured."
exit 1
