//! The one writer for `/etc/ados/config.yaml` on the native front.
//!
//! Every route that changes the node's config document goes through
//! [`update_config`]. The document is co-owned (the Python agent, the supervisor
//! and this surface each read keys the others never touch) and it carries
//! secrets (the MQTT password, the API key, the HMAC secret, the AP passphrase),
//! so a write here follows the same contract the supervisor's bind writer and
//! the Python `update_config` already follow:
//!
//! - **Serialised.** The read, the mutation and the write all happen inside one
//!   exclusive `flock` on the shared lock file, taken with a bounded wait. A
//!   write never proceeds without the lock: two unsynchronised read-modify-write
//!   cycles do not corrupt the file (the rename is atomic), they lose an update.
//! - **A merge, never a replace.** The on-disk mapping is read inside the lock
//!   and the caller mutates it in place, so every key this surface does not know
//!   about round-trips.
//! - **Never over a document it could not read.** An absent or empty file is an
//!   empty mapping. Any other read failure (EIO, invalid UTF-8), a parse failure
//!   (including a duplicate key a hand edit left behind), or a non-mapping root
//!   refuses the write: writing then would truncate whatever the operator has.
//! - **Owner-only.** The replacement is created `0600` with `create_new`,
//!   fsynced, renamed over the target, and the directory is fsynced, so the file
//!   is never world-readable, not even for the instant between create and rename.
//! - **Explicit outcome.** A mutation that changes nothing writes nothing, and
//!   the caller learns whether bytes landed. Every failure is a typed error the
//!   routes map to a 5xx.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_norway::{Mapping, Value as Yaml};

/// The lock file serialising writes to the canonical config document. Shared
/// with the supervisor's bind writer and the Python config writer; a lock only
/// one participant takes is not a lock.
pub const CONFIG_LOCK_PATH: &str = "/run/ados/config.yaml.lock";

/// How long a writer waits for the lock before giving up. A holder keeps it only
/// for a YAML serialise plus an atomic rename, so reaching this bound means a
/// writer is stuck; the same bound the Python writer uses.
pub const WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The mode the config document is written with. It carries secrets.
pub const CONFIG_MODE: u32 = 0o600;

const LOCK_POLL: Duration = Duration::from_millis(20);

/// Why a config write did not land. Nothing was written in any of these cases.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigWriteError {
    /// The lock could not be taken (contention past the bound, or the lock file
    /// could not be opened).
    #[error("config lock {path} unavailable: {reason}")]
    Lock { path: String, reason: String },
    /// The document exists but could not be read.
    #[error("{path} is unreadable: {reason}")]
    Read { path: String, reason: String },
    /// The document was read but is not a YAML mapping this writer can merge into.
    #[error("{path} is unparseable: {reason}")]
    Parse { path: String, reason: String },
    /// The caller's mutation refused the change.
    #[error("config update refused: {0}")]
    Mutate(String),
    /// The merged document could not be serialised.
    #[error("config is not serialisable: {0}")]
    Serialize(String),
    /// Creating, writing, syncing or renaming the replacement failed.
    #[error("writing {path} failed: {reason}")]
    Write { path: String, reason: String },
}

/// A landed (or deliberately skipped) config write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWrite<T> {
    /// Whatever the mutation returned.
    pub value: T,
    /// Whether bytes reached the file. `false` when the document already said
    /// what the caller asked for.
    pub wrote: bool,
}

/// The lock file guarding `config_path`: the shared node lock for the canonical
/// document, a sibling `<name>.lock` for any other path (a test fixture, a
/// staging copy), so a non-canonical write neither contends on nor is serialised
/// by the node's. Mirrors the Python writer's resolution.
pub fn lock_path_for(config_path: &Path) -> PathBuf {
    if config_path == Path::new(crate::config::CONFIG_YAML) {
        return PathBuf::from(CONFIG_LOCK_PATH);
    }
    let mut name = config_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    config_path.with_file_name(name)
}

/// Apply `mutate` to the on-disk config mapping, atomically, under the lock.
///
/// `mutate` edits the mapping in place and may add, change or remove keys. An
/// `Err` from it aborts the write. See the module docs for the full contract.
///
/// The write can wait up to [`WRITE_LOCK_TIMEOUT`] on another writer's lock and
/// then fsyncs, all synchronously. Every route handler calls this from an async
/// task, so on the daemon's multi-thread runtime the wait is declared blocking
/// (the scheduler moves this worker's other tasks elsewhere) rather than freezing
/// a worker the other handlers share. Outside a runtime, or on a current-thread
/// one (tests), it simply runs.
pub fn update_config<T>(
    config_path: &Path,
    mutate: impl FnOnce(&mut Mapping) -> Result<T, String>,
) -> Result<ConfigWrite<T>, ConfigWriteError> {
    let on_multi_thread = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if on_multi_thread {
        tokio::task::block_in_place(|| {
            update_config_with_timeout(config_path, WRITE_LOCK_TIMEOUT, mutate)
        })
    } else {
        update_config_with_timeout(config_path, WRITE_LOCK_TIMEOUT, mutate)
    }
}

