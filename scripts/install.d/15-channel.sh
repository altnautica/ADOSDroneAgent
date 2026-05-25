# shellcheck shell=bash
# =============================================================================
# 15-channel.sh — release-channel selection + signed prebuilt install path.
#
# Two channels:
#
#   edge   (default) — clone the repo and pip-install from source on the
#                      device, exactly as the installer has always done. No
#                      signed artifact required; the source tree itself is the
#                      payload. This is what the canonical
#                      `curl | sudo bash -s -- --pair CODE` one-liner uses.
#
#   stable           — install a prebuilt, signed wheel plus a signed
#                      deploy-bundle, pinned to a release tag. No on-device
#                      source build. SHA256 is mandatory; an Ed25519
#                      (minisign) signature is mandatory too — stable REFUSES
#                      an unsigned or tampered artifact (verify.sh enforces
#                      this). The wheel installs the Python package; the
#                      deploy-bundle lays down the install assets that live
#                      outside the wheel (scripts/, data/, vendored radio
#                      source) so the rest of main_install_flow finds its
#                      source files in the same place a git clone would have
#                      put them.
#
# The deploy-bundle unpacks into the SAME ${FRESH_REPO_DIR}/repo layout the
# edge clone produces, so install_systemd_service, persist_repo_artifacts,
# install_wfb_ng_from_vendor, and install_display_driver all resolve their
# sources without knowing which channel selected them. The detach / resume /
# completeness / checkpoint / health-gate machinery is channel-agnostic and
# runs identically on both.
#
# Sourced after 14-orchestration.sh so ados_fetch / ados_verify_artifact /
# the net.sh + verify.sh helpers are already in scope (14-orchestration
# sources net.sh; this module sources verify.sh the same way).
# =============================================================================

# ─── Vendored Ed25519 public key (release-artifact verification) ─────────────
#
# The CI release pipeline substitutes this string with the real minisign
# public key when a `v*` tag is cut (the same sed-at-tag-time mechanism the
# lightweight installer uses for its own vendored key). Until a real key is
# embedded the value stays the clearly-marked placeholder below; on the
# stable channel an unsubstituted placeholder is treated as "no key", which
# verify.sh turns into a hard refusal — stable never installs unverifiable.
#
# We deliberately do NOT accept an environment override for the public key.
# Letting the operator's environment supply the trust anchor would let an
# attacker who controls that environment swap in their own signing key and
# pass verification on a malicious artifact. Rotation is a code change + a
# git push, not a runtime knob.
ADOS_STABLE_PUBKEY="RWQz4jK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8"
ADOS_STABLE_PUBKEY_PLACEHOLDER="RWQz4jK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8"
# Short fingerprint of the active public key, replaced alongside the key when
# a real key is committed. Operators read it via `install.sh --show-key` and
# compare to the fingerprint printed in the release notes.
ADOS_STABLE_PUBKEY_FINGERPRINT="PLACEHOLDER-NOT-YET-PROVISIONED"
export ADOS_STABLE_PUBKEY ADOS_STABLE_PUBKEY_PLACEHOLDER ADOS_STABLE_PUBKEY_FINGERPRINT

# show_stable_key — print the vendored public key + fingerprint, and whether
# it is still the placeholder. Read-only; backs the dispatcher's --show-key.
show_stable_key() {
    printf 'stable channel public key:  %s\n' "${ADOS_STABLE_PUBKEY}"
    printf 'fingerprint:                %s\n' "${ADOS_STABLE_PUBKEY_FINGERPRINT}"
    if [ "${ADOS_STABLE_PUBKEY}" = "${ADOS_STABLE_PUBKEY_PLACEHOLDER}" ]; then
        printf 'status:                     PLACEHOLDER (stable channel will refuse to install)\n'
    else
        printf 'status:                     active\n'
    fi
}

# GitHub coordinates for release-asset fetches. REPO_URL (lib.sh) is the
# clone URL; these are the API + download host pair the stable channel hits.
ADOS_GH_OWNER="${ADOS_GH_OWNER:-altnautica}"
ADOS_GH_REPO="${ADOS_GH_REPO:-ADOSDroneAgent}"
export ADOS_GH_OWNER ADOS_GH_REPO

