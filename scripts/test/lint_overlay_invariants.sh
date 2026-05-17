#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Lint per-board device-tree overlays under data/overlays/ to enforce
# the chip-vs-SoC separation contract documented in
# upstream/waveshare35-lcd.dtsi:
#
#   * The shared dtsi is the single source of truth for chip-level
#     invariants of the touch + display silicon (Waveshare 3.5"
#     ILI9486 + ADS7846 today; more panels later).
#
#   * Per-board overlays MUST NOT redeclare those invariants, because
#     they were silently drifting across boards (Cubie A7Z had
#     ti,invert-y=0 by omission while Rock 5C inherited ti,invert-y=1;
#     Rock 5C shipped pendown-gpio=ACTIVE_HIGH when every Pi-canonical
#     overlay uses ACTIVE_LOW; etc.).
#
#   * Per-board overlays MAY override SoC-specific properties:
#     interrupt-parent, interrupts, pendown-gpio, pinctrl-names,
#     pinctrl-0, vcc-supply, reset-gpios, dc-gpios, cs-gpios, num-cs,
#     plus pinctrl mux groups under &pio / &pinctrl / &r_pio.
#
# The lint reads each per-board overlay (a file that #includes the
# shared dtsi), strips comments and string-literals, then greps for
# banned-chip-field tokens. Exits 0 on a clean tree, non-zero with a
# pointer to the shared dtsi on the first violation.
#
# Usage:
#   scripts/test/lint_overlay_invariants.sh [overlay-root]
#
# overlay-root defaults to data/overlays/ relative to the repo root.
# Runs in <1s on the current tree.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OVERLAY_ROOT="${1:-${REPO_ROOT}/data/overlays}"

if [ ! -d "${OVERLAY_ROOT}" ]; then
    echo "lint_overlay_invariants: overlay root not found: ${OVERLAY_ROOT}" >&2
    exit 2
fi

SHARED_DTSI_NAME="waveshare35-lcd.dtsi"
SHARED_DTSI="${OVERLAY_ROOT}/upstream/${SHARED_DTSI_NAME}"

# Chip-level invariants that live only in the shared dtsi. Anchored to
# property-line context (`<field-name> =`) so we don't false-match
# substrings inside comments or unrelated identifiers.
BANNED_CHIP_FIELDS=(
    'ti,x-plate-ohms'
    'ti,pressure-max'
    'ti,swap-xy'
    'ti,invert-y'
    'touchscreen-max-pressure'
    'spi-max-frequency'
)

# Per-board fields that must NOT appear in the shared dtsi (the
# regulator phandle name, GPIO bank reference, IRQ trigger cell, and
# pinctrl group all carry SoC-specific content).
BANNED_SOC_FIELDS_IN_SHARED=(
    'interrupt-parent'
    'interrupts'
    'pendown-gpio'
    'pinctrl-0'
    'pinctrl-names'
    'vcc-supply'
    'reset-gpios'
    'dc-gpios'
    'cs-gpios'
)

# Strip C-style /* ... */ blocks, // line comments, and string
# literals so banned tokens inside comment blocks (e.g. the contract
# header in the shared dtsi) and inside `description = "..."` lines
# never trip the grep.
strip_comments_and_strings() {
    sed -E -e '/\/\*/,/\*\//d' -e 's|//.*$||' -e 's|"[^"]*"||g' "$1"
}

# Check one .dts file for redeclared chip-level fields. Echoes one
# error line per violation. Returns count via stdout.
check_per_board_overlay() {
    local dts="$1"
    local stripped count field
    stripped="$(mktemp)"
    trap 'rm -f "${stripped}"' RETURN
    strip_comments_and_strings "${dts}" > "${stripped}"
    count=0
    for field in "${BANNED_CHIP_FIELDS[@]}"; do
        if grep -qE "(^|[[:space:]])${field}[[:space:]]*=" "${stripped}"; then
            echo "lint_overlay_invariants: per-board overlay redeclares chip-level field '${field}': ${dts#"${REPO_ROOT}"/}" >&2
            echo "  -> Remove from the per-board overlay. ${field} lives in data/overlays/upstream/${SHARED_DTSI_NAME}." >&2
            count=$((count + 1))
        fi
    done
    echo "${count}"
}

violations=0

if [ ! -f "${SHARED_DTSI}" ]; then
    echo "lint_overlay_invariants: shared dtsi missing: ${SHARED_DTSI}" >&2
    exit 2
fi

# Check the shared dtsi does not declare any SoC-specific fields.
shared_stripped="$(mktemp)"
strip_comments_and_strings "${SHARED_DTSI}" > "${shared_stripped}"
for field in "${BANNED_SOC_FIELDS_IN_SHARED[@]}"; do
    if grep -qE "(^|[[:space:]])${field}[[:space:]]*=" "${shared_stripped}"; then
        echo "lint_overlay_invariants: shared dtsi declares SoC-specific field '${field}' (must live in per-board overlay): ${SHARED_DTSI#"${REPO_ROOT}"/}" >&2
        violations=$((violations + 1))
    fi
done
rm -f "${shared_stripped}"

# Walk every .dts. A standalone overlay (one that does not #include
# the shared dtsi) gets a WARN; that lets the operator add a future
# panel without bypassing the contract on the assumption it'll be
# refactored to inherit. CI fails only on chip-field redeclaration
# inside an overlay that DID inherit.
while IFS= read -r -d '' dts; do
    if grep -qE "${SHARED_DTSI_NAME}" "${dts}"; then
        per_board_count="$(check_per_board_overlay "${dts}")"
        violations=$((violations + per_board_count))
    else
        echo "lint_overlay_invariants: WARN: ${dts#"${REPO_ROOT}"/} does not #include the shared dtsi; skipping chip-field check." >&2
    fi
done < <(find "${OVERLAY_ROOT}" -type f -name '*.dts' -print0)

if [ "${violations}" -gt 0 ]; then
    echo "" >&2
    echo "lint_overlay_invariants: FAILED with ${violations} violation(s)." >&2
    echo "See data/overlays/README.md for the per-board overlay contract." >&2
    exit 1
fi

echo "lint_overlay_invariants: OK (shared dtsi clean, per-board overlays clean)."