fn update_config_with_timeout<T>(
    config_path: &Path,
    timeout: Duration,
    mutate: impl FnOnce(&mut Mapping) -> Result<T, String>,
) -> Result<ConfigWrite<T>, ConfigWriteError> {
    let display = config_path.display().to_string();
    if let Some(parent) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| ConfigWriteError::Write {
            path: display.clone(),
            reason: e.to_string(),
        })?;
    }

    let _lock = acquire_lock(&lock_path_for(config_path), timeout)?;

    let mut root = load_mapping(config_path)?;
    let before = root.clone();
    let value = mutate(&mut root).map_err(ConfigWriteError::Mutate)?;
    if root == before {
        return Ok(ConfigWrite {
            value,
            wrote: false,
        });
    }

    let body = serde_norway::to_string(&Yaml::Mapping(root))
        .map_err(|e| ConfigWriteError::Serialize(e.to_string()))?;
    write_owner_only(config_path, body.as_bytes()).map_err(|e| ConfigWriteError::Write {
        path: display,
        reason: e.to_string(),
    })?;
    Ok(ConfigWrite { value, wrote: true })
}

/// Read the document for a write. Absent, empty, or a bare YAML null is an empty
/// mapping; anything else that is not a mapping is refused.
fn load_mapping(path: &Path) -> Result<Mapping, ConfigWriteError> {
    let display = || path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Mapping::new()),
        Err(e) => {
            return Err(ConfigWriteError::Read {
                path: display(),
                reason: e.to_string(),
            })
        }
    };
    if text.trim().is_empty() {
        return Ok(Mapping::new());
    }
    match serde_norway::from_str::<Yaml>(&text) {
        Ok(Yaml::Mapping(m)) => Ok(m),
        Ok(Yaml::Null) => Ok(Mapping::new()),
        Ok(_) => Err(ConfigWriteError::Parse {
            path: display(),
            reason: "the document root is not a mapping".to_string(),
        }),
        Err(e) => Err(ConfigWriteError::Parse {
            path: display(),
            reason: e.to_string(),
        }),
    }
}

/// Take the exclusive lock, polling the non-blocking form until `timeout` (a
/// blocking `flock` cannot be given a deadline). The lock releases when the
/// returned file is dropped.
fn acquire_lock(lock_path: &Path, timeout: Duration) -> Result<File, ConfigWriteError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let err = |reason: String| ConfigWriteError::Lock {
        path: lock_path.display().to_string(),
        reason,
    };
    if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| err(e.to_string()))?;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        // A lock target only; never truncate it.
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .map_err(|e| err(e.to_string()))?;

    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(err(format!(
                        "another writer held it for {}s",
                        timeout.as_secs_f32()
                    )));
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(err(e.to_string())),
        }
    }
}

