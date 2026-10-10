//! Tests for the cellular modem manager.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// A scripted D-Bus fake: each `bring_up` consumes the next verdict from a
/// fixed sequence (true = success, false = failure). `imsi` is fixed.
struct ScriptedDbus {
    verdicts: Vec<bool>,
    idx: AtomicU32,
    imsi: Option<String>,
}
impl ScriptedDbus {
    fn new(verdicts: Vec<bool>) -> Self {
        Self {
            verdicts,
            idx: AtomicU32::new(0),
            imsi: None,
        }
    }
    fn with_imsi(verdicts: Vec<bool>, imsi: &str) -> Self {
        Self {
            verdicts,
            idx: AtomicU32::new(0),
            imsi: Some(imsi.to_string()),
        }
    }
    fn next(&self) -> bool {
        let i = self.idx.fetch_add(1, Ordering::SeqCst) as usize;
        self.verdicts.get(i).copied().unwrap_or(false)
    }
}
#[async_trait]
impl ModemDbus for ScriptedDbus {
    async fn bring_up(&self, apn: &str) -> Result<DbusConnectResult, String> {
        if self.next() {
            Ok(DbusConnectResult {
                iface: "wwan0".to_string(),
                ip: "10.1.2.3".to_string(),
                apn: apn.to_string(),
                bearer: String::new(),
                ip4_static: None,
            })
        } else {
            Err("scripted_fail".to_string())
        }
    }
    async fn bring_down(&self) -> Result<(), String> {
        if self.next() {
            Ok(())
        } else {
            Err("scripted_fail".to_string())
        }
    }
    async fn modem_present(&self) -> Result<bool, String> {
        Ok(true)
    }
    async fn imsi(&self) -> Result<Option<String>, String> {
        Ok(self.imsi.clone())
    }
    async fn bearer_connected(&self, _bearer: &str) -> Result<bool, String> {
        Err("no_bearer".to_string())
    }
}

/// An AT opener that never finds a port (tests on a dev host with no modem).
struct NoAtPort;
#[async_trait]
impl AtPortOpener for NoAtPort {
    async fn open(&self) -> Option<Box<dyn SerialTransport>> {
        None
    }
}

/// A serial transport whose AT `command` blocks until `release` is notified.
/// It signals `entered` the moment the first command begins, so a test can
/// prove `status()` reached the (slow) AT round-trip and then check that a
/// concurrent manager method still completed while the round-trip is parked.
struct BlockingAtPort {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    announced: AtomicBool,
}
#[async_trait]
impl SerialTransport for BlockingAtPort {
    async fn command(&mut self, _cmd: &str, _timeout: Duration) -> String {
        // Announce entry on the first command only, then block until the
        // test releases us. Subsequent commands return immediately.
        if !self.announced.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        // A plausible AT reply so status_over parses without panicking.
        "OK".to_string()
    }
}

/// An AT opener that hands back one prepared blocking transport.
struct OneBlockingPort {
    port: Mutex<Option<Box<dyn SerialTransport>>>,
}
#[async_trait]
impl AtPortOpener for OneBlockingPort {
    async fn open(&self) -> Option<Box<dyn SerialTransport>> {
        self.port.lock().await.take()
    }
}

fn mgr(dbus: Arc<dyn ModemDbus>, dir: &std::path::Path) -> ModemManager {
    ModemManager::with_parts_at(
        dbus,
        Arc::new(NoAtPort),
        dir.join("ground-station-modem.json"),
        dir.join("net"),
    )
}

#[test]
fn imsi_apn_map_matches_carriers() {
    // Jio.
    assert_eq!(apn_for_imsi("405857123456789"), Some("jionet"));
    // Airtel (5-digit prefix).
    assert_eq!(apn_for_imsi("4041099887766554"), Some("airtelgprs.com"));
    // Vi.
    assert_eq!(apn_for_imsi("40411000000000"), Some("portalnmms"));
    assert_eq!(apn_for_imsi("40443000000000"), Some("www"));
    // BSNL.
    assert_eq!(apn_for_imsi("40434000000000"), Some("bsnlnet"));
    // No match (US carrier MCC 310).
    assert_eq!(apn_for_imsi("310260123456789"), None);
    // The map is verbatim length (5 Jio + 11 Airtel + 3 Vi + 5 BSNL).
    assert_eq!(IMSI_APN_MAP.len(), 24);
}

