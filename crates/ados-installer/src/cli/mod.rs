//! Command-line surface: argument parsing + run-mode resolution.

pub mod args;
pub mod mode;

pub use args::{Args, ParseError};
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
    --pair <code>                      Pairing code (or pass it positionally).
                                       On an already-installed box a bare code
                                       does a fast re-pair, not a reinstall.
    --upgrade                          Upgrade an existing install in place
    --force                            Clear checkpoints and reinstall
    --branch <name>                    Install from a git branch (dev)
    --channel <name>                   Release channel selector
    --version <ver>                    Pin an explicit agent version
    --display <hint>                   Display hardware hint
    --camera <hint>                    Camera hardware hint
    --uninstall                        Remove the agent
    --status                           Print install status and exit
    --plain                            Plain line output (no animation/color)
    --quiet                            Print only the final summary
    --json                             Machine output on stdout; no progress UI
    --no-color                         Disable color in the progress UI
    --ascii                            ASCII glyph fallback
    -h, --help                         Print this help and exit
";
