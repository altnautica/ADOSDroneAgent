# Embedded Setup Webapp (do not edit here)

This directory is a **mirror** of the canonical universal setup webapp at the
repository root: `web/setup/`. The Rust lite agent embeds these assets into the
release binary via `include_dir!`, while the Python full agent serves the same
files directly from `web/setup/` at runtime. Both halves must serve byte-for-byte
identical UX, otherwise operators see two different wizards depending on which
agent profile is installed.

**Source of truth: `web/setup/` at the repository root.** Edits made directly
here will be overwritten and will NOT reach the Python full agent. To update
the wizard, edit files in `web/setup/`, then resync:

```sh
cp -r web/setup/* agents/lite-rs/crates/ados-setup/web-setup/
rm -rf agents/lite-rs/crates/ados-setup/web-setup/__pycache__
bash scripts/verify-webapp-sync.sh   # exits 0 on parity, 1 on drift
```

CI runs `scripts/verify-webapp-sync.sh` on every push (`.github/workflows/ci.yml`)
and fails the build on drift, so a missed resync cannot ship.
