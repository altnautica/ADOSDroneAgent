use serde_json::{json, Value};

use super::*;
use ados_gnss_stream::base::Llh;
use ados_gnss_stream::fixtures::{self, Pvt};
use ados_gnss_stream::stream::Splitter;

const S: u64 = 1_000_000_000;

fn cfg(sources: &[&str], base: BaseMode) -> GnssConfig {
    GnssConfig {
        receivers: sources
            .iter()
            .enumerate()
            .map(|(i, s)| ReceiverCfg {
                id: format!("gnss{i}"),
                source: Source::parse(s).unwrap(),
                model: None,
                port: valset::RxPort::Uart1,
            })
            .collect(),
        pps_device: None,
        base,
        survey_min_dur_s: 300,
        survey_accuracy_m: 2.0,
        heading_enabled: true,
    }
}

fn feed(svc: &mut Service, idx: usize, bytes: &[u8], now: u64) -> Vec<Vec<u8>> {
    Splitter::new()
        .push(bytes)
        .into_iter()
        .filter_map(|f| svc.on_frame(idx, f, now))
        .collect()
}

/// The GET /api/v1/gnss example from the REST contract, minus `present`,
/// plus `version`.
fn reference() -> Value {
    json!({"version":1,
     "receivers":[{"id":"gnss0","source":"hub","model":"u-blox M10","fix":"3d","sats":14,"hdop":0.8,"lat_deg":12.9716,"lon_deg":77.5946,"h_ellipsoid_m":920.4,"h_msl_m":1006.1,"rtk":"none","pps_locked":true}],
     "base":{"mode":"survey","state":"surveying","elapsed_s":120,"min_dur_s":300,"accuracy_m":3.1,"target_accuracy_m":2.0,"fixed":null,"rtcm_out":{"msgs_per_s":0.0,"bytes_total":0,"targets":0}},
     "heading":{"available":false,"deg":null,"accuracy_deg":null,"baseline_m":null}})
}

