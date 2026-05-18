# Plugin trust keys

First-party publisher public keys live here. Drop the public-half PEM
for any signer id named in the `FIRST_PARTY_SIGNERS` allowlist (see
`src/ados/plugins/signing.py`). `install.sh` deploys these to
`/etc/ados/plugin-keys/` on every fresh install and every `--upgrade`
via the `provision_plugin_keys` helper in `install.d/09-config.sh`.

The agent loads every `*.pem` in `/etc/ados/plugin-keys/` at startup
and uses the filename stem as the signer id. The mapping is exact —
`altnautica-2026-A.pem` becomes signer id `altnautica-2026-A`. The
filename must match the string written on line 1 of the archive's
`SIGNATURE` file.

## What goes here

- Public keys only. PEM-encoded, `SubjectPublicKeyInfo` format. One
  file per signer id, filename = `<signer-id>.pem`.
- Files must be readable on the agent (mode 0644). The install step
  chmods them on copy.

## What never goes here

- Private keys (`*.priv.pem`, `*.priv`, `*.key`). Private keys live in
  the founder's offline backup and the CI signing rig's secret store.
  Never in this repo.
- Anything that is not a PEM public key. The agent skips
  non-`*.pem` files but the directory should stay clean.

## Adding or rotating a key

1. Generate the keypair (see `docs/plugin-signing/key-generation.md`
   for the founder workflow).
2. Add the new signer id to `FIRST_PARTY_SIGNERS` in
   `src/ados/plugins/signing.py`. The allowlist is intentionally a
   hardcoded `frozenset` so a file-system writer cannot impersonate
   a first-party signer just by dropping a `*.pem` with the right
   prefix.
3. Drop the public PEM here with the matching filename.
4. Bump the agent version (single source of truth in
   `src/ados/__init__.py`) and commit.
5. Distribute via the normal upgrade path
   (`scripts/install.sh --upgrade`).

## Revocation

Add the signer id to `/etc/ados/plugin-revocations.json` on every
deployed agent. The agent reads that file at archive-verification
time and refuses any plugin signed by a revoked id. The file format
is a JSON array of strings, e.g. `["altnautica-2026-A"]`.