# Source verify.sh the same way 14-orchestration sources net.sh, so the
# signed-artifact helpers are available even when this module is sourced in
# isolation (the bats orchestration harness does exactly that).
if ! declare -F ados_verify_artifact >/dev/null 2>&1; then
    _CHAN_LIB_DIR=""
    if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
        _CHAN_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd)" || _CHAN_LIB_DIR=""
    fi
    if [ -n "${_CHAN_LIB_DIR}" ] && [ -f "${_CHAN_LIB_DIR}/verify.sh" ]; then
        # shellcheck source=scripts/lib/verify.sh disable=SC1091
        . "${_CHAN_LIB_DIR}/verify.sh"
    elif [ -f /opt/ados/source/scripts/lib/verify.sh ]; then
        # shellcheck disable=SC1091
        . /opt/ados/source/scripts/lib/verify.sh
    fi
    unset _CHAN_LIB_DIR
fi

# ─── Channel resolution ──────────────────────────────────────────────────────
#
# resolve_channel — echo the active channel. Priority: ADOS_CHANNEL env (set
# by the --channel flag in the dispatcher, or exported directly) > default
# edge. Any value other than "stable" normalises to "edge" so a typo never
# silently turns on the signed path or off the working default.
resolve_channel() {
    case "${ADOS_CHANNEL:-edge}" in
        stable) printf 'stable\n' ;;
        *)      printf 'edge\n' ;;
    esac
}

# is_stable_channel — true when the active channel is stable.
is_stable_channel() {
    [ "$(resolve_channel)" = "stable" ]
}

# stable_pubkey_or_empty — echo the vendored public key, or empty when it is
# still the placeholder. verify.sh treats an empty key on the stable channel
# as a hard refusal, which is exactly what we want before CI embeds a real
# key: an unsubstituted build cannot install on stable.
stable_pubkey_or_empty() {
    if [ "${ADOS_STABLE_PUBKEY}" = "${ADOS_STABLE_PUBKEY_PLACEHOLDER}" ]; then
        printf '%s\n' ""
    else
        printf '%s\n' "${ADOS_STABLE_PUBKEY}"
    fi
}

