use super::*;
use crate::config::CloudConfig;
use crate::pairing::PairingState;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn write_pairing(path: &Path, key: Option<&str>) {
    let body = match key {
        Some(k) => format!(r#"{{"paired": true, "api_key": "{k}"}}"#),
        None => r#"{"paired": false}"#.to_string(),
    };
    std::fs::write(path, body).unwrap();
}

fn key_loader(path: PathBuf) -> impl Fn() -> Option<String> + Clone + Send + 'static {
    move || PairingState::load_from(&path).api_key().map(str::to_string)
}

fn cloud_config() -> CloudConfig {
    let mut c = CloudConfig::default();
    c.agent.device_id = "dev-1".into();
    c.server.mode = "cloud".into();
    c.server.cloud.mqtt_broker = "mqtt.cloud.example.com".into();
    c
}

async fn settle(steps: u32) {
    for _ in 0..steps {
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
}

/// What [`start`] hands back: the passwords each run was built with, the
/// count of runs that ended, and the supervisor task.
type Harness = (Arc<Mutex<Vec<String>>>, Arc<Mutex<u32>>, JoinHandle<()>);

/// Run the supervisor with a run that records each transport password it
/// was built with and the count of runs that ended.
fn start(path: PathBuf, shutdown: watch::Receiver<bool>) -> Harness {
    let built = Arc::new(Mutex::new(Vec::new()));
    let ended = Arc::new(Mutex::new(0u32));
    let config = cloud_config();
    let task = tokio::spawn({
        let built = built.clone();
        let ended = ended.clone();
        async move {
            supervise(
                shutdown,
                key_loader(path),
                PAIRING_POLL_INTERVAL,
                RELAY_SETTLE,
                move |key, mut child| {
                    let transport = config.relay_transport(None, &key)?;
                    built.lock().unwrap().push(transport.password);
                    let ended = ended.clone();
                    Some(async move {
                        while !*child.borrow() {
                            if child.changed().await.is_err() {
                                break;
                            }
                        }
                        *ended.lock().unwrap() += 1;
                    })
                },
            )
            .await
        }
    });
    (built, ended, task)
}

#[tokio::test(start_paused = true)]
async fn relay_run_ends_on_key_change_and_rebuilds_with_new_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    write_pairing(&path, Some("key-a"));
    let (_tx, rx) = watch::channel(false);
    let (built, ended, task) = start(path.clone(), rx);
    settle(1).await;
    assert_eq!(*built.lock().unwrap(), vec!["key-a".to_string()]);

    write_pairing(&path, Some("key-b"));
    // One poll ends the run.
    settle(PAIRING_POLL_INTERVAL.as_secs() as u32 + 1).await;
    assert_eq!(*ended.lock().unwrap(), 1);
    // The next iteration dials with the new key.
    settle(RELAY_SETTLE.as_secs() as u32 + 1).await;
    assert_eq!(
        *built.lock().unwrap(),
        vec!["key-a".to_string(), "key-b".to_string()]
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn relay_run_ends_on_unpair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    write_pairing(&path, Some("key-a"));
    let (_tx, rx) = watch::channel(false);
    let (built, ended, task) = start(path.clone(), rx);
    settle(1).await;

    write_pairing(&path, None);
    settle(PAIRING_POLL_INTERVAL.as_secs() as u32 + 1).await;
    assert_eq!(*ended.lock().unwrap(), 1);
    // Unpaired: no new run is built.
    settle(3 * RELAY_SETTLE.as_secs() as u32).await;
    assert_eq!(built.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn gs_bridge_same() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    write_pairing(&path, Some("key-a"));
    let flag = dir.path().join("uplink-active");
    let (_tx, rx) = watch::channel(false);
    let config = cloud_config();
    let runs = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn({
        let runs = runs.clone();
        async move {
            supervise(
                rx,
                key_loader(path.clone()),
                PAIRING_POLL_INTERVAL,
                RELAY_SETTLE,
                move |key, child| {
                    let transport = config.relay_transport(None, &key)?;
                    let runs = runs.clone();
                    let flag = flag.clone();
                    Some(async move {
                        let password = transport.password.clone();
                        let mut bridge = crate::CloudRelayBridge::new("dev-1", None, transport)
                            .with_flag_path(flag);
                        bridge.run(child).await;
                        runs.lock().unwrap().push(password);
                    })
                },
            )
            .await
        }
    });
    settle(1).await;
    write_pairing(&dir.path().join("pairing.json"), Some("key-b"));
    settle(PAIRING_POLL_INTERVAL.as_secs() as u32 + 1).await;
    // The bridge run built with the old key returned on the pairing change.
    assert_eq!(*runs.lock().unwrap(), vec!["key-a".to_string()]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn parent_shutdown_ends_the_run_and_the_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairing.json");
    write_pairing(&path, Some("key-a"));
    let (tx, rx) = watch::channel(false);
    let (_built, ended, task) = start(path, rx);
    settle(1).await;
    tx.send(true).unwrap();
    settle(2).await;
    assert_eq!(*ended.lock().unwrap(), 1);
    assert!(task.is_finished());
}
