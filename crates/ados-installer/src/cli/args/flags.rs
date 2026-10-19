//! The installer flag table: one row per flag, driving the parser, the
//! `--help` text and the JSON export used by the reference docs.

use super::Args;

/// One installer flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagSpec {
    /// Long spelling, e.g. `--profile`.
    pub name: &'static str,
    /// Optional short alias, e.g. `-y`.
    pub short: Option<&'static str>,
    /// Whether the flag consumes the next token as its value.
    pub takes_value: bool,
    /// Placeholder shown in help, e.g. `<hostname>`.
    pub value_name: Option<&'static str>,
    /// Help text (may span lines; continuation lines are indented in `--help`).
    pub help: &'static str,
}

const fn flag(name: &'static str, help: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        takes_value: false,
        value_name: None,
        help,
    }
}

const fn valued(name: &'static str, value_name: &'static str, help: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        short: None,
        takes_value: true,
        value_name: Some(value_name),
        help,
    }
}

const fn with_short(mut f: FlagSpec, short: &'static str) -> FlagSpec {
    f.short = Some(short);
    f
}

/// Every flag the installer accepts, in `--help` order.
pub const FLAGS: &[FlagSpec] = &[
    valued(
        "--profile",
        "<drone|ground_station|workstation|compute|visor>",
        "Agent profile to install",
    ),
    valued("--name", "<hostname>", "mDNS hostname to set"),
    flag(
        "--no-rtl-driver",
        "Skip the RTL8812EU WFB radio driver build",
    ),
    flag(
        "--world-engine",
        "Install the World Engine extension (3D world model + compute\n\
         offload). Default: on for workstation and compute, off for drone\n\
         and ground station. An installed extension is left alone; plugin\n\
         auto-update keeps it current. The last of --world-engine and\n\
         --no-world-engine wins.",
    ),
    flag("--no-world-engine", "Skip the World Engine extension"),
    flag(
        "--no-reboot",
        "Do not perform the single automatic reboot when provisioning needs\n\
         one (a camera/display overlay, an I2C dtparam). The install then\n\
         reports degraded and names what is still staged.",
    ),
    valued(
        "--pair",
        "<code>",
        "Pairing code (or pass it positionally). On an already-installed\n\
         box a bare code does a fast re-pair, not a reinstall.",
    ),
    flag("--upgrade", "Upgrade an existing install in place"),
    flag("--force", "Clear checkpoints and reinstall"),
    flag(
        "--force-armed",
        "Proceed with an upgrade even when the vehicle is (or may be) armed",
    ),
    valued("--branch", "<name>", "Install from a git branch (dev)"),
    valued("--channel", "<name>", "Release channel selector"),
    valued("--version", "<ver>", "Pin an explicit agent version"),
    valued(
        "--ref",
        "<commit>",
        "Pin the agent package AND the prebuilt binaries to one revision,\n\
         from that commit's rev-<sha> release. Requires the edge channel.",
    ),
    valued(
        "--artifacts",
        "<dir>",
        "Install the service binaries from this directory of locally-built\n\
         artifacts instead of fetching them. Each file is named for its\n\
         service (ados-video) or its release asset (ados-video-aarch64) and\n\
         needs its <name>.sha256 beside it; a service the directory does not\n\
         carry still comes from the release. Requires the edge channel; not\n\
         combinable with --ref. Linux only.",
    ),
    valued(
        "--display",
        "<auto|none|id>",
        "Display selection. `auto` (the ground-station default) auto-detects\n\
         a panel; `none` opts out; an explicit id must be one the detected\n\
         board declares.",
    ),
    valued("--camera", "<hint>", "Camera hardware hint"),
    valued(
        "--wifi-ssid",
        "<ssid>",
        "Join this Wi-Fi network during a headless install (so the wired\n\
         cable can be unplugged)",
    ),
    valued(
        "--wifi-pass-file",
        "<path>",
        "File holding the --wifi-ssid password (omit if open; keeps it off\n\
         the command line)",
    ),
    with_short(
        valued(
            "--plugin-key",
            "<path>",
            "Plugin trust key (PEM) to copy into /etc/ados/plugin-keys/ on\n\
             install. Repeatable.",
        ),
        "-k",
    ),
    flag("--uninstall", "Remove the agent"),
    flag("--status", "Print install status and exit"),
    flag("--plain", "Plain line output (no animation/color)"),
    flag("--quiet", "Print only the final summary"),
    flag("--json", "Machine output on stdout; no progress UI"),
    flag("--no-color", "Disable color in the progress UI"),
    flag("--ascii", "ASCII glyph fallback"),
    with_short(
        flag(
            "--yes",
            "Accept the detected defaults; skip the interactive setup and\n\
             install right away",
        ),
        "-y",
    ),
    flag(
        "--non-interactive",
        "Never prompt; run the silent, flag-driven install (the identity\n\
         flags above are the automation surface)",
    ),
    with_short(flag("--help", "Print this help and exit"), "-h"),
];

