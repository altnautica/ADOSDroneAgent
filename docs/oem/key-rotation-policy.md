# Lite agent release-key rotation policy

This document is for OEM operators who deploy ADOS lite agent binaries
to fleets. It explains how the project's release-artifact signing key
rotates, how to confirm which key signed any given release, and what to
do if a compromise is suspected.

For developer-focused key handling (CI provisioning, generating a
keypair, replacing the embedded value in `install-lite.sh`), see
`docs/oem/lite-agent-key-rotation.md`.

## Currently active signing key

**Fingerprint:** `FEAF0BB26CB7C87E`
**Active since:** 2026-05-06
**Issuing party:** Altnautica Pvt Ltd

This is the public key embedded in `scripts/install-lite.sh` and
referenced in CI workflows `.github/workflows/lite-agent-release.yml`
and `.github/workflows/luckfox-image-build.yml`. Operators who
download release artifacts from GitHub Releases verify against this
fingerprint.

To verify a release artifact:

    minisign -V -P "RWR+yLdssguv/iqfINd5cFsiC5+cUKLGvFggEfBS0O94KLWcjAvIczE7" -m <artifact>

The artifact's `.minisig` file accompanies it in the GitHub Release.

## Why we rotate

Long-lived signing keys accumulate risk:

- Staff turnover. Anyone with historic access to a passphrase or
  offline backup is, on paper, still capable of producing a valid
  signature.
- Hardware loss. A workstation, USB drive, or paper backup can be
  stolen, lost, or destroyed.
- Cryptographic margin. Ed25519 is sound today; rotation keeps the
  release pipeline practiced for the day a primitive needs to change.
- Provenance hygiene. A clean rotation cycle gives operators a clear
  way to reason about which artifacts are current and which are
  legacy.

Rotation is routine maintenance, not a rare emergency. The fleet is
expected to ride through it without incident.

## Rotation cadence

- **Routine.** The release-artifact signing key is rotated at least
  once every 12 months. Routine rotations are scheduled, announced in
  the release notes one minor release ahead of time, and produce a new
  `lite-vX.Y.Z` tag whose artifacts are signed with the new key.
- **On suspected compromise.** The key is rotated immediately, before
  the next public release. An emergency-rotation tag of the form
  `lite-vX.Y.Z-rotation` is cut, and the release notes call out the
  rotation in plain language at the top.
- **On staff transition.** Any departure of a person with historic
  access to the signing key or its passphrase triggers a rotation
  inside the same release cycle.

## How operators verify which key signed a release

Every release is signed by exactly one key. Operators can confirm
which key by either of two paths:

1. **Release notes.** The GitHub Release page for each `lite-vX.Y.Z`
   tag prints the public key fingerprint at the top of the notes. The
   fingerprint is a short, copy-pasteable string that uniquely
   identifies the key. Operators can compare it to the fingerprint
   they have on file.
2. **Embedded fingerprint in the installer.** The active key's
   fingerprint is embedded in `scripts/install-lite.sh` alongside the
   public key value. Running the installer with `--show-key` prints
   the embedded fingerprint to stdout without performing any install
   actions:

   ```sh
   ./install-lite.sh --show-key
   ```

   The printed fingerprint should match the release notes for the
   release the operator is about to install.

Hand-verification of a downloaded binary against a printed
fingerprint:

```sh
minisign -V \
    -P "$(cat lite-agent.pub)" \
    -m ados-agent-lite-<version>-<target>.tar.gz \
    -x ados-agent-lite-<version>-<target>.tar.gz.minisig
```

`Signature and comment signature verified` confirms a clean install
candidate. Anything else (parse error, signature mismatch) means the
file does not match the public key supplied — either tampered or
signed by a different key.

## Backwards compatibility window

After a rotation the previous key remains trusted for **90 days**.
During this window:

- Operators can re-install or downgrade to artifacts signed by the
  previous key without seeing a verification error.
- Mixed fleets (some boards on the old release, some on the new) keep
  installing cleanly from their respective releases.
- Existing operator boards do **not** re-verify a signature on every
  boot. The signature check happens at install time only, so already
  installed agents keep running indefinitely regardless of where the
  rotation calendar sits.

After the 90-day window the previous key is removed from
`install-lite.sh` and from the embedded trust set. Re-installing an
artifact signed by the retired key will fail closed with a clear
verification error. Operators who need to install a legacy artifact
after the window can fall back to the offline tarball install path
(`ADOS_LITE_LOCAL_TARBALL=...`) with `--skip-verify` after manually
checking the artifact against the retired key out-of-band.

## Where to report a suspected key compromise

Email **team@altnautica.com** with the subject line
`SECURITY — lite agent key compromise`.

Include in the body:

- The release tag(s) you suspect were affected.
- How you came across the suspected compromise (a leaked passphrase,
  an unexpected signature on an artifact, a binary with mismatched
  fingerprint, etc.).
- Whether any production fleet has already installed a suspect
  artifact.

Reports are triaged the same business day. If the report is
substantiated, the Altnautica team will:

1. Rotate to a fresh keypair within the next release cycle and tag
   `lite-vX.Y.Z-rotation`.
2. Pull any GitHub Release artifacts that are confirmed malicious.
3. Notify operators through the project's release channel and pin a
   notice to the repository README until the rotation is complete.
4. Document the incident in the public CHANGELOG.

Operators are encouraged to keep the fingerprint of every release
they deploy on file (in their own asset records) so that a key swap
is detectable from their side as well as ours.

## Related operator docs

- `docs/oem/luckfox-pico-zero-flash-guide.md` — how operators flash
  and bring up a board, including the `minisign` verification step.
- `docs/oem/lite-agent-key-rotation.md` — developer-side runbook for
  generating, provisioning, and rotating the keypair.
- `docs/oem/lite-deployment.md` — broader deployment topics.