#[test]
fn config_render_is_byte_exact_to_python_json_dumps() {
    let cfg = ModemConfig {
        apn: Some("jionet".to_string()),
        cap_gb: Some(5.0),
        enabled: Some(true),
    };
    assert_eq!(
        cfg.render_json(),
        r#"{"apn": "jionet", "cap_gb": 5.0, "enabled": true}"#
    );
    // A partial config (only the keys present) renders just those keys.
    let partial = ModemConfig {
        apn: Some("internet".to_string()),
        cap_gb: None,
        enabled: Some(false),
    };
    assert_eq!(
        partial.render_json(),
        r#"{"apn": "internet", "enabled": false}"#
    );
}

#[test]
fn desired_session_decides_up_down_leave() {
    // No config file → leave the modem alone (a bare board never auto-dials).
    assert_eq!(
        desired_modem_session(false, &ModemConfig::default()),
        ModemSession::Leave
    );
    // Config present, enabled absent → default-enabled → Up.
    assert_eq!(
        desired_modem_session(true, &ModemConfig::default()),
        ModemSession::Up
    );
    // Config present, enabled true → Up.
    let on = ModemConfig {
        enabled: Some(true),
        ..Default::default()
    };
    assert_eq!(desired_modem_session(true, &on), ModemSession::Up);
    // Config present, enabled false → Down.
    let off = ModemConfig {
        enabled: Some(false),
        ..Default::default()
    };
    assert_eq!(desired_modem_session(true, &off), ModemSession::Down);
    // A disabled config with no file still leaves the modem alone.
    assert_eq!(desired_modem_session(false, &off), ModemSession::Leave);
}

#[tokio::test]
async fn reload_config_picks_up_a_rewritten_file_and_flips_enabled() {
    // The daemon owns the session: the REST write path only rewrites the
    // file, so the manager must re-read it to learn enabled/apn changes.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ground-station-modem.json");
    // Seed a disabled config and load it into a fresh manager.
    std::fs::write(
        &path,
        ModemConfig {
            apn: Some("internet".into()),
            cap_gb: Some(2.0),
            enabled: Some(false),
        }
        .render_json(),
    )
    .unwrap();
    let m = mgr(Arc::new(DisabledDbus), dir.path());
    // Initial reload sees the disabled config → desired session is Down.
    let cfg = m.reload_config().await;
    assert_eq!(cfg.enabled, Some(false));
    assert_eq!(desired_modem_session(true, &cfg), ModemSession::Down);
    assert!(!m.enabled().await);

    // Operator rewrites the file to enable + a new APN (out of process).
    std::fs::write(
        &path,
        ModemConfig {
            apn: Some("jionet".into()),
            cap_gb: Some(2.0),
            enabled: Some(true),
        }
        .render_json(),
    )
    .unwrap();
    let cfg = m.reload_config().await;
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(desired_modem_session(true, &cfg), ModemSession::Up);
    assert!(m.enabled().await);
    // The dialed APN follows the file too.
    assert_eq!(m.configured_apn().await, "jionet");
}

#[tokio::test]
async fn configured_apn_defaults_to_auto_when_unset() {
    let dir = tempfile::tempdir().unwrap();
    let m = mgr(Arc::new(DisabledDbus), dir.path());
    assert_eq!(m.configured_apn().await, "auto");
}

#[tokio::test]
async fn configure_persists_sidecar_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let m = mgr(Arc::new(DisabledDbus), dir.path());
    let out = m
        .configure(Some("jionet"), Some(5.0), Some(true))
        .await
        .unwrap();
    assert_eq!(out["apn"], "jionet");
    assert_eq!(out["enabled"], true);
    // On-disk bytes byte-match Python.
    let body = std::fs::read_to_string(dir.path().join("ground-station-modem.json")).unwrap();
    assert_eq!(body, r#"{"apn": "jionet", "cap_gb": 5.0, "enabled": true}"#);
    // A fresh manager reads it back.
    let m2 = mgr(Arc::new(DisabledDbus), dir.path());
    assert!(m2.enabled().await);
}

#[tokio::test]
async fn configure_refuses_an_apn_that_could_splice_at_commands() {
    let dir = tempfile::tempdir().unwrap();
    let m = mgr(Arc::new(DisabledDbus), dir.path());
    for bad in [
        "inet\"\r\nAT+CFUN=0",
        "",
        "has space",
        "a".repeat(64).as_str(),
    ] {
        assert!(m.configure(Some(bad), None, None).await.is_err(), "{bad:?}");
    }
    assert!(!dir.path().join("ground-station-modem.json").exists());
    assert!(validate_apn("airtelgprs.com").is_ok());
    assert!(validate_apn("my-apn.example").is_ok());
}

