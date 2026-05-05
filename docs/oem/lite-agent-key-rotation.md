# Lite agent release-key rotation

Operators integrating the lite agent ship signed binaries through the
project's GitHub Releases. This runbook covers generating the Ed25519
signing keypair, rotating keys, and recovering when a key is suspected
of compromise.

## Why a separate key

The lite agent ships as a prebuilt binary. Each release artifact carries
a `.minisig` signature alongside its `.sha256`. The installer
(`scripts/install-lite.sh`) refuses to install an unsigned or
mis-signed binary unless the operator explicitly sets
`ADOS_LITE_ALLOW_UNSIGNED=1`. The public verifying key is embedded in
`install-lite.sh` so a network-positioned attacker cannot substitute
their own key by altering an environment variable on the target SBC.

This release-artifact key is **distinct from** the plugin-system signing
key. Plugin packages (`.adosplug` archives) are verified by the agent's
plugin host using bare `cryptography.Ed25519` with PEM keys; release
artifacts are verified by the host shell using the `minisign` binary.
Different verifiers, different storage, different rotation cycle.

## Generate the keypair

Run on a workstation that is NOT a CI runner. The private key never
leaves this workstation.

```sh
minisign -G -p lite-agent.pub -s lite-agent.key
```

Use a long passphrase. Store both files in offline cold storage (e.g.
encrypted USB drive, paper backup of the seed in a safe). Lose the
passphrase and the key is gone — no recovery.

## Provision the public key

Open `scripts/install-lite.sh` and replace the placeholder line:

```sh
MINISIGN_PUBLIC_KEY="RWQz4jK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8YjK8"
```

with the contents of `lite-agent.pub` (the part after `untrusted comment:`,
no whitespace). Commit and push. The next CI release will sign artifacts
with the new key, and operators running `install-lite.sh` will verify
against it.

## Provision the signing key in CI

The CI workflow at `.github/workflows/lite-agent-release.yml` looks for
two GitHub Actions secrets:

- `LITE_AGENT_MINISIGN_KEY` — the contents of `lite-agent.key`
- `LITE_AGENT_MINISIGN_PASSPHRASE` — the passphrase used at key generation

Set both via `Settings → Secrets and variables → Actions`. The next push
to `main` (or tag) will produce signed artifacts.

## Rotate the key

Routine rotation (annually, or on staff transition):

1. Generate a fresh keypair with `minisign -G`.
2. Replace the public key in `install-lite.sh`.
3. Replace the GitHub Actions secrets with the new private key + passphrase.
4. Tag a new release. The new artifacts verify against the new public key.
5. Old installs continue to work — they were already verified at install
   time. New installs require the new key.

Existing operator boards do NOT re-verify a signature on every boot;
the verification happens at install time only. Rotation does not break
running fleets.

## Compromise recovery

If the private key (or passphrase) is suspected of compromise:

1. Immediately rotate to a fresh keypair (steps above).
2. Tag an emergency release `lite-vX.Y.Z-rotation` with the new key.
3. Notify operators that any binary signed with the old key should not
   be re-installed.
4. Delete the GitHub Actions secrets containing the old key.
5. If a malicious binary is suspected of being signed and published,
   delete the relevant GitHub Release artifacts and document the
   incident in `CHANGELOG.md`.

## Verifying a binary by hand

```sh
minisign -V \
    -P "RWQz<paste public key here>" \
    -m ados-agent-lite-<version>-<target>.tar.gz \
    -x ados-agent-lite-<version>-<target>.tar.gz.minisig
```

`Signature and comment signature verified` confirms a clean install
candidate. Anything else (parse error, signature mismatch) means the
file has been tampered with or the wrong public key was supplied.

## Offline / air-gapped environments

The lite installer accepts a pre-downloaded artifact bundle:

```sh
sudo ADOS_LITE_LOCAL_TARBALL=/path/to/ados-agent-lite-<version>-<target>.tar.gz \
     ADOS_LITE_LOCAL_SIG=/path/to/...minisig \
     ADOS_LITE_LOCAL_SHA=/path/to/...sha256 \
     ./install-lite.sh
```

Set `ADOS_LITE_ALLOW_UNSIGNED=1` only on operator workstations that
have already verified the bundle out-of-band; never set it on a fleet
SBC that downloads from a remote host.
