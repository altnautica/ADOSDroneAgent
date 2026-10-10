//! The live `security:` posture the LAN edge reads per request.
//!
//! The configured API key, the HMAC gate and the CORS allow-list used to be
//! read once at startup, so a `security.*` write changed nothing until the
//! service restarted, and a config that failed to parse at startup silently
//! meant "HMAC off". [`SecurityState`] reads the shared config snapshot instead
//! (one parse per change of the file, re-checked every couple of seconds) and
//! keeps the last good copy across a parse error. Only when no good copy has
//! ever been read does it report [`SecurityView::Unreadable`], which the edge
//! answers by refusing off-box mutations until the config is repaired on the
//! node.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::config::{config_snapshot, ControlSecurityConfig, CONFIG_CACHE_TTL};

/// The error code an off-box mutation gets while the config is unreadable.
pub const CONFIG_UNREADABLE_CODE: &str = "E_CONFIG_UNREADABLE";

/// The operator message paired with [`CONFIG_UNREADABLE_CODE`].
pub const CONFIG_UNREADABLE_MESSAGE: &str = "config.yaml cannot be read; fix it on the node";

/// What the security posture currently is.
#[derive(Debug, Clone)]
pub enum SecurityView {
    /// The current config, or the last one that parsed.
    Known(Arc<ControlSecurityConfig>),
    /// The config has never been readable since this process started.
    Unreadable,
}

enum Source {
    /// The agent config file, read through the shared snapshot cache.
    File { path: PathBuf, ttl: Duration },
    /// A fixed posture, for a caller that holds one already (tests, tools).
    Fixed(Arc<ControlSecurityConfig>),
}

/// The per-request security posture reader. Shared behind an `Arc` by the
/// credential check, the proxied-route gate, the CORS layer and the routes.
pub struct SecurityState {
    source: Source,
    last_good: Mutex<Option<Arc<ControlSecurityConfig>>>,
}

impl SecurityState {
    /// Read the `security:` and `mcp:` slices of the config at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self::with_ttl(path, CONFIG_CACHE_TTL)
    }

    /// As [`Self::new`], re-checking the file at most once per `ttl`. A test
    /// passes `Duration::ZERO` so a rewrite is seen on the next read.
    pub fn with_ttl(path: PathBuf, ttl: Duration) -> Self {
        Self {
            source: Source::File { path, ttl },
            last_good: Mutex::new(None),
        }
    }

    /// A posture that never changes.
    pub fn fixed(config: ControlSecurityConfig) -> Self {
        Self {
            source: Source::Fixed(Arc::new(config)),
            last_good: Mutex::new(None),
        }
    }

    /// The current posture. A parse or read error keeps the last good copy;
    /// with no good copy the view is [`SecurityView::Unreadable`].
    pub fn current(&self) -> SecurityView {
        let (path, ttl) = match &self.source {
            Source::Fixed(cfg) => return SecurityView::Known(Arc::clone(cfg)),
            Source::File { path, ttl } => (path, *ttl),
        };
        let mut last_good = self.last_good.lock();
        match config_snapshot(path, ttl).security {
            Ok(cfg) => {
                *last_good = Some(Arc::clone(&cfg));
                SecurityView::Known(cfg)
            }
            Err(_) => match last_good.as_ref() {
                Some(cfg) => SecurityView::Known(Arc::clone(cfg)),
                None => SecurityView::Unreadable,
            },
        }
    }

    /// The posture's config, or the all-defaults config while unreadable.
    /// The defaults accept no configured key and leave HMAC off; the edge has
    /// already refused every off-box mutation in that state.
    pub fn config_or_default(&self) -> Arc<ControlSecurityConfig> {
        match self.current() {
            SecurityView::Known(cfg) => cfg,
            SecurityView::Unreadable => Arc::default(),
        }
    }

    /// True while the config has never been readable.
    pub fn is_unreadable(&self) -> bool {
        matches!(self.current(), SecurityView::Unreadable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &std::path::Path, body: Option<&str>) -> (SecurityState, PathBuf) {
        let path = dir.join("config.yaml");
        if let Some(body) = body {
            std::fs::write(&path, body).unwrap();
        }
        (SecurityState::with_ttl(path.clone(), Duration::ZERO), path)
    }

    #[test]
    fn a_security_write_is_seen_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (s, path) = state(dir.path(), Some("security:\n  hmac_enabled: false\n"));
        assert!(!s.config_or_default().security.hmac_enabled);
        std::fs::write(
            &path,
            "security:\n  hmac_enabled: true\n  hmac_secret: a-long-enough-secret\n",
        )
        .unwrap();
        assert!(s.config_or_default().security.hmac_enabled);
    }

    #[test]
    fn a_parse_error_keeps_the_last_good_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (s, path) = state(
            dir.path(),
            Some("security:\n  api:\n    api_key: cfg-key\n"),
        );
        assert_eq!(s.config_or_default().security.api.api_key, "cfg-key");
        std::fs::write(&path, "security: {api: [unclosed, garbage here\n").unwrap();
        match s.current() {
            SecurityView::Known(cfg) => assert_eq!(cfg.security.api.api_key, "cfg-key"),
            SecurityView::Unreadable => panic!("the last good copy must stand"),
        }
    }

    #[test]
    fn a_config_never_read_good_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _path) = state(dir.path(), Some("security: {hmac_enabled: [unclosed\n"));
        assert!(s.is_unreadable());
        // Unreadable reads as the defaults for the accessors, never as "HMAC
        // on with no secret" or a stale key.
        assert!(!s.config_or_default().security.hmac_enabled);
    }

    #[test]
    fn an_absent_config_is_the_defaults_not_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _path) = state(dir.path(), None);
        assert!(!s.is_unreadable());
    }
}
