//! Hand-rolled argv parser (the workspace has no `clap`).
//!
//! Supports the same surface the bash installer accepts: the profile/name/pair
//! identity flags, the `--upgrade` / `--force` install-mode flags, the
//! branch/channel/version source selectors, the display/camera hardware hints,
//! and the `--uninstall` / `--status` / `--help` actions. A bare positional
//! argument is treated as a pair code (the `--pair KEY` shorthand).

/// Parsed command-line arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    /// `--profile <v>` — `drone` | `ground_station` | `workstation` | `compute`
    /// (the ground-station spelling is normalized; others pass through).
    pub profile: Option<String>,
    /// `--name <v>` — mDNS hostname to set.
    pub name: Option<String>,
    /// `--pair <v>` or a bare positional — pairing code / key.
    pub pair: Option<String>,
    /// `--upgrade` — upgrade an existing install in place.
    pub upgrade: bool,
    /// `--force` — clear checkpoints and reinstall from scratch.
    pub force: bool,
    /// `--branch <v>` — install from a git branch (dev path).
    pub branch: Option<String>,
    /// `--channel <v>` — release channel selector.
    pub channel: Option<String>,
    /// `--version <v>` — pin an explicit agent version.
    pub version: Option<String>,
    /// `--display <v>` — display hardware hint.
    pub display: Option<String>,
    /// `--camera <v>` — camera hardware hint.
    pub camera: Option<String>,
    /// `--wifi-ssid <v>` — join this Wi-Fi network non-interactively during a
    /// headless (flag-driven) install, so the operator can later unplug the
    /// wired cable. The join reuses the same nmcli path as the onboarding
    /// wizard and never touches the interface the SSH session rides on.
    pub wifi_ssid: Option<String>,
    /// `--wifi-pass <v>` — password for `--wifi-ssid` (omit for an open network).
    pub wifi_pass: Option<String>,
    /// `--uninstall` — remove the agent.
    pub uninstall: bool,
    /// `--status` — print install status and exit.
    pub status: bool,
    /// `--plain` — force the escape-free line renderer (no animation/color).
    pub plain: bool,
    /// `--quiet` — print only the final summary (and errors).
    pub quiet: bool,
    /// `--json` — machine output on stdout; no progress UI.
    pub json: bool,
    /// `--no-color` — disable color in the rich renderer.
    pub no_color: bool,
    /// `--ascii` — ASCII glyph fallback (no Unicode box/spinner).
    pub ascii: bool,
    /// `--no-rtl-driver` — skip building the RTL8812EU WFB radio driver (a node
    /// with no long-range radio, e.g. workstation/compute, does not need it).
    pub no_rtl_driver: bool,
    /// `--yes` / `-y` — accept the auto-detected defaults and skip the
    /// interactive onboarding wizard (the trust-the-defaults fast path).
    pub yes: bool,
    /// `--non-interactive` — never open the terminal for the onboarding wizard;
    /// force the silent, flag-driven install.
    pub non_interactive: bool,
    /// `--help` — print usage and exit.
    pub help: bool,
}

/// Normalize a profile spelling: the wire/hyphen form `ground-station` and the
/// on-disk/underscore form `ground_station` are the same profile. Everything
/// else (including `drone`) passes through unchanged.
pub fn normalize_profile(raw: &str) -> String {
    match raw {
        "ground-station" | "ground_station" => "ground_station".to_string(),
        other => other.to_string(),
    }
}

impl Args {
    /// Parse from the process arguments (skipping argv[0]).
    pub fn from_env() -> Result<Self, ParseError> {
        Self::parse(std::env::args().skip(1))
    }

    /// Parse an arbitrary argument iterator. A flag that expects a value but
    /// hits end-of-input or another flag yields a `ParseError`.
    pub fn parse<I, S>(iter: I) -> Result<Self, ParseError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let tokens: Vec<String> = iter.into_iter().map(Into::into).collect();
        let mut args = Args::default();
        let mut i = 0;