/// Replace `path` with `body`, owner-only: a `create_new` temp sibling at
/// [`CONFIG_MODE`], fsynced, renamed over the target, then the directory fsynced
/// so the rename itself is durable.
fn write_owner_only(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(CONFIG_MODE)
            .open(&tmp)?;
        // The create mode is filtered by the umask; set it outright.
        f.set_permissions(std::fs::Permissions::from_mode(CONFIG_MODE))?;
        f.write_all(body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    File::open(dir)?.sync_all()
}

/// Get (creating) the mapping at `key` under `parent`. A value there that is not
/// a mapping is replaced with an empty one, the same way the Python writer's
/// section helper treats a malformed section.
pub fn section<'a>(parent: &'a mut Mapping, key: &str) -> &'a mut Mapping {
    let node = parent
        .entry(Yaml::String(key.to_string()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    if !node.is_mapping() {
        *node = Yaml::Mapping(Mapping::new());
    }
    match node {
        Yaml::Mapping(m) => m,
        _ => unreachable!("the node was just made a mapping"),
    }
}

/// [`section`] along a path of keys (`["video", "wfb"]`).
pub fn section_path<'a>(root: &'a mut Mapping, keys: &[&str]) -> &'a mut Mapping {
    keys.iter().fold(root, |map, key| section(map, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_tx_power(root: &mut Mapping) -> Result<(), String> {
        section_path(root, &["video", "wfb"]).insert(
            Yaml::String("tx_power_dbm".to_string()),
            Yaml::Number(10.into()),
        );
        Ok(())
    }

    #[test]
    fn an_unparseable_document_is_never_written_over() {
        // A duplicate key the Python loader accepts but this parser rejects, and a
        // plain syntax error: both must leave the operator's bytes untouched.
        for original in [
            "agent:\n  name: rig\nvideo:\n  a: 1\nvideo:\n  b: 2\n",
            "agent: [unclosed\n",
            "- a list root\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = dir.path().join("config.yaml");
            std::fs::write(&cfg, original).unwrap();
            let err = update_config(&cfg, set_tx_power).unwrap_err();
            assert!(matches!(err, ConfigWriteError::Parse { .. }), "{err}");
            assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        }
    }

    #[test]
    fn an_unreadable_document_is_never_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, b"agent:\n  name: \xff\xfe\n").unwrap();
        let err = update_config(&cfg, set_tx_power).unwrap_err();
        assert!(matches!(err, ConfigWriteError::Read { .. }), "{err}");
        assert_eq!(
            std::fs::read(&cfg).unwrap(),
            b"agent:\n  name: \xff\xfe\n".to_vec()
        );
    }

    #[test]
    fn a_write_is_owner_only_whatever_the_file_had_and_whatever_the_umask() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: rig\n").unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        update_config(&cfg, set_tx_power).unwrap();
        assert_eq!(mode_of(&cfg), 0o600);

        // A fresh document is created owner-only too.
        let fresh = dir.path().join("fresh.yaml");
        update_config(&fresh, set_tx_power).unwrap();
        assert_eq!(mode_of(&fresh), 0o600);
    }

    #[test]
    fn a_write_merges_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: rig\nvideo:\n  wfb:\n    channel: 149\n",
        )
        .unwrap();
        let old_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&cfg).unwrap().ino()
        };
        let out = update_config(&cfg, set_tx_power).unwrap();
        assert!(out.wrote);

        let doc: Yaml = serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(doc["agent"]["name"], Yaml::String("rig".into()));
        assert_eq!(doc["video"]["wfb"]["channel"], Yaml::Number(149.into()));
        assert_eq!(doc["video"]["wfb"]["tx_power_dbm"], Yaml::Number(10.into()));
        // Replaced by rename, not rewritten in place, and no temp file is left.
        use std::os::unix::fs::MetadataExt;
        assert_ne!(std::fs::metadata(&cfg).unwrap().ino(), old_inode);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_absent_or_empty_document_starts_from_an_empty_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.yaml");
        assert!(update_config(&absent, set_tx_power).unwrap().wrote);
        let empty = dir.path().join("empty.yaml");
        std::fs::write(&empty, "\n").unwrap();
        assert!(update_config(&empty, set_tx_power).unwrap().wrote);
    }

    #[test]
    fn a_no_op_mutation_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        update_config(&cfg, set_tx_power).unwrap();
        let before = std::fs::metadata(&cfg).unwrap().modified().unwrap();
        let out = update_config(&cfg, set_tx_power).unwrap();
        assert!(!out.wrote);
        assert_eq!(std::fs::metadata(&cfg).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn a_refused_mutation_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: rig\n").unwrap();
        let err = update_config(&cfg, |root| {
            root.clear();
            Err::<(), _>("no".to_string())
        })
        .unwrap_err();
        assert_eq!(err, ConfigWriteError::Mutate("no".to_string()));
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            "agent:\n  name: rig\n"
        );
    }

    #[test]
    fn a_held_lock_refuses_the_write_after_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: rig\n").unwrap();
        let holder = acquire_lock(&lock_path_for(&cfg), Duration::ZERO).unwrap();
        let err =
            update_config_with_timeout(&cfg, Duration::from_millis(50), set_tx_power).unwrap_err();
        assert!(matches!(err, ConfigWriteError::Lock { .. }), "{err}");
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            "agent:\n  name: rig\n"
        );
        drop(holder);
        assert!(update_config(&cfg, set_tx_power).unwrap().wrote);
    }

    #[test]
    fn the_canonical_document_shares_the_node_lock() {
        assert_eq!(
            lock_path_for(Path::new(crate::config::CONFIG_YAML)),
            PathBuf::from(CONFIG_LOCK_PATH)
        );
        assert_eq!(
            lock_path_for(Path::new("/tmp/x/config.yaml")),
            PathBuf::from("/tmp/x/config.yaml.lock")
        );
    }
}
