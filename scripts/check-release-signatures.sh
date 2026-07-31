#!/usr/bin/env bash
# Fail when a published binary in the prebuilt catalog has no signature.
#
# Signature verification is only worth enabling if the catalog is actually
# signed end to end. It was not: the onnx pair published unsigned for months
# because their runner had no signing tool and the job swallowed the failure,
# several kernel modules predate the signing step and survive because the
# manifest merge retains old rows, and one binary belongs to a crate that was
# deleted — so it sits unsigned in a trusted namespace with nothing to rebuild
# it from.
#
# Every one of those was invisible until someone listed the assets by hand.
# This makes the catalog's signing coverage a checked fact.
#
# Usage:
#   scripts/check-release-signatures.sh              # every prebuilt-* tag
#   scripts/check-release-signatures.sh prebuilt-tui # one tag
#
# Requires the GitHub CLI, authenticated.
set -uo pipefail

# Files that are not themselves artifacts and so are not expected to carry a
# signature of their own.
is_sidecar() {
  case "$1" in
    *.minisig | *.sha256) return 0 ;;
    *) return 1 ;;
  esac
}

# Assets knowingly published without a signature. An entry here is a decision,
# not an oversight: it must name the reason so the next reader does not have to
# re-derive it. Keep this empty if at all possible.
#
# Format: "<tag>/<asset>  # reason"
allowlisted() {
  case "$1/$2" in
    # (no entries)
    *) return 1 ;;
  esac
}

command -v gh >/dev/null || { echo "gh CLI not found; cannot check the catalog" >&2; exit 2; }

# `mapfile` is bash 4+; this has to run on a stock macOS bash 3.2 too, and a
# missing builtin previously made the script report "nothing to check" and exit
# 0 — the exact silent-success this check exists to prevent.
tags=("$@")
if [ ${#tags[@]} -eq 0 ]; then
  tag_list=$(gh release list --limit 100 --json tagName -q '.[].tagName' | grep '^prebuilt-') || {
    echo "could not list releases" >&2
    exit 2
  }
  while IFS= read -r t; do
    [ -n "$t" ] && tags+=("$t")
  done <<< "$tag_list"
fi

if [ ${#tags[@]} -eq 0 ]; then
  echo "no prebuilt-* releases found; refusing to report success on an empty catalog" >&2
  exit 2
fi

unsigned=0
checked=0
for tag in "${tags[@]}"; do
  assets=()
  if ! asset_list=$(gh release view "$tag" --json assets -q '.assets[].name' 2>&1 | sort); then
    echo "ERROR: could not read assets for ${tag}" >&2
    exit 2
  fi
  while IFS= read -r a; do
    [ -n "$a" ] && assets+=("$a")
  done <<< "$asset_list"
  if [ ${#assets[@]} -eq 0 ]; then
    echo "WARN  ${tag}: no assets (or not readable)" >&2
    continue
  fi
  for name in "${assets[@]}"; do
    is_sidecar "$name" && continue
    checked=$((checked + 1))
    if printf '%s\n' "${assets[@]}" | grep -qxF "${name}.minisig"; then
      continue
    fi
    if allowlisted "$tag" "$name"; then
      echo "ALLOW ${tag}/${name} (allowlisted)"
      continue
    fi
    echo "UNSIGNED ${tag}/${name}"
    unsigned=$((unsigned + 1))
  done
done

echo "checked ${checked} artifact(s) across ${#tags[@]} tag(s); ${unsigned} unsigned"
# Checking nothing is not the same as finding nothing wrong. A rate-limited or
# 5xx API run would otherwise print "0 unsigned" and exit 0 — the silent success
# this check exists to prevent, reported by the check itself.
if [ "$checked" -eq 0 ]; then
    echo "ERROR: inspected 0 artifacts; refusing to report success" >&2
    exit 2
fi
if [ "$unsigned" -gt 0 ]; then
  cat >&2 <<'EOF'

An artifact in the prebuilt catalog has no .minisig.

Either the publishing job did not sign it (check that the signing secret is
available on that job's runner and that the sign step is not degrading to a
warning), or it is a leftover from a job or crate that no longer exists, in
which case remove it from the release rather than leaving an unsigned binary
in a namespace installers trust.
EOF
  exit 1
fi
