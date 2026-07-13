#!/usr/bin/env bash
# =============================================================================
# run-compute-node-macos.sh — run a workstation/compute node on a dev laptop.
#
# Builds and launches the two daemons a workstation node needs locally — the
# native HTTP control surface (ados-control) and the compute engine
# (ados-compute) — pointed at a self-contained dev home under $HOME/.ados so the
# default /run/ados and /etc/ados system paths are never touched. Both daemons
# share the same run dir, config, and pairing file, so the LAN-paired GCS reaches
# the control surface on :8080 and the compute card reads the heartbeat sidecar
# local-first.
#
# Usage:
#   scripts/dev/run-compute-node-macos.sh
#
# Overridable via the environment (sensible defaults seeded otherwise):
#   ADOS_HOME              dev home dir            (default $HOME/.ados)
#   ADOS_COMPUTE_NODE_ID   this node's id          (default mac-<short hostname>)
#
# No machine name or user path is baked in: the device id and node name are
# derived at runtime from `hostname`, and every path hangs off $HOME.
# =============================================================================
set -uo pipefail

# --- Locate the repo + the cargo workspace (the crates dir). -----------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CRATES_DIR="$REPO_ROOT/crates"
# A compute node serves the REAL ONNX detector over a live RTSP feed, so it runs
# an optimized (release) build by default — a debug build's inference is slow
# enough that it can't keep up with the frame stream and mediamtx drops the node
# as a slow reader. Set ADOS_COMPUTE_PROFILE=debug for a fast-iteration build.
PROFILE="${ADOS_COMPUTE_PROFILE:-release}"
if [ "$PROFILE" = "release" ]; then
  CARGO_PROFILE_FLAG="--release"; PROFILE_DIR="release"
else
  CARGO_PROFILE_FLAG=""; PROFILE_DIR="debug"
fi
TARGET_DIR="${CARGO_TARGET_DIR:-$CRATES_DIR/target}/$PROFILE_DIR"

# --- Dev home + run/compute dirs (never the system /run//etc paths). ---------
ADOS_HOME="${ADOS_HOME:-$HOME/.ados}"
mkdir -p "$ADOS_HOME/run" "$ADOS_HOME/compute"

# --- Derive a stable, non-personal node identity from the live hostname. -----
HOSTNAME_SHORT="$(hostname -s 2>/dev/null || echo compute-node)"
HOSTNAME_FULL="$(hostname 2>/dev/null || echo compute-node)"
SLUG="$(printf '%s' "$HOSTNAME_SHORT" | tr '[:upper:] ' '[:lower:]-' | tr -cd 'a-z0-9-')"
[ -n "$SLUG" ] || SLUG="node"
DEVICE_ID="mac-${SLUG}"

# --- Seed a workstation config if absent (atlas on). -------------------------
CONFIG_PATH="$ADOS_HOME/config.yaml"
if [ ! -f "$CONFIG_PATH" ]; then
  echo "seeding $CONFIG_PATH (workstation profile, atlas enabled)"
  cat > "$CONFIG_PATH" <<YAML
agent:
  profile: workstation
  device_id: ${DEVICE_ID}
  name: ${HOSTNAME_FULL}
atlas:
  enabled: true
YAML
fi

# --- Build both daemons (hard-fail: a broken build must stop here). ----------
# ados-compute is built with `coreml` so it serves the REAL ONNX detector on the
# Apple GPU (else it falls back to the mock — one placeholder box). Set
# ADOS_COMPUTE_NO_COREML=1 to build the mock-only node (no ONNX Runtime).
echo "building ados-control + ados-compute ..."
if [ -n "${ADOS_COMPUTE_NO_COREML:-}" ]; then
  ( cd "$CRATES_DIR" && cargo build $CARGO_PROFILE_FLAG -p ados-control && cargo build $CARGO_PROFILE_FLAG -p ados-compute )
else
  ( cd "$CRATES_DIR" && cargo build $CARGO_PROFILE_FLAG -p ados-control && cargo build $CARGO_PROFILE_FLAG -p ados-compute --features coreml )
fi

CONTROL_BIN="$TARGET_DIR/ados-control"
COMPUTE_BIN="$TARGET_DIR/ados-compute"
for bin in "$CONTROL_BIN" "$COMPUTE_BIN"; do
  if [ ! -x "$bin" ]; then
    echo "error: built binary not found: $bin" >&2
    exit 1
  fi
done

