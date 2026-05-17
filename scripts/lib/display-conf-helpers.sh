# shellcheck shell=bash
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Pure shell helpers for reading and preserving operator-mutable
# fields in /etc/ados/display.conf. Sourced by the LCD overlay
# installer and exercised standalone by
# tests/test_display_conf_idempotency.py so the test covers the same
# code path the installer runs.
#
# Functions defined here:
#
#   display_conf_preserve_rotation CONF_PATH DEFAULT_ROTATION
#     Echo the rotation value to use. If CONF_PATH exists and
#     contains a `rotation=` line with one of the four canonical
#     values (0/90/180/270), echo that. Else echo DEFAULT_ROTATION.
#     Malformed values (e.g. "abc") fall through to the default with
#     a warning on stderr.
#
# These helpers are pure: no global state mutation, no side effects
# beyond stdout/stderr. Safe to source multiple times.

display_conf_preserve_rotation() {
    local conf_path="${1:-}"
    local default_rotation="${2:-0}"
    local existing
    if [ -z "${conf_path}" ]; then
        echo "${default_rotation}"
        return 0
    fi
    if [ ! -f "${conf_path}" ]; then
        echo "${default_rotation}"
        return 0
    fi
    existing="$(awk -F= '/^rotation=/{print $2; exit}' "${conf_path}" 2>/dev/null | tr -d '[:space:]')"
    case "${existing}" in
        0|90|180|270)
            echo "${existing}"
            ;;
        "")
            echo "${default_rotation}"
            ;;
        *)
            echo "display_conf_preserve_rotation: ignoring unrecognised rotation '${existing}'; using default ${default_rotation}." >&2
            echo "${default_rotation}"
            ;;
    esac
}