        while i < tokens.len() {
            let tok = tokens[i].as_str();
            match tok {
                "--upgrade" => args.upgrade = true,
                "--force" => args.force = true,
                "--uninstall" => args.uninstall = true,
                "--status" => args.status = true,
                "--plain" => args.plain = true,
                "--quiet" => args.quiet = true,
                "--json" => args.json = true,
                "--no-color" => args.no_color = true,
                "--ascii" => args.ascii = true,
                "--no-rtl-driver" => args.no_rtl_driver = true,
                "--yes" | "-y" => args.yes = true,
                "--non-interactive" => args.non_interactive = true,
                "--help" | "-h" => args.help = true,
                "--profile" => args.profile = Some(take_value(&tokens, &mut i, "--profile")?),
                "--name" => args.name = Some(take_value(&tokens, &mut i, "--name")?),
                "--pair" => args.pair = Some(take_value(&tokens, &mut i, "--pair")?),
                "--branch" => args.branch = Some(take_value(&tokens, &mut i, "--branch")?),
                "--channel" => args.channel = Some(take_value(&tokens, &mut i, "--channel")?),
                "--version" => args.version = Some(take_value(&tokens, &mut i, "--version")?),
                "--display" => args.display = Some(take_value(&tokens, &mut i, "--display")?),
                "--camera" => args.camera = Some(take_value(&tokens, &mut i, "--camera")?),
                "--wifi-ssid" => args.wifi_ssid = Some(take_value(&tokens, &mut i, "--wifi-ssid")?),
                "--wifi-pass" => args.wifi_pass = Some(take_value(&tokens, &mut i, "--wifi-pass")?),
                other if other.starts_with('-') => {
                    return Err(ParseError::UnknownFlag(other.to_string()));
                }
                positional => {
                    // A bare token is the pair code (the `--pair KEY` shorthand).
                    if args.pair.is_some() {
                        return Err(ParseError::UnexpectedPositional(positional.to_string()));
                    }
                    args.pair = Some(positional.to_string());
                }
            }
            i += 1;
        }

        // Normalize the profile spelling at the parse boundary.
        if let Some(p) = args.profile.take() {
            args.profile = Some(normalize_profile(&p));
        }

        Ok(args)
    }
}

/// Consume the value token following a value-taking flag, advancing the index.
fn take_value(tokens: &[String], i: &mut usize, flag: &str) -> Result<String, ParseError> {
    let next = tokens.get(*i + 1);
    match next {
        Some(v) if !is_flag(v) => {
            *i += 1;
            Ok(v.clone())
        }
        _ => Err(ParseError::MissingValue(flag.to_string())),
    }
}

/// A token is a flag if it begins with `-` and is longer than one char (so a
/// bare `-` would be treated as a value, matching common CLI conventions).
fn is_flag(tok: &str) -> bool {
    tok.starts_with('-') && tok.len() > 1
}

