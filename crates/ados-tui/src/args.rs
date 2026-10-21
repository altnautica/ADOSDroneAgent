//! Command-line flags and agent target resolution.

use crate::prefs::{host_for_url, SavedTarget};

/// Where the agent stores the pairing key (matches `ados.core.paths.PAIRING_JSON`).
pub const PAIRING_JSON: &str = "/etc/ados/pairing.json";

/// The default agent HTTP port.
pub const DEFAULT_PORT: u16 = 8080;

pub const USAGE: &str = "\
usage: ados-tui [--host <host[:port]>] [--key <key>] [--target <saved-name>]

  --host <host[:port]>   connect to an agent on another machine (port 8080 by default)
  --key <key>            pairing key to send (else $ADOS_KEY, a saved target's key,
                         or the local pairing file)
  --target <name>        connect to a target saved in Settings > Agents
  --help                 show this help
  --version              show the version

Environment: ADOS_HOST, ADOS_KEY, ADOS_NO_UPDATE_CHECK.";

/// Parsed flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    pub host: Option<String>,
    pub key: Option<String>,
    pub target: Option<String>,
}

/// What parsing asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Args),
    Help,
    Version,
    /// Bad usage, with the reason.
    Error(String),
}

/// Parse the flags after the program name.
pub fn parse(argv: &[String]) -> Parsed {
    let mut args = Args::default();
    let mut it = argv.iter();
    while let Some(flag) = it.next() {
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (flag.as_str(), None),
        };
        let mut value = |n: &str| inline.clone().or_else(|| it.next().cloned()).ok_or(format!("{n} needs a value"));
        match name {
            "--help" | "-h" => return Parsed::Help,
            "--version" | "-V" => return Parsed::Version,
            "--host" => match value("--host") {
                Ok(v) => args.host = Some(v),
                Err(e) => return Parsed::Error(e),
            },
            "--key" => match value("--key") {
                Ok(v) => args.key = Some(v),
                Err(e) => return Parsed::Error(e),
            },
            "--target" => match value("--target") {
                Ok(v) => args.target = Some(v),
                Err(e) => return Parsed::Error(e),
            },
            other => return Parsed::Error(format!("unknown flag: {other}")),
        }
    }
    Parsed::Run(args)
}

/// The agent this session talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The saved-target name, when connected to one.
    pub name: Option<String>,
    /// Base URL, e.g. `http://localhost:8080`.
    pub base: String,
    /// The host part, for display and recipes.
    pub host: String,
    /// Not on this machine: shell actions and local-only features are hidden.
    pub remote: bool,
}

impl Target {
    /// The on-box agent.
    pub fn local() -> Self {
        Self::from_host(None, "localhost", DEFAULT_PORT)
    }

    pub fn from_host(name: Option<String>, host: &str, port: u16) -> Self {
        Self {
            name,
            base: format!("http://{}:{port}", host_for_url(host)),
            host: host.to_string(),
            remote: !is_loopback(host),
        }
    }

    pub fn from_saved(t: &SavedTarget) -> Self {
        Self::from_host(Some(t.name.clone()), &t.host, t.port)
    }

    /// The chip text: `local` or the host.
    pub fn label(&self) -> String {
        if self.remote {
            self.name.clone().unwrap_or_else(|| self.host.clone())
        } else {
            "local".into()
        }
    }
}

/// Whether `host` names this machine.
pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h == "::1" || h.starts_with("127.")
}

/// Split `host[:port]` (IPv6 literals in brackets) into host and port.
pub fn split_host_port(spec: &str) -> Result<(String, u16), String> {
    let spec = spec
        .trim()
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if let Some(rest) = spec.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("bad host: {spec}"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("bad port: {p}"))?,
            None => DEFAULT_PORT,
        };
        return Ok((host.to_string(), port));
    }
    match spec.rsplit_once(':') {
        // A bare IPv6 literal has several colons: no port.
        Some((host, port)) if !host.contains(':') => Ok((
            host.to_string(),
            port.parse().map_err(|_| format!("bad port: {port}"))?,
        )),
        _ if spec.is_empty() => Err("empty host".into()),
        _ => Ok((spec.to_string(), DEFAULT_PORT)),
    }
}

/// The environment inputs resolution reads, injected for tests.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub ados_host: Option<String>,
    pub ados_key: Option<String>,
    /// The local pairing file's `api_key`, when readable.
    pub pairing_key: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            ados_host: std::env::var("ADOS_HOST").ok().filter(|s| !s.is_empty()),
            ados_key: std::env::var("ADOS_KEY").ok().filter(|s| !s.is_empty()),
            pairing_key: load_pairing_key(),
        }
    }
}

