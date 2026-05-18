# Plugin signing — key generation and rotation

Operator-facing runbook for generating, storing, and rotating the
Ed25519 keypairs used to sign first-party plugin archives. Read this
end-to-end before minting any production key. The cost of leaking a
private signing key is every plugin shipped under that key id must be
revoked across the fleet, which means a forced agent upgrade for every
deployed unit.

This doc covers the keys named in `FIRST_PARTY_SIGNERS` (see
`src/ados/plugins/signing.py`). Third-party publishers run their own
key infrastructure and never touch the keys described here.

## Threat model

The plugin signing key is the root of trust for the publisher half of
the plugin pipeline. Anyone holding the private key can sign an
arbitrary archive that the agent will accept as first-party, which
unlocks the `inline` GCS isolation level and the `inprocess` agent
isolation level. Both isolation levels run plugin code in the same
process address space as the host.

Treat the private key like a code-signing certificate: offline backup,
limited online exposure, single CI signing rig, audited usage.

## Key names

Two signer ids are reserved in code:

- `altnautica-2026-A` — primary, used for every signed release until
  explicit rotation.
- `altnautica-2026-B` — reserve, ready to take over if A is rotated
  out. Mint at the same time as A so a future rotation does not
  block on key minting.

Both ids are in the `FIRST_PARTY_SIGNERS` allowlist already. The
public-half filenames installed on the agent must match these strings
exactly: `altnautica-2026-A.pem` and `altnautica-2026-B.pem`.

## Generating the keypairs

Run on a trusted workstation. Prefer a fresh machine boot, no network,
no synced clipboard manager, no screen recording. Once minted, the
private key never leaves this session except into the storage targets
documented below.

```bash
# Pick a working directory on encrypted disk.
mkdir -p ~/altnautica-keys
cd ~/altnautica-keys

# Mint the primary keypair.
ados plugin keygen altnautica-2026-A \
    --output-dir ~/altnautica-keys

# Mint the reserve keypair.
ados plugin keygen altnautica-2026-B \
    --output-dir ~/altnautica-keys
```

Each call prints a short fingerprint. Write the fingerprints down —
they are the fastest way to confirm later that the public PEM
installed on an agent matches the private key held offline.

Files produced:

```
~/altnautica-keys/altnautica-2026-A.pem        (public, mode 0644)
~/altnautica-keys/altnautica-2026-A.priv.pem   (private, mode 0600)
~/altnautica-keys/altnautica-2026-B.pem        (public, mode 0644)
~/altnautica-keys/altnautica-2026-B.priv.pem   (private, mode 0600)
```

Verify file modes before moving on:

```bash
ls -l ~/altnautica-keys/*.priv.pem
# Both files should show -rw------- (mode 0600).
```

## Storing the private keys

Three storage targets, each independent. A loss of any one must not
compromise the others.

### 1. Password manager (primary recovery)

Store each `*.priv.pem` as a Secure Note in 1Password under the
Altnautica vault. Use the signer id as the note title. Paste the
full PEM body including the `-----BEGIN PRIVATE KEY-----` and
`-----END PRIVATE KEY-----` banners. Mark the note as do-not-export.

### 2. Offline encrypted backup

Encrypt each private key with `age` against a passphrase recorded in
a separate Secure Note from the keys themselves.

```bash
# Install age if missing.
brew install age   # macOS

# Encrypt each private key.
for f in ~/altnautica-keys/*.priv.pem; do
    age --passphrase \
        -o "${f}.age" \
        "${f}"
done

# Copy the .age files to two physical media (USB stick + external SSD)
# and store them in two different physical locations.
```

After encryption verifies, scrub the plaintext private PEMs from the
working directory:

```bash
# macOS does not ship shred; use rm followed by an aggressive sync.
rm ~/altnautica-keys/*.priv.pem
sync
```

The public `.pem` files stay on the workstation — they are not secret.

### 3. CI signing rig (online use)

The release workflow at
`ADOSExtensions/.github/workflows/sign-release.yml` reads the
private key from a GitHub Actions secret named
`ALTNAUTICA_PLUGIN_KEY_A`. Store the key as a base64-encoded blob so
the workflow can pipe it through `base64 -d`:

```bash
base64 -i ~/altnautica-keys/altnautica-2026-A.priv.pem | pbcopy
# pbcopy puts the base64 text on the macOS clipboard. Paste it into
# GitHub > Settings > Secrets and variables > Actions > New repository
# secret > Name: ALTNAUTICA_PLUGIN_KEY_A. Save.
```

Repeat for the reserve key under secret name `ALTNAUTICA_PLUGIN_KEY_B`.
The workflow only references A by default; B is loaded only when a
rotation switches the active signer id.

Wipe the clipboard immediately after pasting:

```bash
pbcopy < /dev/null
```

## Installing the public keys on the agent

Public keys ride along with the install script. Drop the two PEMs in
`scripts/plugin-keys/` in the repo:

```bash
cp ~/altnautica-keys/altnautica-2026-A.pem \
    /path/to/ADOSDroneAgent/scripts/plugin-keys/altnautica-2026-A.pem
cp ~/altnautica-keys/altnautica-2026-B.pem \
    /path/to/ADOSDroneAgent/scripts/plugin-keys/altnautica-2026-B.pem
```

The signer id must equal the filename stem. The agent's
`load_trusted_keys` helper uses the filename without `.pem` as the
signer id and rejects archives signed under any other id.

Commit both PEM files (public keys are not secret) on `main`. Bump
the agent patch version in `src/ados/__init__.py` so deployed agents
pick up the new keys on the next `--upgrade`.

Confirm distribution by checking a paired agent after the upgrade:

```bash
ssh skynode.local "sudo ls -l /etc/ados/plugin-keys/"
# Expect altnautica-2026-A.pem and altnautica-2026-B.pem, mode 0600,
# owned by root.
```

## Recovery and rotation

### Rotating A → B

Use when the primary key is suspected compromised or hits its planned
retirement date.

1. Update the active signer id in the CI workflow. Edit
   `.github/workflows/sign-release.yml` and switch the secret
   reference from `secrets.ALTNAUTICA_PLUGIN_KEY_A` to
   `secrets.ALTNAUTICA_PLUGIN_KEY_B`. Switch the `--signer-id`
   argument from `altnautica-2026-A` to `altnautica-2026-B`.
2. If A is compromised (not just retiring), add `altnautica-2026-A`
   to `/etc/ados/plugin-revocations.json` and push the revocation
   to every deployed agent via the next `--upgrade`.
3. Mint the next reserve key. `ados plugin keygen altnautica-2026-C`,
   add `altnautica-2026-C` to `FIRST_PARTY_SIGNERS`, drop the public
   PEM in `scripts/plugin-keys/`, commit, bump version, ship.
4. Audit which plugins were signed under A. Re-sign every active
   plugin under B and re-publish.

### Recovering a lost private key

1. Decrypt the `.age` backup on a trusted workstation:

```bash
age --decrypt -o altnautica-2026-A.priv.pem \
    altnautica-2026-A.priv.pem.age
chmod 0600 altnautica-2026-A.priv.pem
```

2. Verify the recovered key matches the fingerprint recorded at
   mint time. Mismatch means the backup is the wrong one — stop and
   investigate. Do not push a key whose fingerprint does not match.

3. Reload the recovered key into the CI signing rig secret store and
   resume releases.

## Validation

After any key operation, sign a throwaway plugin and walk it through
the agent's verification path end-to-end:

```bash
# On the dev workstation, with the private key in scope:
ados plugin keygen testkey --output-dir /tmp/testkeys
mkdir -p /tmp/testplugin
# Author a minimal manifest.yaml + agent/plugin.py here.
ados plugin sign /tmp/testplugin \
    --key ~/altnautica-keys/altnautica-2026-A.priv.pem \
    --signer-id altnautica-2026-A \
    --output /tmp/testplugin.adosplug

# On the paired agent:
scp /tmp/testplugin.adosplug skynode.local:/tmp/
ssh skynode.local "sudo ados plugin install /tmp/testplugin.adosplug --json"
# Expect ok=true, signer_id=altnautica-2026-A.
```

If the verification fails, the public PEM on the agent does not match
the private key used to sign. Diff the fingerprints. Do not push a
real plugin until the round trip is clean.

## Audit

Track every key operation in the founder's signing log. Minimum
fields per entry: ISO 8601 timestamp, action (mint / install / rotate
/ revoke), signer id, fingerprint, operator, hostname. The log lives
alongside the encrypted backups, not in this repo.
