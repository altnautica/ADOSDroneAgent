#!/bin/sh
#
# First-boot pairing-code surfacing for the ADOS lite agent on Buildroot.
#
# Lifecycle:
#   1. If /etc/ados/pairing.json exists with a non-empty pair_code, exit early —
#      the operator pre-paired the image at build time or via a prior boot.
#   2. Otherwise, generate a fresh pair code via the agent CLI. The agent must
#      already be running (S99ados-agent-lite starts before this in init order
#      via the K* numbering convention; if the order is reversed in a downstream
#      BSP, this script tolerates a not-yet-running agent and retries briefly).
#   3. Print the code to dmesg + the active console/UART so an operator with a
#      USB-UART cable hooked to the SBC's debug pins can read it without SSH.
#   4. If the board profile reports an attached display (OLED or LCD), render
#      the code there too.
#
# The agent CLI subcommand `pair --autogen` is the spec'd entry point. If that
# subcommand isn't present yet (lite-agent < TBD), the script falls back to
# reading pairing state from the running agent's HTTP API and printing whatever
# beacon code the agent is already broadcasting. Either way the operator gets
# a code to type into Mission Control "Add drone".
#
# TODO(future): wire `ados-agent-lite pair --autogen` once that flag lands.
# Tracked at agents/lite-rs/CHANGELOG.md.

set -u

LOG_TAG="ados-first-boot"
PAIRING_FILE="/etc/ados/pairing.json"
AGENT_BIN="/usr/local/bin/ados-agent-lite"
AGENT_CONFIG="/etc/ados/agent.yaml"
API_BASE="http://127.0.0.1:8080/api/v1"

# Where to print the banner. /dev/console is always present on Buildroot;
# /dev/ttyS0 is the typical UART debug console on Luckfox-class boards.
BANNER_TARGETS="/dev/console /dev/ttyS0"

log() {
	# logger may not exist on the most stripped Buildroot rootfs; fall back
	# to /dev/kmsg which is always writable by root.
	if command -v logger >/dev/null 2>&1; then
		logger -t "$LOG_TAG" -- "$*"
	fi
	if [ -w /dev/kmsg ]; then
		printf '<6>%s: %s\n' "$LOG_TAG" "$*" > /dev/kmsg
	fi
}

# Already paired? Skip the surface.
if [ -s "$PAIRING_FILE" ]; then
	if grep -q '"pair_code"\s*:\s*"[^"]\+"' "$PAIRING_FILE" 2>/dev/null; then
		log "pairing.json already populated; first-boot surface skipped"
		exit 0
	fi
fi

# Wait briefly for the agent's HTTP API to be reachable. The agent service
# starts before this in normal init order, but on slow boards or USB-UART
# arbitration delays the socket may take a moment.
i=0
while [ "$i" -lt 30 ]; do
	if command -v wget >/dev/null 2>&1 && \
		wget -q -T 1 -O /dev/null "$API_BASE/setup/state" >/dev/null 2>&1; then
		break
	fi
	if command -v curl >/dev/null 2>&1 && \
		curl -fsS --max-time 1 -o /dev/null "$API_BASE/setup/state" >/dev/null 2>&1; then
		break
	fi
	sleep 1
	i=$((i + 1))
done

CODE=""

# Preferred path: ask the agent to generate and persist a fresh code.
# TODO(future): the --autogen flag is on the lite-agent backlog. Until it
# lands, the agent's beacon-broadcast loop generates a per-device code we can
# read via the HTTP surface (next branch below).
if [ -x "$AGENT_BIN" ] && "$AGENT_BIN" --help 2>&1 | grep -q -- '--autogen'; then
	if "$AGENT_BIN" --config "$AGENT_CONFIG" pair --autogen >/tmp/.pair-out 2>&1; then
		CODE=$(grep -oE 'PAIR[: ][A-Za-z0-9]{4,8}' /tmp/.pair-out | head -n1 | sed -E 's/PAIR[: ]//')
	fi
	rm -f /tmp/.pair-out
fi

# Fallback: read the unpaired-state heartbeat the agent is already broadcasting.
if [ -z "$CODE" ]; then
	if command -v wget >/dev/null 2>&1; then
		BODY=$(wget -q -T 2 -O - "$API_BASE/pairing/state" 2>/dev/null || true)
	elif command -v curl >/dev/null 2>&1; then
		BODY=$(curl -fsS --max-time 2 "$API_BASE/pairing/state" 2>/dev/null || true)
	else
		BODY=""
	fi
	# Tolerate either { "pair_code": "ABCD" } or { "code": "ABCD" } shapes.
	CODE=$(printf '%s' "$BODY" \
		| grep -oE '"(pair_code|code)"\s*:\s*"[A-Za-z0-9]{4,8}"' \
		| head -n1 \
		| sed -E 's/.*"([A-Za-z0-9]+)".*/\1/')
fi

if [ -z "$CODE" ]; then
	log "could not obtain a pair code from the agent; operator must pair via Mission Control 'Add drone' beacon scan"
	exit 1
fi

BANNER="==== ADOS PAIR CODE: ${CODE} ===="
log "$BANNER"

for tgt in $BANNER_TARGETS; do
	if [ -w "$tgt" ]; then
		printf '\n%s\n\n' "$BANNER" > "$tgt" 2>/dev/null || true
	fi
done

# Best-effort OLED render. The board profile lists optional display peripherals;
# the agent owns the framebuffer/I2C path. We invoke a generic "show banner"
# CLI subcommand if present, otherwise skip silently — the UART surface above
# is the primary channel.
if [ -x "$AGENT_BIN" ] && "$AGENT_BIN" --help 2>&1 | grep -q 'display'; then
	"$AGENT_BIN" display banner "${BANNER}" >/dev/null 2>&1 || true
fi

exit 0