/// Look a token up in [`FLAGS`] by its long or short spelling.
pub fn find_flag(tok: &str) -> Option<&'static FlagSpec> {
    FLAGS.iter().find(|f| f.name == tok || f.short == Some(tok))
}

/// The `--help` text, rendered from [`FLAGS`].
pub fn usage() -> String {
    const COL: usize = 39;
    let mut out = String::from(
        "ados-installer — install / upgrade the ADOS Drone Agent\n\n\
         USAGE:\n    ados-installer [OPTIONS] [PAIR_CODE]\n\nOPTIONS:\n",
    );
    for f in FLAGS {
        let mut head = String::from("    ");
        if let Some(s) = f.short {
            head.push_str(s);
            head.push_str(", ");
        }
        head.push_str(f.name);
        if let Some(v) = f.value_name {
            head.push(' ');
            head.push_str(v);
        }
        let mut lines = f.help.lines();
        let first = lines.next().unwrap_or("");
        if head.chars().count() < COL {
            let pad = COL - head.chars().count();
            out.push_str(&format!("{head}{}{first}\n", " ".repeat(pad)));
        } else {
            out.push_str(&format!("{head}\n{}{first}\n", " ".repeat(COL)));
        }
        for l in lines {
            out.push_str(&format!("{}{}\n", " ".repeat(COL), l.trim_start()));
        }
    }
    out.push_str(
        "\nOn a fresh, interactive install with none of the identity flags set, an\n\
         onboarding wizard walks you through the setup. It is skipped automatically\n\
         whenever a decisive flag is given, in --json/--quiet/CI, or when no terminal\n\
         is attached.\n",
    );
    out
}

impl Args {
    /// Record one parsed flag (by its canonical long name) on `self`. Every
    /// name in [`FLAGS`] has an arm; `value` is `Some` exactly for the
    /// value-taking flags.
    pub(super) fn apply(&mut self, name: &str, value: Option<String>) {
        match name {
            "--upgrade" => self.upgrade = true,
            "--force" => self.force = true,
            "--force-armed" => self.force_armed = true,
            "--uninstall" => self.uninstall = true,
            "--status" => self.status = true,
            "--plain" => self.plain = true,
            "--quiet" => self.quiet = true,
            "--json" => self.json = true,
            "--no-color" => self.no_color = true,
            "--ascii" => self.ascii = true,
            "--no-rtl-driver" => self.no_rtl_driver = true,
            "--world-engine" => self.world_engine = Some(true),
            "--no-world-engine" => self.world_engine = Some(false),
            "--no-reboot" => self.no_reboot = true,
            "--yes" => self.yes = true,
            "--non-interactive" => self.non_interactive = true,
            "--help" => self.help = true,
            "--profile" => self.profile = value,
            "--name" => self.name = value,
            "--pair" => self.pair = value,
            "--branch" => self.branch = value,
            "--channel" => self.channel = value,
            "--version" => self.version = value,
            "--ref" => self.rev = value,
            "--artifacts" => self.artifacts = value,
            "--display" => self.display = value,
            "--camera" => self.camera = value,
            "--wifi-ssid" => self.wifi_ssid = value,
            "--wifi-pass-file" => self.wifi_pass_file = value,
            "--plugin-key" => self.plugin_key.extend(value),
            other => unreachable!("flag {other} is in FLAGS but has no apply arm"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_table_parses_every_flag() {
        for f in FLAGS {
            let mut argv = vec![f.name.to_string()];
            if f.takes_value {
                argv.push("value".into());
            }
            let parsed = Args::parse(argv.clone())
                .unwrap_or_else(|e| panic!("{} did not parse: {e}", f.name));
            assert_ne!(parsed, Args::default(), "{} changed nothing", f.name);
            if let Some(s) = f.short {
                argv[0] = s.to_string();
                assert_eq!(Args::parse(argv).unwrap(), parsed, "{s} != {}", f.name);
            }
            assert_eq!(f.takes_value, f.value_name.is_some(), "{}", f.name);
            assert!(usage().contains(f.name), "{} missing from usage", f.name);
        }
    }

    #[test]
    fn flag_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for f in FLAGS {
            assert!(seen.insert(f.name), "duplicate {}", f.name);
            if let Some(s) = f.short {
                assert!(seen.insert(s), "duplicate {s}");
            }
        }
    }
}