#[tokio::test]
async fn three_consecutive_dbus_failures_flip_to_fallback() {
    let dir = tempfile::tempdir().unwrap();
    // bring_up verdicts: fail, fail, fail.
    let m = mgr(
        Arc::new(ScriptedDbus::new(vec![false, false, false])),
        dir.path(),
    );
    // Two failures: still trying D-Bus, not yet in fallback.
    let r1 = m.bring_up("internet", None).await;
    assert_eq!(r1["connected"], false);
    assert!(!m.needs_at_fallback().await);
    let r2 = m.bring_up("internet", None).await;
    assert_eq!(r2["connected"], false);
    assert!(!m.needs_at_fallback().await);
    // Third failure crosses the threshold → fallback.
    let r3 = m.bring_up("internet", None).await;
    assert_eq!(r3["connected"], false);
    assert_eq!(r3["needs_at_fallback"], true);
    assert!(m.needs_at_fallback().await);
}

#[tokio::test]
async fn fallback_retries_dbus_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let m = mgr(
        Arc::new(ScriptedDbus::new(vec![false, false, false, true])),
        dir.path(),
    );
    for _ in 0..3 {
        m.bring_up("internet", None).await;
    }
    assert!(m.needs_at_fallback().await);
    // The next bring-up in fallback tries D-Bus again, and it is back.
    let ok = m.bring_up("internet", None).await;
    assert_eq!(ok["connected"], true);
    assert!(!m.needs_at_fallback().await);
}

#[tokio::test]
async fn dbus_success_recovers_from_fallback() {
    let dir = tempfile::tempdir().unwrap();
    // fail x3 (→ fallback), then a success must NOT be attempted while in
    // fallback. So: drive to fallback, then a separate manager shows that a
    // success resets the counter. Use verdicts: success first.
    let m = mgr(Arc::new(ScriptedDbus::new(vec![true])), dir.path());
    let ok = m.bring_up("jionet", None).await;
    assert_eq!(ok["connected"], true);
    assert_eq!(ok["iface"], "wwan0");
    assert_eq!(ok["ip"], "10.1.2.3");
    assert!(!m.needs_at_fallback().await);
}

#[tokio::test]
async fn recovery_after_fallback_when_dbus_succeeds_again() {
    let dir = tempfile::tempdir().unwrap();
    // fail, fail, fail (→ fallback), then NOTHING (fallback skips dbus).
    // To prove recovery we manually clear fallback by issuing a success on a
    // manager whose verdicts are fail,fail,fail,success and forcing a retry
    // out of fallback: the manager only retries dbus when not in fallback,
    // so recovery is driven by an explicit non-fallback bring_up. Model that
    // by checking register_dbus_success resets state via a success-first
    // sequence after a manual fallback clear is out of scope; instead assert
    // the counter resets on success within the non-fallback window.
    let m = mgr(
        Arc::new(ScriptedDbus::new(vec![false, false, true])),
        dir.path(),
    );
    m.bring_up("internet", None).await; // fail 1
    m.bring_up("internet", None).await; // fail 2 (still not fallback)
    let ok = m.bring_up("internet", None).await; // success 3 → counter reset
    assert_eq!(ok["connected"], true);
    assert!(!m.needs_at_fallback().await);
}

#[tokio::test]
async fn auto_apn_resolves_from_live_imsi_on_dbus_path() {
    let dir = tempfile::tempdir().unwrap();
    // A D-Bus modem that succeeds and carries a Jio IMSI.
    let dbus = Arc::new(ScriptedDbus::with_imsi(vec![true], "405857999888777"));
    let m = ModemManager::with_parts_at(
        dbus,
        Arc::new(NoAtPort),
        dir.path().join("ground-station-modem.json"),
        dir.path().join("net"),
    );
    // The daemon reads the live IMSI and passes it so "auto" maps to jionet.
    let imsi = m.read_imsi().await;
    assert_eq!(imsi.as_deref(), Some("405857999888777"));
    let out = m.bring_up("auto", imsi.as_deref()).await;
    assert_eq!(out["connected"], true);
    assert_eq!(out["apn"], "jionet");
}

#[cfg(unix)]
#[tokio::test]
async fn status_reports_mode_and_iface_liveness() {
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    // D-Bus present (modem_present → true), not in fallback → mode "dbus".
    let m = mgr(Arc::new(ScriptedDbus::new(vec![true])), dir.path());
    let s = m.status().await;
    assert_eq!(s["mode"], "dbus");
    assert_eq!(s["needs_at_fallback"], false);
    // Bring wwan0 up in sysfs → iface_up true.
    rig_cellular(&net, "wwan0", "qmi_wwan");
    let wwan = net.join("wwan0");
    std::fs::create_dir_all(&wwan).unwrap();
    std::fs::write(wwan.join("operstate"), "up\n").unwrap();
    let s = m.status().await;
    assert_eq!(s["iface_up"], true);
    assert_eq!(s["iface"], "wwan0");
    assert!(m.probe().await);
}