# --- Shared runtime environment for both daemons. ----------------------------
# ados-control reads ADOS_CONFIG; ados-compute reads ADOS_CONFIG_YAML — point
# both at the same file. The pairing file is shared so a claim through the
# control surface authorises the compute job API too.
export ADOS_RUN_DIR="$ADOS_HOME/run"
export ADOS_CONFIG="$CONFIG_PATH"
export ADOS_CONFIG_YAML="$CONFIG_PATH"
export ADOS_PAIRING_JSON="$ADOS_HOME/pairing.json"
export ADOS_CONTROL_SOCKET="$ADOS_HOME/run/control.sock"
export ADOS_CONTROL_PORT=8080
export ADOS_COMPUTE_DB="$ADOS_HOME/compute/jobs.db"
# The dataset + artifact work root. Without this it defaults to the system path
# /var/ados/compute/work, which is not writable on a dev laptop, so a reconstruct
# job fails with "create workdir: Permission denied". Keep it under the dev home.
export ADOS_COMPUTE_WORK="${ADOS_COMPUTE_WORK:-$ADOS_HOME/compute/work}"
export ADOS_COMPUTE_BIND=0.0.0.0:8092
export ADOS_ATLAS_ENABLED=1
export ADOS_COMPUTE_NODE_ID="${ADOS_COMPUTE_NODE_ID:-${DEVICE_ID}}"

# --- Perception-offload detector. --------------------------------------------
# Point ADOS_COMPUTE_DETECTOR_MODEL at a COCO detection .onnx to serve REAL
# detections to an offloading drone; otherwise the node serves the mock detector
# (a single placeholder box). Default search: $ADOS_HOME/models/. The input size
# / head / class labels default to a YOLOv8 COCO model and are overridable.
DETECTOR_DEFAULT="$ADOS_HOME/models/yolov8n_coco_640.onnx"
if [ -z "${ADOS_COMPUTE_DETECTOR_MODEL:-}" ] && [ -f "$DETECTOR_DEFAULT" ]; then
  export ADOS_COMPUTE_DETECTOR_MODEL="$DETECTOR_DEFAULT"
fi
if [ -n "${ADOS_COMPUTE_DETECTOR_MODEL:-}" ]; then
  export ADOS_COMPUTE_DETECTOR_INPUT="${ADOS_COMPUTE_DETECTOR_INPUT:-640x640}"
  export ADOS_COMPUTE_DETECTOR_HEAD="${ADOS_COMPUTE_DETECTOR_HEAD:-yolo8}"
  # COCO-80 class labels (the standard YOLO order), so returned classes read as
  # "person"/"car"/... instead of bare indices. Override to match your model.
  export ADOS_COMPUTE_DETECTOR_CLASSES="${ADOS_COMPUTE_DETECTOR_CLASSES:-person,bicycle,car,motorcycle,airplane,bus,train,truck,boat,traffic light,fire hydrant,stop sign,parking meter,bench,bird,cat,dog,horse,sheep,cow,elephant,bear,zebra,giraffe,backpack,umbrella,handbag,tie,suitcase,frisbee,skis,snowboard,sports ball,kite,baseball bat,baseball glove,skateboard,surfboard,tennis racket,bottle,wine glass,cup,fork,knife,spoon,bowl,banana,apple,sandwich,orange,broccoli,carrot,hot dog,pizza,donut,cake,chair,couch,potted plant,bed,dining table,toilet,tv,laptop,mouse,remote,keyboard,cell phone,microwave,oven,toaster,sink,refrigerator,book,clock,vase,scissors,teddy bear,hair drier,toothbrush}"
  echo "detector: $ADOS_COMPUTE_DETECTOR_MODEL (input $ADOS_COMPUTE_DETECTOR_INPUT, head $ADOS_COMPUTE_DETECTOR_HEAD)"
else
  echo "no detector model — serving the MOCK detector (one placeholder box)."
  echo "  drop a COCO .onnx at $DETECTOR_DEFAULT or set ADOS_COMPUTE_DETECTOR_MODEL for real detections."
fi
# Pin a browser-reachable artifact base (this host's LAN IPv4, derived at
# runtime) so reconstruction artifact URLs use a resolvable address instead of
# the drifting mDNS .local hostname the browser cannot resolve.
if [ -z "${ADOS_COMPUTE_PUBLIC_URL:-}" ]; then
  _lan_ip="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || echo 127.0.0.1)"
  export ADOS_COMPUTE_PUBLIC_URL="http://${_lan_ip}:8092"
fi

# --- Launch both, tear both down on exit/signal. -----------------------------
CONTROL_PID=""
COMPUTE_PID=""
cleanup() {
  trap - INT TERM EXIT
  echo
  echo "stopping compute node ..."
  [ -n "$CONTROL_PID" ] && kill "$CONTROL_PID" 2>/dev/null || true
  [ -n "$COMPUTE_PID" ] && kill "$COMPUTE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup INT TERM EXIT

echo "starting ados-control on :$ADOS_CONTROL_PORT (run dir $ADOS_RUN_DIR)"
"$CONTROL_BIN" &
CONTROL_PID=$!

echo "starting ados-compute on $ADOS_COMPUTE_BIND (node $ADOS_COMPUTE_NODE_ID)"
"$COMPUTE_BIN" &
COMPUTE_PID=$!

echo "compute node up — control :$ADOS_CONTROL_PORT, compute :8092. Ctrl-C to stop."
# Block until both exit or a signal fires the trap.
wait "$CONTROL_PID" 2>/dev/null || true
wait "$COMPUTE_PID" 2>/dev/null || true