# ─── Release-tag resolution ──────────────────────────────────────────────────
#
# resolve_stable_tag — echo the release tag to pin the stable install to.
# Priority: ADOS_VERSION (set by --version X.Y.Z) > latest v* release via the
# GitHub releases API. A bare X.Y.Z is normalised to vX.Y.Z; a value already
# prefixed with v is taken verbatim. Returns non-zero (and echoes nothing)
# when no tag can be resolved so the caller can hard-fail on stable.
resolve_stable_tag() {
    local v="${ADOS_VERSION:-}"
    if [ -n "${v}" ]; then
        case "${v}" in
            v*) printf '%s\n' "${v}" ;;
            *)  printf 'v%s\n' "${v}" ;;
        esac
        return 0
    fi

    # No pin: ask the GitHub releases API for the newest v* tag. The full
    # agent releases use the `vMAJOR.MINOR.PATCH` convention; the lightweight
    # agent uses `lite-v*`, so the value-class `"v[0-9]` excludes those
    # naturally (a lite tag serialises as `"tag_name":"lite-v..."`, which
    # does not match `"v[0-9]`). grep -oE pulls every tag_name value as its
    # own token regardless of whether the API returns pretty-printed or
    # single-line JSON; the API lists releases newest-first so head -n1 is
    # the latest. The trailing grep peels the bare tag out of the matched
    # key/value token.
    local api tag
    api="https://api.github.com/repos/${ADOS_GH_OWNER}/${ADOS_GH_REPO}/releases"
    tag="$(ados_fetch "${api}" 2>/dev/null \
        | grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"v[0-9][^"]*"' \
        | head -n1 \
        | grep -oE 'v[0-9][^"]*')"
    if [ -z "${tag}" ]; then
        return 1
    fi
    printf '%s\n' "${tag}"
}

# ─── Asset naming ────────────────────────────────────────────────────────────
#
# The release pipeline publishes, per tag:
#   ados_drone_agent-<X.Y.Z>-py3-none-any.whl          (+ .sha256 + .minisig)
#   ados-drone-agent-deploy-<X.Y.Z>.tar.gz             (+ .sha256 + .minisig)
#   SHA256SUMS                                          (all assets)
#
# stable_version_from_tag — strip the leading v so the asset names (which the
# wheel build derives from the package version) match the tag.
stable_version_from_tag() {
    printf '%s\n' "${1#v}"
}

# stable_wheel_name VERSION — the wheel filename for VERSION. setuptools
# normalises the project name "ados-drone-agent" to "ados_drone_agent" in the
# wheel, and the package is pure-Python so the tag is always py3-none-any.
stable_wheel_name() {
    printf 'ados_drone_agent-%s-py3-none-any.whl\n' "$1"
}

# stable_bundle_name VERSION — the deploy-bundle tarball filename for VERSION.
stable_bundle_name() {
    printf 'ados-drone-agent-deploy-%s.tar.gz\n' "$1"
}

# stable_asset_base TAG — the releases/download/<tag> base URL.
stable_asset_base() {
    printf 'https://github.com/%s/%s/releases/download/%s\n' \
        "${ADOS_GH_OWNER}" "${ADOS_GH_REPO}" "$1"
}

# ─── Fetch + verify ──────────────────────────────────────────────────────────
#
# fetch_and_verify_stable_asset BASEURL NAME DESTDIR PUBKEY — download NAME
# plus its .sha256 and .minisig into DESTDIR, then run the channel verifier.
# On the stable channel ados_verify_artifact refuses anything that is not
# SHA256-clean AND signed by PUBKEY (a missing key, a missing signature, or a
# tampered signature are all fatal). Returns non-zero on any failure so the
# caller hard-fails the install — stable is supposed to break loudly on a bad
# artifact; that is the whole point of choosing it.
fetch_and_verify_stable_asset() {
    local baseurl="$1" name="$2" destdir="$3" pubkey="$4"
    local artifact="${destdir}/${name}"

    info "Fetching ${name}..."
    if ! ados_fetch "${baseurl}/${name}" "${artifact}" 120; then
        error "stable channel: failed to download ${name} from ${baseurl}"
        return 1
    fi
    # The checksum and signature sidecars are mandatory on stable. A miss
    # here is a hard failure rather than a fall-through to SHA256-only.
    if ! ados_fetch "${baseurl}/${name}.sha256" "${artifact}.sha256" 30; then
        error "stable channel: missing ${name}.sha256 sidecar"
        return 1
    fi
    if ! ados_fetch "${baseurl}/${name}.minisig" "${artifact}.minisig" 30; then
        error "stable channel: missing ${name}.minisig sidecar"
        return 1
    fi

    if ! ados_verify_artifact "${artifact}" "${pubkey}" stable; then
        error "stable channel: verification failed for ${name}"
        return 1
    fi
    info "Verified ${name} (SHA256 + Ed25519 signature)."
    return 0
}

# fetch_and_verify_stable_assets TAG DESTDIR — download + verify the wheel and
# the deploy-bundle for TAG into DESTDIR. Echoes nothing; sets the globals
# STABLE_WHEEL_PATH and STABLE_BUNDLE_PATH on success. Hard-fails (non-zero)
# on any download or verification miss.
STABLE_WHEEL_PATH=""
STABLE_BUNDLE_PATH=""
fetch_and_verify_stable_assets() {
    local tag="$1" destdir="$2"
    local version base pubkey wheel bundle
    version="$(stable_version_from_tag "${tag}")"
    base="$(stable_asset_base "${tag}")"
    pubkey="$(stable_pubkey_or_empty)"

    # Refuse early when there is no trust anchor. verify.sh would refuse too,
    # but a clear message up front beats a per-artifact one.
    if [ -z "${pubkey}" ]; then
        error "stable channel selected but no signing key is embedded in this installer."
        error "This build still carries the placeholder key; install from a signed release,"
        error "or use the edge channel (the default) which installs from source."
        return 1
    fi

    wheel="$(stable_wheel_name "${version}")"
    bundle="$(stable_bundle_name "${version}")"

    install -d -m 0755 "${destdir}" 2>/dev/null || true

    if ! fetch_and_verify_stable_asset "${base}" "${wheel}" "${destdir}" "${pubkey}"; then
        return 1
    fi
    if ! fetch_and_verify_stable_asset "${base}" "${bundle}" "${destdir}" "${pubkey}"; then
        return 1
    fi

    STABLE_WHEEL_PATH="${destdir}/${wheel}"
    STABLE_BUNDLE_PATH="${destdir}/${bundle}"
    export STABLE_WHEEL_PATH STABLE_BUNDLE_PATH
    return 0
}

# ─── Deploy-bundle unpack ────────────────────────────────────────────────────
#
# unpack_deploy_bundle BUNDLE DESTROOT — extract the verified deploy-bundle so
# its contents land at ${DESTROOT}/repo/, reproducing the exact tree layout
# the edge git-clone produces (repo/scripts, repo/data, repo/vendor, ...).
# main_install_flow then points SYSTEMD_SRC_DIR + FRESH_REPO_DIR at DESTROOT
# and every downstream install helper resolves its source unchanged.
#
# The bundle's top-level directory is "repo" (the release pipeline tars it
# that way), so a plain extract into DESTROOT yields DESTROOT/repo/...
unpack_deploy_bundle() {
    local bundle="$1" destroot="$2"
    install -d -m 0755 "${destroot}" 2>/dev/null || true

    # Validate the archive layout BEFORE extracting anything. The bundle is
    # signature-verified upstream, but defence-in-depth still rejects a
    # malformed or hostile tarball so a single bad entry cannot escape the
    # destination directory. tar -tz only lists; nothing is written to disk.
    local listing
    if ! listing="$(tar -tzf "${bundle}" 2>/dev/null)"; then
        error "stable channel: cannot list deploy-bundle ${bundle}"
        return 1
    fi
    if [ -z "${listing}" ]; then
        error "stable channel: deploy-bundle ${bundle} is empty"
        return 1
    fi
    # Reject path traversal (any entry with a .. component) or absolute paths
    # (leading /) anywhere in the archive — these would write outside destroot.
    local entry
    while IFS= read -r entry; do
        [ -z "${entry}" ] && continue
        case "${entry}" in
            /*)
                error "stable channel: deploy-bundle rejects absolute path entry: ${entry}"
                return 1
                ;;
            ..|../*|*/../*|*/..)
                error "stable channel: deploy-bundle rejects path-traversal entry: ${entry}"
                return 1
                ;;
        esac
    done <<EOF
${listing}
EOF
    # The release pipeline roots every entry under repo/; refuse a bundle whose
    # archive root is anything else so the post-extract layout check is meaningful.
    local first_entry
    first_entry="$(printf '%s\n' "${listing}" | head -n 1)"
    case "${first_entry}" in
        repo/|repo) : ;;
        *)
            error "stable channel: deploy-bundle root is not repo/ (got: ${first_entry})"
            return 1
            ;;
    esac

    info "Unpacking verified deploy-bundle into ${destroot}..."
    if ! tar -xzf "${bundle}" -C "${destroot}"; then
        error "stable channel: failed to unpack deploy-bundle ${bundle}"
        return 1
    fi
    if [ ! -d "${destroot}/repo/data/systemd" ]; then
        error "stable channel: deploy-bundle missing expected repo/data/systemd tree"
        return 1
    fi
    return 0
}

