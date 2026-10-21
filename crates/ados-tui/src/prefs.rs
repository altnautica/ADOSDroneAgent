//! Per-user terminal preferences (`tui.json`).
//!
//! Lives at `$XDG_CONFIG_HOME/ados/tui.json`, else `$HOME/.config/ados/tui.json`,
//! else `%APPDATA%\ados\tui.json`. A missing file is the defaults; an
//! unparsable one is the defaults for this session with saving disabled, so a
//! hand-edited file is never overwritten by a parse failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Accepted refresh intervals in milliseconds.
pub const REFRESH_CHOICES: [u64; 3] = [1000, 2000, 5000];
pub const DEFAULT_REFRESH_MS: u64 = 2000;

/// A saved agent target.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTarget {
    pub name: String,
    pub host: String,
    pub port: u16,
    /// The pairing key, when the operator chose to remember it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl SavedTarget {
    /// The base URL, e.g. `http://192.168.1.50:8080`.
    pub fn base(&self) -> String {
        format!("http://{}:{}", host_for_url(&self.host), self.port)
    }
}

/// Bracket a bare IPv6 literal for use in a URL.
pub fn host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// The preferences file body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub version: u32,
    pub theme: String,
    pub color_blind: bool,
    pub symbols: String,
    pub motion: String,
    pub mouse: bool,
    pub refresh_ms: u64,
    /// `overview` or `last`.
    pub start_tab: String,
    pub last_tab: String,
    /// Favourite parameter names per parameter catalog key.
    pub favorite_params: BTreeMap<String, Vec<String>>,
    pub targets: Vec<SavedTarget>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            version: 1,
            theme: "amber".into(),
            color_blind: false,
            symbols: "unicode".into(),
            motion: "auto".into(),
            mouse: false,
            refresh_ms: DEFAULT_REFRESH_MS,
            start_tab: "overview".into(),
            last_tab: "overview".into(),
            favorite_params: BTreeMap::new(),
            targets: Vec::new(),
        }
    }
}

/// Loaded preferences plus where and whether to save them.
#[derive(Debug, Clone)]
pub struct PrefsStore {
    pub prefs: Prefs,
    path: Option<PathBuf>,
    /// False when the file existed but did not parse: never overwrite it.
    pub writable: bool,
}

/// The preferences path from the given environment lookups.
pub fn path_from(
    xdg_config_home: Option<String>,
    home: Option<String>,
    appdata: Option<String>,
) -> Option<PathBuf> {
    let nonempty = |v: Option<String>| v.filter(|s| !s.is_empty());
    if let Some(x) = nonempty(xdg_config_home) {
        return Some(Path::new(&x).join("ados").join("tui.json"));
    }
    if let Some(h) = nonempty(home) {
        return Some(Path::new(&h).join(".config").join("ados").join("tui.json"));
    }
    nonempty(appdata).map(|a| Path::new(&a).join("ados").join("tui.json"))
}

/// The preferences path for this process.
pub fn env_path() -> Option<PathBuf> {
    path_from(
        std::env::var("XDG_CONFIG_HOME").ok(),
        std::env::var("HOME").ok(),
        std::env::var("APPDATA").ok(),
    )
}

/// Clamp out-of-range values to their defaults.
fn sanitize(mut p: Prefs) -> Prefs {
    if !REFRESH_CHOICES.contains(&p.refresh_ms) {
        p.refresh_ms = DEFAULT_REFRESH_MS;
    }
    p
}

impl PrefsStore {
    /// Load from `path`. Missing → defaults (writable); unreadable or
    /// unparsable → defaults, not writable.
    pub fn load(path: Option<PathBuf>) -> Self {
        let Some(p) = path.clone() else {
            return Self {
                prefs: Prefs::default(),
                path: None,
                writable: false,
            };
        };
        match std::fs::read_to_string(&p) {
            Ok(text) => match serde_json::from_str::<Prefs>(&text) {
                Ok(prefs) => Self {
                    prefs: sanitize(prefs),
                    path,
                    writable: true,
                },
                Err(_) => Self {
                    prefs: Prefs::default(),
                    path,
                    writable: false,
                },
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self {
                prefs: Prefs::default(),
                path,
                writable: true,
            },
            Err(_) => Self {
                prefs: Prefs::default(),
                path,
                writable: false,
            },
        }
    }

    /// Persist atomically (write `tui.json.tmp`, then rename). Owner-only on
    /// unix: the file can hold pairing keys. A no-op when not writable.
    pub fn save(&self) -> std::io::Result<()> {
        let (Some(path), true) = (self.path.as_ref(), self.writable) else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(&self.prefs)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    }

    /// The file path, when one could be resolved.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ados-tui-prefs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_xdg_dir_wins_then_home_then_appdata() {
        assert_eq!(
            path_from(Some("/x".into()), Some("/h".into()), None),
            Some(PathBuf::from("/x/ados/tui.json"))
        );
        assert_eq!(
            path_from(Some(String::new()), Some("/h".into()), None),
            Some(PathBuf::from("/h/.config/ados/tui.json"))
        );
        assert_eq!(
            path_from(None, None, Some("C:/A".into())),
            Some(Path::new("C:/A").join("ados").join("tui.json"))
        );
        assert_eq!(path_from(None, None, None), None);
    }

    #[test]
    fn an_unparsable_file_yields_defaults_and_is_never_written() {
        let dir = temp_dir("bad");
        let file = dir.join("ados").join("tui.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{not json").unwrap();
        let store = PrefsStore::load(Some(file.clone()));
        assert_eq!(store.prefs, Prefs::default());
        assert!(!store.writable);
        store.save().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{not json");
    }

    #[test]
    fn an_out_of_set_refresh_falls_back() {
        let dir = temp_dir("refresh");
        let file = dir.join("tui.json");
        std::fs::write(&file, r#"{"refresh_ms": 1234, "theme": "nvg"}"#).unwrap();
        let store = PrefsStore::load(Some(file));
        assert_eq!(store.prefs.refresh_ms, DEFAULT_REFRESH_MS);
        assert_eq!(store.prefs.theme, "nvg");
        assert!(store.writable);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("roundtrip");
        let file = path_from(Some(dir.to_string_lossy().into_owned()), None, None).unwrap();
        let mut store = PrefsStore::load(Some(file.clone()));
        assert!(store.writable, "a missing file is writable");
        store.prefs.theme = "brand-light".into();
        store.prefs.refresh_ms = 5000;
        store.prefs.targets.push(SavedTarget {
            name: "bench".into(),
            host: "192.168.1.50".into(),
            port: 8080,
            api_key: Some("k".into()),
        });
        store
            .prefs
            .favorite_params
            .insert("ardupilot-copter".into(), vec!["ATC_RAT_RLL_P".into()]);
        store.save().unwrap();
        let back = PrefsStore::load(Some(file.clone()));
        assert_eq!(back.prefs, store.prefs);
        assert!(!file.with_extension("json.tmp").exists());
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains("\"theme\": \"brand-light\""));
    }

    #[test]
    fn an_ipv6_target_is_bracketed() {
        let t = SavedTarget {
            name: "v6".into(),
            host: "fe80::1".into(),
            port: 8080,
            api_key: None,
        };
        assert_eq!(t.base(), "http://[fe80::1]:8080");
    }
}
