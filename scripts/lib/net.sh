# shellcheck shell=bash
# =============================================================================
# net.sh — network fetch helpers for the driver build path.
#
# Sourceable library. Side-effect-free at source time (defines functions
# only). Used by the prebuilt kernel-module driver path to fetch release
# artifacts with uniform retry/backoff plus a fast offline probe.
#
# Functions:
#   ados_fetch URL [OUTFILE] [TIMEOUT_SECS]
#       Fetch URL with curl (preferred) or wget. Writes to OUTFILE when
#       given, otherwise to stdout. Retries 3x with backoff. Returns
#       non-zero on failure and never exits, so a caller decides whether a
#       miss is fatal (the prebuilt path treats it as "fall back to DKMS").
# =============================================================================

# Define minimal loggers only when the caller has not already provided them.
# A caller that sources a richer logger keeps it; standalone callers (the
# driver path) get these plain fallbacks.
command -v info >/dev/null 2>&1 || info() { printf '[INFO]  %s\n' "$*" >&2; }
command -v warn >/dev/null 2>&1 || warn() { printf '[WARN]  %s\n' "$*" >&2; }

ados_fetch() {
    local url="$1" outfile="${2:-}" timeout_secs="${3:-30}"
    # TIMEOUT_SECS bounds a STALL (under 1 KB/s for that long), not the whole
    # transfer: a flat ceiling kills a slow but healthy download of a multi-MB
    # module. A file download resumes across curl's retries.
    if command -v curl >/dev/null 2>&1; then
        if [ -n "${outfile}" ]; then
            curl -fsSL --connect-timeout 10 --speed-time "${timeout_secs}" --speed-limit 1024 \
                --retry 3 --retry-delay 2 --continue-at - -o "${outfile}" "${url}"
        else
            curl -fsSL --connect-timeout 10 --speed-time "${timeout_secs}" --speed-limit 1024 \
                --retry 3 --retry-delay 2 "${url}"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ -n "${outfile}" ]; then
            wget -q -T "${timeout_secs}" --tries=3 -O "${outfile}" "${url}"
        else
            wget -q -T "${timeout_secs}" --tries=3 -O - "${url}"
        fi
    else
        warn "neither curl nor wget available; cannot fetch ${url}"
        return 1
    fi
}