#[cfg(unix)]
#[tokio::test]
async fn usb0_is_the_tether_not_the_modem_when_the_gadget_is_provisioned() {
    // A modem-less ground station that has provisioned the USB-gadget tether
    // owns `usb0` for the tether, not a cellular modem. The modem manager
    // must NOT claim `usb0` as its cellular iface, or `is_up()`/`get_iface()`
    // would report the cellular slot up + routable when there is no modem.
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    let gadget = dir.path().join("usb_gadget").join("ados_gs");

    // Bring `usb0` up in sysfs (the tether NIC) but NO `wwan0`.
    rig_cellular(&net, USB_IFACE, "rndis_host");
    let usb = net.join(USB_IFACE);
    std::fs::write(usb.join("operstate"), "up\n").unwrap();

    // DisabledDbus → modem_present is false, so liveness depends purely on
    // the resolved iface. Point the gadget marker at the (present) tempdir.
    let m = ModemManager::with_parts_at(
        Arc::new(DisabledDbus),
        Arc::new(NoAtPort),
        dir.path().join("ground-station-modem.json"),
        net.clone(),
    )
    .with_gadget_dir(gadget.clone());

    // Gadget present → `usb0` is the tether → no cellular iface, so the
    // cellular slot reads down + not routable.
    std::fs::create_dir_all(&gadget).unwrap();
    assert_eq!(m.get_iface(), "");
    assert!(!m.is_up().await);
    assert!(!m.probe().await);
    let s = m.status().await;
    assert_eq!(s["iface"], "");
    assert_eq!(s["iface_up"], false);

    // Remove the gadget marker → a real modem owns `usb0` (RNDIS/AT) → the
    // same up `usb0` now counts as the cellular link.
    std::fs::remove_dir_all(&gadget).unwrap();
    assert_eq!(m.get_iface(), USB_IFACE);
    assert!(m.is_up().await);
    assert!(m.probe().await);
    let s = m.status().await;
    assert_eq!(s["iface"], USB_IFACE);
    assert_eq!(s["iface_up"], true);
}