/// Argument-parse failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// A value-taking flag was the last token, or was followed by a flag.
    #[error("flag {0} expects a value")]
    MissingValue(String),
    /// An unrecognized `--flag`.
    #[error("unknown flag {0}")]
    UnknownFlag(String),
    /// A second bare positional after the pair code was already set.
    #[error("unexpected positional argument {0}")]
    UnexpectedPositional(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_args_is_default() {
        let a = Args::parse(Vec::<String>::new()).unwrap();
        assert_eq!(a, Args::default());
    }

    #[test]
    fn parses_value_flags() {
        let a = Args::parse([
            "--profile",
            "drone",
            "--name",
            "skynode",
            "--branch",
            "main",
            "--channel",
            "edge",
            "--version",
            "0.49.38",
            "--display",
            "spi-lcd",
            "--camera",
            "uvc",
        ])
        .unwrap();
        assert_eq!(a.profile.as_deref(), Some("drone"));
        assert_eq!(a.name.as_deref(), Some("skynode"));
        assert_eq!(a.branch.as_deref(), Some("main"));
        assert_eq!(a.channel.as_deref(), Some("edge"));
        assert_eq!(a.version.as_deref(), Some("0.49.38"));
        assert_eq!(a.display.as_deref(), Some("spi-lcd"));
        assert_eq!(a.camera.as_deref(), Some("uvc"));
    }

    #[test]
    fn parses_bool_flags() {
        let a = Args::parse(["--upgrade", "--force"]).unwrap();
        assert!(a.upgrade);
        assert!(a.force);
        assert!(!a.uninstall);
    }

    #[test]
    fn no_rtl_driver_flag_parses() {
        assert!(Args::parse(["--no-rtl-driver"]).unwrap().no_rtl_driver);
        assert!(!Args::default().no_rtl_driver);
    }

    #[test]
    fn parses_wifi_flags() {
        // SSID/password can carry spaces and symbols (quoted by the shell).
        let a = Args::parse(["--wifi-ssid", "Home Net", "--wifi-pass", "s3cr3t"]).unwrap();
        assert_eq!(a.wifi_ssid.as_deref(), Some("Home Net"));
        assert_eq!(a.wifi_pass.as_deref(), Some("s3cr3t"));
        // Both absent by default (no headless Wi-Fi join unless requested).
        assert!(Args::default().wifi_ssid.is_none());
        assert!(Args::default().wifi_pass.is_none());
        // An SSID with no password parses (open network).
        let open = Args::parse(["--wifi-ssid", "cafe"]).unwrap();
        assert_eq!(open.wifi_ssid.as_deref(), Some("cafe"));
        assert!(open.wifi_pass.is_none());
    }

    #[test]
    fn wizard_gate_flags_parse() {
        let y = Args::parse(["--yes"]).unwrap();
        assert!(y.yes);
        assert!(Args::parse(["-y"]).unwrap().yes);
        let ni = Args::parse(["--non-interactive"]).unwrap();
        assert!(ni.non_interactive);
        // Neither is set by default.
        assert!(!Args::default().yes);
        assert!(!Args::default().non_interactive);
    }

    #[test]
    fn pair_via_flag_and_positional() {
        let flagged = Args::parse(["--pair", "ABCD-1234"]).unwrap();
        assert_eq!(flagged.pair.as_deref(), Some("ABCD-1234"));
        let positional = Args::parse(["ABCD-1234"]).unwrap();
        assert_eq!(positional.pair.as_deref(), Some("ABCD-1234"));
    }

    #[test]
    fn ground_station_profile_is_normalized() {
        let hyphen = Args::parse(["--profile", "ground-station"]).unwrap();
        assert_eq!(hyphen.profile.as_deref(), Some("ground_station"));
        let under = Args::parse(["--profile", "ground_station"]).unwrap();
        assert_eq!(under.profile.as_deref(), Some("ground_station"));
    }

    #[test]
    fn missing_value_errors() {
        let err = Args::parse(["--profile"]).unwrap_err();
        assert_eq!(err, ParseError::MissingValue("--profile".to_string()));
        // A flag immediately following a value-taking flag also errors.
        let err2 = Args::parse(["--name", "--force"]).unwrap_err();
        assert_eq!(err2, ParseError::MissingValue("--name".to_string()));
    }

    #[test]
    fn unknown_flag_errors() {
        let err = Args::parse(["--bogus"]).unwrap_err();
        assert_eq!(err, ParseError::UnknownFlag("--bogus".to_string()));
    }

    #[test]
    fn second_positional_errors() {
        let err = Args::parse(["CODE1", "CODE2"]).unwrap_err();
        assert_eq!(err, ParseError::UnexpectedPositional("CODE2".to_string()));
    }

    #[test]
    fn help_short_and_long() {
        assert!(Args::parse(["-h"]).unwrap().help);
        assert!(Args::parse(["--help"]).unwrap().help);
    }
}
