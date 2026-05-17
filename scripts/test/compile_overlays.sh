#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Compile every .dts under data/overlays/ with cpp + dtc to catch
# syntax errors, missing includes, and stale dt-binding references
# before they ship to a real SBC. Mirrors the cpp + dtc pipeline
# used by scripts/drivers/install-display-overlay.sh at install time
# (see install-display-overlay.sh:368-381) so a green CI run implies
# a green install-time compile.
#
# Usage:
#   scripts/test/compile_overlays.sh [overlay-root] [out-dir]
#
# overlay-root defaults to data/overlays/ relative to the repo root.
# out-dir defaults to a tmpdir that's cleaned on exit. The compiled
# .dtbo files are not installed -- this is a lint, not a deploy.
#
# Requires dt-bindings headers under /lib/modules/$(uname -r)/build/
# or /usr/include. CI installs the linux-headers package; locally on
# a Mac the headers are absent and this script no-ops with a warn so
# `make lint` stays green during development.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OVERLAY_ROOT="${1:-${REPO_ROOT}/data/overlays}"
OUT_DIR="${2:-}"
CLEANUP_OUT=0
if [ -z "${OUT_DIR}" ]; then
    OUT_DIR="$(mktemp -d)"
    CLEANUP_OUT=1
fi
trap '[ "${CLEANUP_OUT}" = "1" ] && rm -rf "${OUT_DIR}"' EXIT

# Detect required tools.
need_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "compile_overlays: missing tool: $1" >&2
        return 1
    fi
}

if ! need_tool cpp || ! need_tool dtc; then
    echo "compile_overlays: required toolchain missing; install dtc + a C preprocessor." >&2
    exit 2
fi

# Resolve dt-binding header roots. The shared dtsi includes
# <dt-bindings/gpio/gpio.h>, <dt-bindings/pinctrl/rockchip.h>, and
# <dt-bindings/interrupt-controller/irq.h>. Those ship with the
# kernel sources; if they're not on disk the compile cannot run.
INC_ARGS=()
KBUILD=""
if [ -d "/lib/modules/$(uname -r 2>/dev/null)/build/include" ]; then
    KBUILD="/lib/modules/$(uname -r)/build/include"
    INC_ARGS+=( -I "${KBUILD}" )
fi
if [ -d "/usr/src/linux-headers-$(uname -r 2>/dev/null)/include" ]; then
    INC_ARGS+=( -I "/usr/src/linux-headers-$(uname -r)/include" )
fi
INC_ARGS+=( -I "/usr/include" )

# Sanity check: at least one binding header reachable.
if ! { printf '#include <dt-bindings/gpio/gpio.h>\n' | cpp -E -x assembler-with-cpp -undef -nostdinc "${INC_ARGS[@]}" - >/dev/null 2>&1; }; then
    echo "compile_overlays: dt-bindings headers unreachable. On Linux install linux-headers-$(uname -r 2>/dev/null) (Debian/Ubuntu) or kernel-devel (RHEL/Fedora). On macOS this lint is a no-op." >&2
    if [ "$(uname -s)" = "Darwin" ]; then
        echo "compile_overlays: skipping on macOS (dt-bindings not available)." >&2
        exit 0
    fi
    exit 2
fi

# Walk every .dts (top-level overlays). Skip the shared dtsi -- it's
# included by per-board overlays and isn't a standalone compile unit.
fail_count=0
while IFS= read -r -d '' dts; do
    name="$(basename "${dts}" .dts)"
    rel="${dts#"${REPO_ROOT}"/}"
    pre="${OUT_DIR}/${name}.cpp.dts"
    dtbo="${OUT_DIR}/${name}.dtbo"
    if ! cpp -E -x assembler-with-cpp -undef -nostdinc \
            -I "$(dirname "${dts}")" \
            "${INC_ARGS[@]}" \
            "${dts}" -o "${pre}" 2>"${OUT_DIR}/${name}.cpp.log"; then
        echo "compile_overlays: FAIL cpp ${rel}" >&2
        cat "${OUT_DIR}/${name}.cpp.log" >&2
        fail_count=$((fail_count + 1))
        continue
    fi
    if ! dtc -@ -I dts -O dtb -o "${dtbo}" "${pre}" 2>"${OUT_DIR}/${name}.dtc.log"; then
        echo "compile_overlays: FAIL dtc ${rel}" >&2
        cat "${OUT_DIR}/${name}.dtc.log" >&2
        fail_count=$((fail_count + 1))
        continue
    fi
    echo "compile_overlays: OK ${rel}"
done < <(find "${OVERLAY_ROOT}" -type f -name '*.dts' -print0)

if [ "${fail_count}" -gt 0 ]; then
    echo "" >&2
    echo "compile_overlays: FAILED with ${fail_count} overlay(s)." >&2
    exit 1
fi

echo "compile_overlays: OK (all overlays compile)."
