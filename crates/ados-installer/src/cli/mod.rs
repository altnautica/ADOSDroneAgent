//! Command-line surface: argument parsing + run-mode resolution.

pub mod args;
pub mod mode;

pub use args::{display_value_error, Args, ParseError};
pub use mode::RunMode;

/// Usage text printed for `--help`. Mirrors the bash installer's flag surface.
pub const USAGE: &str = "\
ados-installer — install / upgrade the ADOS Drone Agent

USAGE:
    ados-installer [OPTIONS] [PAIR_CODE]

OPTIONS:
    --profile <drone|ground_station|workstation|compute>
                                       Agent profile to install
    --name <hostname>                  mDNS hostname to set
    --no-rtl-driver                    Skip the RTL8812EU WFB radio driver build
    --world-engine / --no-world-engine Install (or skip) the World Engine
                                       extension (3D world model + compute
                                       offload). Default: on for workstation
                                       and compute, off for drone and ground
                                       station. An installed extension is left
                                       alone; plugin auto-update keeps it
                                       current.
    --no-reboot                        Do not perform the single automatic
                                       reboot when provisioning needs one (a
                                       camera/display overlay, an I2C dtparam).
                                       The install then reports degraded and
                                       names what is still staged.
    --pair <code>                      Pairing code (or pass it positionally).
                                       On an already-installed box a bare code
                                       does a fast re-pair, not a reinstall.
    --upgrade                          Upgrade an existing install in place
    --force                            Clear checkpoints and reinstall
    --branch <name>                    Install from a git branch (dev)
    --channel <name>                   Release channel selector
    --version <ver>                    Pin an explicit agent version
    --ref <commit>                     Pin the agent package AND the prebuilt
                                       binaries to one revision, from that
                                       commit's rev-<sha> release. Requires the
                                       edge channel.
    --artifacts <dir>                  Install the service binaries from this
                                       directory of locally-built artifacts
                                       instead of fetching them. Each file is
                                       named for its service (ados-video) or its
                                       release asset (ados-video-aarch64) and
                                       needs its <name>.sha256 beside it; a
                                       service the directory does not carry
                                       still comes from the release. Requires
                                       the edge channel; not combinable with
                                       --ref. Linux only.
    --display <auto|none|id>           Display selection. `auto` (the ground-
                                       station default) auto-detects a panel;
                                       `none` opts out; an explicit id must be
                                       one the detected board declares.
    --camera <hint>                    Camera hardware hint
    --wifi-ssid <ssid>                 Join this Wi-Fi network during a headless
                                       install (so the wired cable can be unplugged)
    --wifi-pass <password>             Password for --wifi-ssid (omit if open)
    --uninstall                        Remove the agent
    --status                           Print install status and exit
    --plain                            Plain line output (no animation/color)
    --quiet                            Print only the final summary
    --json                             Machine output on stdout; no progress UI
    --no-color                         Disable color in the progress UI
    --ascii                            ASCII glyph fallback
    -y, --yes                          Accept the detected defaults; skip the
                                       interactive setup and install right away
    --non-interactive                  Never prompt; run the silent, flag-driven
                                       install (the identity flags above are the
                                       automation surface)
    -h, --help                         Print this help and exit

On a fresh, interactive install with none of the identity flags set, an
onboarding wizard walks you through the setup. It is skipped automatically
whenever a decisive flag is given, in --json/--quiet/CI, or when no terminal
is attached.
";
