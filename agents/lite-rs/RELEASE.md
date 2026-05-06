# Lite agent — local release flow

This is the canonical flow for cutting a lite-agent release. Builds run
on the maintainer's Mac (or any Linux box with Docker), artifacts are
signed locally, and the GitHub Release is published from the same shell.
The CI workflow at `.github/workflows/lite-agent-release.yml` and
`.github/workflows/image-build.yml` remain available as a fallback but
are not the primary path.

## One-time setup

```sh
cargo install cross           # Docker-based cross-compile
brew install minisign gh      # signing + GitHub CLI
gh auth login                 # one-time, needs `repo` scope
```

You will also need:

- **Docker Desktop** running (for `cross` and the Luckfox Buildroot SDK).
- **Minisign keypair** for the lite agent. The public key is vendored in
  `scripts/install-lite.sh` so installs verify against it. Keep the
  private key encrypted in a password manager and load it before each
  release:

  ```sh
  export LITE_AGENT_MINISIGN_KEY="$(cat ~/.config/ados/lite-agent-minisign.key)"
  export LITE_AGENT_MINISIGN_PASSWORD='your-password-here'
  ```

## Cutting a release

From the repo root:

```sh
make -C agents/lite-rs release VERSION=0.1.4
```

That single command:

1. Cross-compiles the Rust binary for `armv7-musl`, `aarch64-glibc`,
   `aarch64-musl`, and `x86_64-musl`.
2. Builds the Luckfox Pico Zero flashable image via the imagebuilder
   driver (`agents/lite-rs/imagebuilder/lib/build-driver.sh`).
3. Emits `ados-agent-manifest.json` from the HAL board YAMLs (this is
   the JSON the GCS Flash Tool fetches via its `/api/ados-manifest`
   proxy).
4. Signs every artifact with minisign, generates a SHA256 checksum.
5. Calls `gh release create lite-v0.1.4` and uploads the manifest,
   image, signature, and checksum to the public agent repo.

## Individual targets during development

| Target | What it does |
|--------|---|
| `make build` | Cross-compile every target |
| `make build-luckfox` | Cross-compile `armv7-musl` only |
| `make image` | Build the Luckfox image |
| `make manifest` | Emit `dist/ados-agent-manifest.json` |
| `make sign` | Sign every existing artifact in `dist/` |
| `make clean` | Remove `dist/` |

## What the GCS does with the manifest

Mission Control's Flash Tool calls `/api/ados-manifest` (a 1-hour-cached
proxy) which fetches
`https://github.com/altnautica/ADOSDroneAgent/releases/latest/download/ados-agent-manifest.json`.
Each board entry declares whether it gets a curl install or a browser
flash. The Luckfox image URL embedded in the manifest also points at the
same release, so a single `make release` cycle keeps the GCS, the
manifest, the image, and the signatures in lockstep.

If no GitHub release is reachable (first-run, network outage), the GCS
proxy serves an embedded fallback manifest so the Flash Tool stays
usable. The fallback only contains curl-install boards.

## Verifying a published release end-to-end

```sh
# 1. Download the manifest the GCS will see
curl -fsSL \
  https://github.com/altnautica/ADOSDroneAgent/releases/latest/download/ados-agent-manifest.json \
  | jq '.agentVersion, (.boards | length)'

# 2. Download + verify the Luckfox image signature
curl -fLO https://github.com/altnautica/ADOSDroneAgent/releases/latest/download/ados-luckfox-pico-zero-0.1.4.img.gz
curl -fLO https://github.com/altnautica/ADOSDroneAgent/releases/latest/download/ados-luckfox-pico-zero-0.1.4.img.gz.minisig
minisign -V -P "$(grep -oE 'RW[A-Za-z0-9+/=]+' ../../scripts/install-lite.sh | head -n1)" \
  -m ados-luckfox-pico-zero-0.1.4.img.gz

# 3. Sanity-check from the GCS dev server
cd ../../  # back to ADOSMissionControl
curl -fsS http://localhost:4000/api/ados-manifest | jq '.boards[] | {id, stacks}'
```