fn same_keys(ours: &Value, theirs: &Value, at: &str) {
    match (ours, theirs) {
        (Value::Object(a), Value::Object(b)) => {
            let ka: Vec<&String> = a.keys().collect();
            let kb: Vec<&String> = b.keys().collect();
            assert_eq!(ka, kb, "keys at {at}");
            for (k, v) in a {
                same_keys(v, &b[k], &format!("{at}.{k}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for v in a {
                same_keys(v, &b[0], &format!("{at}[]"));
            }
        }
        _ => {}
    }
}

#[test]
fn status_has_exactly_the_contract_fields() {
    let mut svc = Service::new(&cfg(&["udp:7800", "udp:7801"], BaseMode::Off), 0);
    feed(&mut svc, 0, &fixtures::nav_pvt(Pvt::default()), S);
    feed(
        &mut svc,
        0,
        &fixtures::mon_ver("00190000", &["MOD=ZED-F9P"]),
        S,
    );
    let status = svc.status(S, 0);
    let v = serde_json::to_value(&status).unwrap();
    same_keys(&v, &reference(), "");
    assert!(serde_json::to_string(&status)
        .unwrap()
        .starts_with(r#"{"version":1,"receivers":[{"id":"gnss0","source":"udp:7800""#));
    let r = &v["receivers"][0];
    assert_eq!(r["fix"], "3d");
    assert_eq!(r["rtk"], "none");
    assert_eq!(r["model"], "u-blox ZED-F9P");
    assert_eq!(v["receivers"][1]["fix"], "none");
    assert_eq!(v["receivers"][1]["hdop"], Value::Null);
    assert_eq!(v["base"]["mode"], "off");
    assert_eq!(v["base"]["state"], "off");
}

#[test]
fn fixed_base_is_commanded_on_attach_and_confirmed_by_its_own_ack() {
    let pos = Llh {
        lat_deg: 1.0,
        lon_deg: 2.0,
        h_ellipsoid_m: 3.0,
    };
    let mut svc = Service::new(&cfg(&["udp:7800"], BaseMode::Fixed(pos)), 0);
    // Not attached yet: nothing commanded, nothing claimed.
    let v = serde_json::to_value(svc.status(0, 0)).unwrap();
    assert_eq!(
        (v["base"]["mode"].clone(), v["base"]["state"].clone()),
        (json!("fixed"), json!("off"))
    );
    svc.tick(60 * S);
    assert_eq!(svc.base_state(), BaseState::Off);

    let (tx, mut rx) = mpsc::channel(8);
    svc.on_attach(0, tx, S);
    assert_eq!(rx.try_recv().unwrap(), valset::poll_mon_ver());
    assert_eq!(
        rx.try_recv().unwrap(),
        valset::nav_outputs(valset::RxPort::Uart1, true, false)
    );
    assert_eq!(
        rx.try_recv().unwrap(),
        valset::fixed(valset::RxPort::Uart1, 1.0, 2.0, 3.0)
    );
    // The first ACK belongs to the output enables, not the base command.
    feed(&mut svc, 0, &fixtures::ack(true, 0x06, 0x8A), S);
    assert_eq!(svc.base_state(), BaseState::Off);
    feed(&mut svc, 0, &fixtures::ack(true, 0x06, 0x8A), S);
    let v = serde_json::to_value(svc.status(S, 0)).unwrap();
    assert_eq!(v["base"]["state"], "fixed");
    assert_eq!(
        v["base"]["fixed"],
        json!({"lat_deg": 1.0, "lon_deg": 2.0, "h_ellipsoid_m": 3.0})
    );
}

#[test]
fn a_refused_output_enable_does_not_fail_the_base() {
    let mode = BaseMode::Survey {
        min_dur_s: 10,
        accuracy_m: 1.0,
    };
    let mut svc = Service::new(&cfg(&["udp:7800"], mode), 0);
    let (tx, _rx) = mpsc::channel(8);
    svc.on_attach(0, tx, S);
    assert_eq!(svc.base_state(), BaseState::Surveying);
    feed(&mut svc, 0, &fixtures::ack(false, 0x06, 0x8A), S);
    assert_eq!(svc.base_state(), BaseState::Surveying);
    feed(&mut svc, 0, &fixtures::ack(false, 0x06, 0x8A), S);
    assert_eq!(svc.base_state(), BaseState::Failed);
}

#[test]
fn rtcm_is_forwarded_and_counted_only_while_a_base() {
    let mut svc = Service::new(&cfg(&["udp:7800", "udp:7801"], BaseMode::Off), 0);
    let frame = fixtures::rtcm(1005, 19);
    assert!(feed(&mut svc, 0, &frame, S).is_empty());

    svc.set_base(
        BaseMode::Survey {
            min_dur_s: 60,
            accuracy_m: 1.0,
        },
        S,
    )
    .unwrap();
    let mut two = frame.clone();
    two.extend_from_slice(&fixtures::rtcm(1074, 40));
    let out = feed(&mut svc, 0, &two, 2 * S);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], frame);
    // A rover's RTCM is not forwarded.
    assert!(feed(&mut svc, 1, &frame, 2 * S).is_empty());
    let base = svc.base_out(2 * S, 4);
    assert_eq!(base.rtcm_out.bytes_total, two.len() as u64);
    assert!((base.rtcm_out.msgs_per_s - 1.0).abs() < 1e-9);
    assert_eq!(base.rtcm_out.targets, 4);
}

#[test]
fn hub_base_fails_and_writes_reach_attached_ports() {
    let svc = Service::new(
        &cfg(
            &["hub"],
            BaseMode::Survey {
                min_dur_s: 10,
                accuracy_m: 1.0,
            },
        ),
        0,
    );
    assert_eq!(svc.base_state(), BaseState::Failed);
    assert!(svc.base_reason().unwrap().contains("sensor hub"));

    let mut svc = Service::new(&cfg(&["udp:7800", "udp:7801"], BaseMode::Off), 0);
    assert_eq!(svc.set_base(BaseMode::Off, 0), Ok(()));
    let uart = valset::RxPort::Uart1;
    let (tx, mut rx) = mpsc::channel(8);
    svc.on_attach(0, tx, 0);
    assert_eq!(rx.try_recv().unwrap(), valset::poll_mon_ver());
    assert_eq!(
        rx.try_recv().unwrap(),
        valset::nav_outputs(uart, true, false)
    );
    // Base off is stated explicitly, so a receiver left in base mode stops.
    assert_eq!(rx.try_recv().unwrap(), valset::off(uart));
    svc.set_base(
        BaseMode::Survey {
            min_dur_s: 10,
            accuracy_m: 1.0,
        },
        S,
    )
    .unwrap();
    assert_eq!(rx.try_recv().unwrap(), valset::survey_in(uart, 10, 1.0));
    // Re-attach re-sends the whole configuration with the active base command.
    let (tx, mut rx) = mpsc::channel(8);
    svc.on_attach(0, tx, 2 * S);
    assert_eq!(rx.try_recv().unwrap(), valset::poll_mon_ver());
    assert_eq!(
        rx.try_recv().unwrap(),
        valset::nav_outputs(uart, true, false)
    );
    assert_eq!(rx.try_recv().unwrap(), valset::survey_in(uart, 10, 1.0));
    assert!(rx.try_recv().is_err());
    // A heading rover gets NAV-RELPOSNED and no base command.
    let (tx, mut rx) = mpsc::channel(8);
    svc.on_attach(1, tx, 2 * S);
    assert_eq!(rx.try_recv().unwrap(), valset::poll_mon_ver());
    assert_eq!(
        rx.try_recv().unwrap(),
        valset::nav_outputs(uart, false, true)
    );
    assert!(rx.try_recv().is_err());

    let mut none = Service::new(&cfg(&[], BaseMode::Off), 0);
    assert_eq!(
        none.set_base(BaseMode::Off, 0).unwrap_err().0,
        E_NO_RECEIVER
    );
}
