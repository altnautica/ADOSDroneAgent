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
    --profile <drone|ground_station>   Agent profile to install
    --name <hostname>                  mDNS hostname to set
    --pair <code>                      Pairing code (or pass it positionally)
    --upgrade                          Upgrade an existing install in place
    --force                            Clear checkpoints and reinstall
    --branch <name>                    Install from a git branch (dev)
    --channel <name>                   Release channel selector
    --version <ver>                    Pin an explicit agent version
    --display <hint>                   Display hardware hint
    --camera <hint>                    Camera hardware hint
    --uninstall                        Remove the agent
    --status                           Print install status and exit
    -h, --help                         Print this help and exit
";