# ─── Wheel install ───────────────────────────────────────────────────────────
#
# install_agent_from_wheel WHEEL [EXTRA] — pip-install the verified wheel into
# the venv (no source build). EXTRA, when given, installs an optional-deps
# group from the same wheel (e.g. ground-station). Returns pip's exit status.
install_agent_from_wheel() {
    local wheel="$1" extra="${2:-}"
    "${VENV_DIR}/bin/pip" install --upgrade pip --quiet
    if [ -n "${extra}" ]; then
        info "Installing prebuilt wheel with [${extra}] extras..."
        "${VENV_DIR}/bin/pip" install --upgrade "${wheel}[${extra}]" --quiet
    else
        info "Installing prebuilt wheel..."
        "${VENV_DIR}/bin/pip" install --upgrade "${wheel}" --quiet
    fi
}

# ─── Channel summary line ────────────────────────────────────────────────────
#
# print_channel_banner — one info line stating which channel + (on stable) the
# pinned tag. Cheap and read-only; called once near the top of the flow so the
# journal records the selected channel for after-the-fact debugging.
print_channel_banner() {
    if is_stable_channel; then
        info "Release channel: stable (signed prebuilt wheel + deploy-bundle, pinned to a tag)."
    else
        info "Release channel: edge (clone + build from source; default)."
    fi
}