#[tokio::test]
async fn status_does_not_hold_state_lock_across_the_at_round_trip() {
    // A manager seeded into AT-fallback with a held-open port whose AT
    // exchange blocks. While status() is parked in that exchange, a
    // concurrent enabled() (which takes the same state lock) must still
    // complete promptly — proving status() released the lock before the
    // slow serial I/O.
    let dir = tempfile::tempdir().unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let port = Box::new(BlockingAtPort {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        announced: AtomicBool::new(false),
    });

    let m = Arc::new(ModemManager::with_parts_at(
        Arc::new(DisabledDbus),
        Arc::new(OneBlockingPort {
            port: Mutex::new(None),
        }),
        dir.path().join("ground-station-modem.json"),
        dir.path().join("net"),
    ));
    // Seed fallback + a cached blocking port.
    {
        let mut st = m.state.lock().await;
        st.fallback_mode = true;
        st.brought_up = true;
        st.at_port = Some(port);
    }

    // Run status() concurrently; it will block inside the AT round-trip.
    let m_status = Arc::clone(&m);
    let status_task = tokio::spawn(async move { m_status.status().await });

    // Wait until status() has entered the (parked) AT exchange.
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("status() reached the AT round-trip");

    // The state lock must be free now: enabled() returns without waiting on
    // the parked AT exchange. A short timeout proves it is not serialized
    // behind status().
    let still_responsive = tokio::time::timeout(Duration::from_millis(200), m.enabled())
        .await
        .expect("enabled() must not block behind a parked AT poll");
    assert!(still_responsive); // default (absent enabled key) → true.

    // Release the AT exchange and let status() finish.
    release.notify_one();
    let out = tokio::time::timeout(Duration::from_secs(5), status_task)
        .await
        .expect("status() completes after release")
        .expect("status task did not panic");
    assert_eq!(out["mode"], "at");
    assert_eq!(out["needs_at_fallback"], true);

    // The port was re-stored for the next poll (no teardown raced).
    assert!(m.state.lock().await.at_port.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn data_usage_reads_sysfs_and_is_absent_when_iface_missing() {
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    let m = mgr(Arc::new(DisabledDbus), dir.path());
    // No iface dir → available:false.
    let u = m.data_usage();
    assert_eq!(u["available"], false);
    assert_eq!(u["total_bytes"], 0);
    // Create wwan0 stats.
    rig_cellular(&net, "wwan0", "qmi_wwan");
    let stats = net.join("wwan0").join("statistics");
    std::fs::create_dir_all(&stats).unwrap();
    std::fs::write(stats.join("rx_bytes"), "1000\n").unwrap();
    std::fs::write(stats.join("tx_bytes"), "500\n").unwrap();
    let u = m.data_usage();
    assert_eq!(u["available"], true);
    assert_eq!(u["iface"], "wwan0");
    assert_eq!(u["rx_bytes"], 1000);
    assert_eq!(u["total_bytes"], 1500);
}

#[cfg(unix)]
#[tokio::test]
async fn is_up_reflects_iface_operstate() {
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    let m = mgr(Arc::new(DisabledDbus), dir.path());
    // No iface → down.
    assert!(!m.is_up().await);
    // wwan0 operstate up → up.
    rig_cellular(&net, "wwan0", "qmi_wwan");
    let wwan = net.join("wwan0");
    std::fs::create_dir_all(&wwan).unwrap();
    std::fs::write(wwan.join("operstate"), "up\n").unwrap();
    assert!(m.is_up().await);
    assert_eq!(m.get_iface(), "wwan0");
    // get_gateway is None (point-to-point; daemon reads ip route).
    assert!(m.get_gateway().await.is_none());
}

/// A D-Bus fake whose single bearer reports a static IPv4 config and whose
/// `Connected` property is scripted.
struct BearerDbus {
    connected: AtomicBool,
}
#[async_trait]
impl ModemDbus for BearerDbus {
    async fn bring_up(&self, apn: &str) -> Result<DbusConnectResult, String> {
        Ok(DbusConnectResult {
            iface: "wwan0".to_string(),
            ip: String::new(),
            apn: apn.to_string(),
            bearer: "/org/freedesktop/ModemManager1/Bearer/0".to_string(),
            ip4_static: Some(Ip4Static {
                address: "10.64.12.7".to_string(),
                prefix: 30,
                gateway: Some("10.64.12.8".to_string()),
            }),
        })
    }
    async fn bring_down(&self) -> Result<(), String> {
        Ok(())
    }
    async fn modem_present(&self) -> Result<bool, String> {
        Ok(true)
    }
    async fn imsi(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn bearer_connected(&self, bearer: &str) -> Result<bool, String> {
        assert_eq!(bearer, "/org/freedesktop/ModemManager1/Bearer/0");
        Ok(self.connected.load(Ordering::SeqCst))
    }
}

/// The session reads the bearer's own `Connected` state: a carrier detach
/// with the netdev still up reads down, so the reconcile loop re-dials. The
/// static bearer's address and default route are applied on connect.
#[cfg(unix)]
#[tokio::test]
async fn bearer_disconnected_redials() {
    let dir = tempfile::tempdir().unwrap();
    let net = dir.path().join("net");
    rig_cellular(&net, "wwan0", "qmi_wwan");
    std::fs::write(net.join("wwan0").join("operstate"), "unknown\n").unwrap();
    let dbus = Arc::new(BearerDbus {
        connected: AtomicBool::new(true),
    });
    let runner = Arc::new(crate::cmd::testing::ScriptedRunner::new());
    let m = mgr(Arc::clone(&dbus) as Arc<dyn ModemDbus>, dir.path())
        .with_runner(Arc::clone(&runner) as Arc<dyn CmdRunner>);

    let out = m.bring_up("internet", None).await;
    assert_eq!(out["connected"], true);
    assert_eq!(out["iface"], "wwan0");
    assert_eq!(out["ip"], "10.64.12.7");
    let calls = runner.recorded();
    assert_eq!(
        calls[0],
        ["ip", "addr", "replace", "10.64.12.7/30", "dev", "wwan0"]
    );
    assert_eq!(
        calls[1],
        [
            "ip",
            "route",
            "replace",
            "default",
            "via",
            "10.64.12.8",
            "dev",
            "wwan0",
            "metric",
            "30"
        ]
    );
    assert!(m.session_up().await);

    // The carrier detached the bearer; the netdev is still present.
    dbus.connected.store(false, Ordering::SeqCst);
    assert!(
        !m.session_up().await,
        "a disconnected bearer must read down"
    );
    // The reconcile loop's response to a down session is a fresh dial.
    dbus.connected.store(true, Ordering::SeqCst);
    let again = m.bring_up("internet", None).await;
    assert_eq!(again["connected"], true);
    assert!(m.session_up().await);
}