/// The `api_key` of the local pairing file, when readable.
pub fn load_pairing_key() -> Option<String> {
    let text = std::fs::read_to_string(PAIRING_JSON).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("api_key")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolve the target and key. Target: `--target` (saved) → `--host` →
/// `$ADOS_HOST` → the local agent. Key: `--key` → `$ADOS_KEY` → the saved
/// target's key → (local targets only) the pairing file.
pub fn resolve(
    args: &Args,
    env: &Env,
    saved: &[SavedTarget],
) -> Result<(Target, Option<String>), String> {
    let mut saved_key = None;
    let target = if let Some(name) = &args.target {
        let t = saved
            .iter()
            .find(|t| &t.name == name)
            .ok_or_else(|| format!("no saved target named {name}"))?;
        saved_key = t.api_key.clone();
        Target::from_saved(t)
    } else if let Some(spec) = args.host.as_ref().or(env.ados_host.as_ref()) {
        let (host, port) = split_host_port(spec)?;
        // A host that matches a saved target picks up its remembered key.
        if let Some(t) = saved.iter().find(|t| t.host == host && t.port == port) {
            saved_key = t.api_key.clone();
            Target::from_saved(t)
        } else {
            Target::from_host(None, &host, port)
        }
    } else {
        Target::local()
    };
    let key = args
        .key
        .clone()
        .or_else(|| env.ados_key.clone())
        .or(saved_key)
        .or_else(|| {
            if target.remote {
                None
            } else {
                env.pairing_key.clone()
            }
        });
    Ok((target, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flags_parse_and_unknown_is_an_error() {
        assert_eq!(
            parse(&argv(&["--host", "10.0.0.2:9000", "--key=abc"])),
            Parsed::Run(Args {
                host: Some("10.0.0.2:9000".into()),
                key: Some("abc".into()),
                target: None
            })
        );
        assert_eq!(parse(&argv(&["--help"])), Parsed::Help);
        assert!(matches!(parse(&argv(&["--bogus"])), Parsed::Error(_)));
        assert!(matches!(parse(&argv(&["--host"])), Parsed::Error(_)));
    }

    #[test]
    fn host_port_splits() {
        assert_eq!(split_host_port("drone.local").unwrap(), ("drone.local".into(), 8080));
        assert_eq!(split_host_port("10.0.0.2:9000").unwrap(), ("10.0.0.2".into(), 9000));
        assert_eq!(split_host_port("[fe80::1]:81").unwrap(), ("fe80::1".into(), 81));
        assert_eq!(split_host_port("fe80::1").unwrap(), ("fe80::1".into(), 8080));
        assert!(split_host_port("x:notaport").is_err());
    }

    #[test]
    fn key_precedence_is_flag_env_saved_then_local_file() {
        let saved = vec![SavedTarget {
            name: "bench".into(),
            host: "192.168.1.50".into(),
            port: 8080,
            api_key: Some("saved".into()),
        }];
        let env = Env {
            ados_host: None,
            ados_key: Some("envkey".into()),
            pairing_key: Some("filekey".into()),
        };
        let flag = Args {
            target: Some("bench".into()),
            key: Some("flagkey".into()),
            ..Default::default()
        };
        assert_eq!(resolve(&flag, &env, &saved).unwrap().1.as_deref(), Some("flagkey"));

        let no_flag = Args {
            target: Some("bench".into()),
            ..Default::default()
        };
        assert_eq!(resolve(&no_flag, &env, &saved).unwrap().1.as_deref(), Some("envkey"));

        let no_env = Env {
            ados_key: None,
            ..env.clone()
        };
        let (t, key) = resolve(&no_flag, &no_env, &saved).unwrap();
        assert_eq!(key.as_deref(), Some("saved"));
        assert!(t.remote);
        assert_eq!(t.base, "http://192.168.1.50:8080");

        // The local pairing file is only ever sent to the local agent.
        let (t, key) = resolve(&Args::default(), &no_env, &[]).unwrap();
        assert!(!t.remote);
        assert_eq!(key.as_deref(), Some("filekey"));
        let remote = Args {
            host: Some("10.0.0.9".into()),
            ..Default::default()
        };
        assert_eq!(resolve(&remote, &no_env, &[]).unwrap().1, None);
    }

    #[test]
    fn env_host_is_used_when_no_flag() {
        let env = Env {
            ados_host: Some("10.1.1.1".into()),
            ..Default::default()
        };
        let (t, _) = resolve(&Args::default(), &env, &[]).unwrap();
        assert_eq!(t.base, "http://10.1.1.1:8080");
        assert!(t.remote);
        assert!(resolve(
            &Args {
                target: Some("missing".into()),
                ..Default::default()
            },
            &env,
            &[]
        )
        .is_err());
    }
}
