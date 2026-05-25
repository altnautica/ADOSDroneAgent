# shellcheck shell=bash
# =============================================================================
# net.sh — network fetch helpers for the full agent install pipeline.
#
# Sourceable library. Side-effect-free at source time (defines functions
# only). Used by the install.d modules, the prebuilt kernel-module driver
# path, and the stable-channel installer to fetch release artifacts with
# uniform retry/backoff plus a fast offline probe.
#
# Functions:
#   ados_fetch URL [OUTFILE] [TIMEOUT_SECS]
#       Fetch URL with curl (preferred) or wget. Writes to OUTFILE when
#       given, otherwise to stdout. Retries 3x with backoff. Returns
#       non-zero on failure and never exits, so a caller decides whether a
#       miss is fatal (the prebuilt path treats it as "fall back to DKMS").
#   ados_reachable URL [TIMEOUT_SECS]
#       Fast reachability probe. Returns 0 if URL responds. Timeout defaults
#       to ADOS_REACHABLE_TIMEOUT (or 5s when unset); an explicit arg wins.
#       Used as an offline guard before optional network steps so an
#       off-internet board fails fast instead of hanging. Slow links can raise
#       the default by exporting ADOS_REACHABLE_TIMEOUT.
# =============================================================================

# Define minimal loggers only when the caller has not already provided them.
# install.d/lib.sh ships richer colored versions; standalone callers (the
# driver path) get these plain fallbacks.
command -v info >/dev/null 2>&1 || info() { printf '[INFO]  %s\n' "$*" >&2; }
command -v warn >/dev/null 2>&1 || warn() { printf '[WARN]  %s\n' "$*" >&2; }

ados_fetch() {
    local url="$1" outfile="${2:-}" timeout_secs="${3:-30}"
    if command -v curl >/dev/null 2>&1; then
        if [ -n "${outfile}" ]; then
            curl -fsSL --connect-timeout 10 --max-time "${timeout_secs}" \
                --retry 3 --retry-delay 2 -o "${outfile}" "${url}"
        else
            curl -fsSL --connect-timeout 10 --max-time "${timeout_secs}" \
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

ados_reachable() {
    local url="$1" timeout_secs="${2:-${ADOS_REACHABLE_TIMEOUT:-5}}"
    if command -v curl >/dev/null 2>&1; then
        curl -fsS --connect-timeout "${timeout_secs}" --max-time "${timeout_secs}" \
            -o /dev/null "${url}" >/dev/null 2>&1
    elif command -v wget >/dev/null 2>&1; then
        wget -q -T "${timeout_secs}" --tries=1 -O /dev/null "${url}" >/dev/null 2>&1
    else
        return 1
    fi
}
