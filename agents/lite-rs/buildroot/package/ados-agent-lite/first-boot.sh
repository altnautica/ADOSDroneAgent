#!/bin/sh
#
# First-boot pairing surface for the ADOS lite agent.
#
# Idempotent: the script writes a sentinel after the first successful run
# and exits 0 on any subsequent invocation. Operators who want to
# re-surface the pair code (e.g. they didn't see the UART banner the
# first time) remove /etc/ados/.first-boot-done and re-run via the
# busybox sysv-rc init: `/etc/init.d/S98ados-first-boot start`.
#
# Lifecycle:
#   1. If the sentinel exists, exit 0.
#   2. Ask the running agent to mint-or-return its current pair code via
#      `ados-agent-lite pair --autogen`. The agent persists the code to
#      pairing.json under the canonical TTL semantics, so this script
#      is forward-compatible with operators who pre-populate pairing.json
#      via a build-time overlay.
#   3. Print the code to logger + /dev/kmsg + the active UART(s).
#   4. If an SSD1306-class OLED is detected on I2C bus 1 at 0x3C, render
#      the code there too via `ados-agent-lite display --message`.
#   5. Touch the sentinel.
#
# Failure mode: if the agent isn't running yet, the script exits 1 and
# the operator can manually re-run after the agent service is up. The
# sentinel is NOT written on failure so the next invocation retries.

set -eu

PAIRING_JSON=/etc/ados/pairing.json
SENTINEL=/etc/ados/.first-boot-done
AGENT_BIN=/usr/local/bin/ados-agent-lite

if [ -f "$SENTINEL" ]; then
    exit 0
fi

# Ensure the parent directory exists so `touch "$SENTINEL"` at the end
# does not fail on a freshly-flashed rootfs where /etc/ados/ may not yet
# carry the agent's expected layout.
mkdir -p "$(dirname "$SENTINEL")"

# Generate-or-return a pair code via the agent. `pair --autogen` is the
# canonical entry point: it asks PairingStore::get_or_create_code(),
# persists, and prints the line `==== ADOS PAIR CODE: XXXXXX ====` to
# stdout. We capture stdout, extract the 6-character code, and re-emit
# it on every surface. The agent must be reachable; the busybox init
# orders S98 before S99, but a slow boot can flip that — handle the
# missing-agent case explicitly rather than silently swallowing.
PAIR_OUT=""
if [ ! -x "$AGENT_BIN" ]; then
    logger -t ados-first-boot "ERROR: $AGENT_BIN missing or not executable"
    echo "first-boot: agent binary missing at $AGENT_BIN" > /dev/kmsg 2>/dev/null || true
    exit 1
fi

if ! PAIR_OUT=$("$AGENT_BIN" pair --autogen 2>&1); then
    logger -t ados-first-boot "ERROR: pair --autogen failed; agent not running?"
    echo "first-boot: pair --autogen failed" > /dev/kmsg 2>/dev/null || true
    exit 1
fi

# Pair codes are 6 uppercase alphanumerics — the same charset the
# PairingStore generator emits. `head -n1` keeps the first match in
# case the agent prints multiple banners (which it does not today,
# but the pattern is defensive).
CODE=$(printf '%s\n' "$PAIR_OUT" | grep -oE '[A-Z0-9]{6}' | head -n1 || true)

if [ -z "$CODE" ]; then
    logger -t ados-first-boot "ERROR: could not extract pair code from agent output"
    echo "first-boot: pair --autogen produced no code" > /dev/kmsg 2>/dev/null || true
    exit 1
fi

BANNER="==== ADOS PAIR CODE: $CODE ===="

# logger lands in /var/log/messages on Buildroot rootfs.
logger -t ados-first-boot "$BANNER"

# /dev/kmsg surfaces in dmesg + the kernel console (if a serial
# console is configured at boot). Always writable by root.
echo "$BANNER" > /dev/kmsg 2>/dev/null || true

# UART banner: print to the canonical Luckfox debug UART plus the
# generic console so an operator with a USB-UART cable hooked to the
# debug pins reads the code without SSH. The list is widened with
# /dev/console (always present on Buildroot) and /dev/ttyS2 because
# the RV1106 evaluation pinout sometimes routes the debug UART to ttyS2
# rather than ttyS0 depending on boot loader configuration.
for tty in /dev/ttyS0 /dev/ttyS2 /dev/console; do
    if [ -w "$tty" ]; then
        printf '\n%s\n\n' "$BANNER" > "$tty" 2>/dev/null || true
    fi
done

# OLED probe. SSD1306 panels live at 0x3C on I2C bus 1 by Linux
# convention. i2cdetect's exit code is 0 even when nothing answers, so
# we check stdout for the "3c" cell. The probe is best-effort: if
# i2cdetect is missing (lighter rootfs builds), or if no panel
# responds, we silently skip and rely on the UART surface above.
if command -v i2cdetect >/dev/null 2>&1; then
    if i2cdetect -y 1 0x3c 0x3c 2>/dev/null | grep -qi '3c'; then
        # The agent owns the OLED framebuffer; we hand the banner to
        # the dedicated `display` subcommand, which is a forward-compat
        # hook (the binary may not implement it yet — `|| true` swallows
        # the not-found error so the rest of the boot continues).
        "$AGENT_BIN" display --message "ADOS PAIR: $CODE" 2>/dev/null || true
    fi
fi

# Sentinel last so a failure earlier leaves the surface re-runnable.
touch "$SENTINEL"
