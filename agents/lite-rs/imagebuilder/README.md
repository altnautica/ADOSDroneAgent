# ADOS lite agent — universal image builder

This directory holds the orchestrator that produces flashable SD-card
images for every SBC the lite-rs Rust drone agent runs on.

The ADOS image-build pipeline is **single-orchestrator, multi-board**:
one set of shared helpers, one shared rootfs overlay, one CI workflow,
and a per-board recipe that knows how to drive the board's native build
flow (vendor SDK, pi-gen, Armbian, JetPack, etc.).

## Layout

```
imagebuilder/
├── README.md                 ← you are here
├── lib/
│   ├── common.sh             ← shared shell helpers (logging, sign, gzip+sha256, publish)
│   └── build-driver.sh       ← orchestrator entry point: run-recipe-for-a-board-slug
├── overlay/                  ← UNIVERSAL rootfs overlay — same on every board
│   ├── etc/ados/ap-fallback/  hostapd.conf.template + dnsmasq.conf
│   ├── etc/init.d/            S99ados-agent-lite (busybox sysv-rc init script)
│   └── etc/systemd/system/    ados-agent-lite.service (systemd unit)
├── boards/
│   └── <slug>/
│       ├── recipe.sh          board-specific build recipe (4 required + 3 optional bash hooks)
│       ├── board.yaml         board metadata: SoC, kernel, target triple, drivers, SDK source
│       ├── patches/           per-board patch files (against vendor SDK files)
│       └── drivers/           out-of-tree kernel-module recipes
├── packaging/
│   └── <pkg>/                 cross-board userspace packages (e.g. ados-rkmpi-wrapper)
└── ci/
    └── matrix.yaml            board → triple → toolchain map for the GitHub Actions matrix
```

## Recipe contract

Each `boards/<slug>/recipe.sh` defines four required bash functions:

```bash
recipe::sdk_clone()      # clone vendor SDK / pi-gen / Armbian source
recipe::sdk_configure()  # board-specific defconfig / lunch / configure
recipe::sdk_build()      # build kernel + rootfs (board-specific make calls)
recipe::stage_image()    # produce an .img.gz at $IMGBUILD_OUTPUT/<artifact>
```

…plus three optional hooks called between the required ones:

```bash
recipe::pre_overlay()    # before overlay/ rsync (e.g., create dirs in rootfs)
recipe::post_overlay()   # after overlay/ rsync (e.g., enable systemd unit)
recipe::build_drivers()  # cross-build out-of-tree kernel modules
```

The orchestrator (`lib/build-driver.sh`) sources the recipe and calls
hooks in fixed order:

```
sdk_clone → sdk_configure → build_drivers → sdk_build →
pre_overlay → overlay_into → post_overlay → stage_image → sign + publish
```

## Adding a new board

```sh
mkdir -p boards/<slug>/{patches,drivers}
$EDITOR boards/<slug>/board.yaml         # see existing boards for schema
$EDITOR boards/<slug>/recipe.sh          # implement the 4 required hooks
# Add `<slug>` to .github/workflows/image-build.yml matrix.board list
git add boards/<slug>/
git commit -m "imagebuilder: <slug> board recipe"
```

CI picks up the new board automatically on the next `lite-image-v*` tag
push.

## Local build

```sh
# From the repo root, with `gh` CLI authenticated:
agents/lite-rs/imagebuilder/lib/build-driver.sh luckfox-pico-zero

# Or smoke-test the orchestrator scaffolding without a real build:
agents/lite-rs/imagebuilder/lib/build-driver.sh --check
```

Output lands at `output/<slug>/ados-<slug>-<version>.img.gz` (+ `.sha256`
+ `.minisig` if `LITE_AGENT_MINISIGN_KEY` env is set).

## Reusing overlay/ across boards

The overlay tree is the SAME on every board. Differences (busybox
sysv-rc vs systemd, uclibc vs glibc) are handled in `recipe::pre_overlay`
and `recipe::post_overlay` hooks. Common surface includes:

- `etc/ados/ap-fallback/` — Wi-Fi soft-AP fallback config templates,
  consumed by the lite agent's `WifiSupervisor` after 30 s of no
  `wpa_supplicant` association.
- `etc/init.d/S99ados-agent-lite` — busybox sysv-rc init script (used
  on Luckfox + other busybox boards; deleted in `post_overlay` on
  systemd boards).
- `etc/systemd/system/ados-agent-lite.service` — systemd unit (used on
  Pi-class + Rockchip BSP + Jetson; ignored on busybox boards).
- `usr/local/bin/ados-agent-lite` — placeholder; real binary downloaded
  from the `lite-agent-main` rolling Release in `recipe::post_overlay`.

The agent binary is NEVER built inside this orchestrator — the
`lite-agent-release.yml` workflow publishes per-triple tarballs on
every push to main, and the recipe pulls the right one for its target.

## Signing

Both image artifacts and binary artifacts are signed by the same
minisign Ed25519 keypair. The public key is embedded at:

- `scripts/install-lite.sh` (curl-installer path)
- `docs/oem/key-rotation-policy.md` (active fingerprint reference)

The secret key + password are stored as repo secrets:

- `LITE_AGENT_MINISIGN_KEY`
- `LITE_AGENT_MINISIGN_PASSWORD`

Operators verify any artifact via:

```sh
minisign -V -P "$(grep -oE 'RW[A-Za-z0-9+/=]+' scripts/install-lite.sh | head -n1)" \
         -m <artifact>
```

## Why this exists

The previous attempt assumed every board was a stock Buildroot tree
that we could layer a `BR2_EXTERNAL` on. That works for some boards
(e.g. anything where we control the kernel + bootloader). It does not
work for vendor-SDK boards like Luckfox where the SDK has its own build
orchestrator. It also does not fit Debian-rootfs boards like Raspberry
Pi where pi-gen is the canonical path.

This orchestrator embraces the heterogeneity: each board's recipe
speaks its native build language, the shared overlay carries the parts
that ARE universal (agent binary, AP fallback, systemd unit, etc.), and
adding a new board is a single PR.
