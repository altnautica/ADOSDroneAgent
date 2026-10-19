use std::sync::{Arc, Mutex};

use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

use super::*;
use crate::frame::{Frame, Kind};

#[derive(Clone, Default)]
struct FakePins(Arc<Mutex<Vec<Mode>>>);

impl ModePins for FakePins {
    fn set_mode(&mut self, mode: Mode) -> io::Result<()> {
        self.0.lock().unwrap().push(mode);
        Ok(())
    }
}

const PID: [u8; 7] = [0x00, 0x22, 0x00, 0x0B, 0x0E, 0x0A, 0x14];
const OPEN_WRITES: [u8; 15] = [
    0xC1, 0x80, 0x07, // read PID
    0xC0, 0x00, 0x05, 0xFF, 0xFF, 0x00, 0x62, 0x23, // ADDH..REG1, 10 dBm
    0xC0, 0x06, 0x01, 0xC3, // REG3: RSSI byte, fixed point, WOR 2000 ms
];

fn cfg(band: &str) -> LoraConfig {
    let mut c = LoraConfig::default();
    c.e22.module_band = band.into();
    c
}

fn frame(node: u16, text: &[u8]) -> Vec<u8> {
    Frame {
        node_id: node,
        kind: Kind::Text,
        payload: text.to_vec(),
    }
    .encode()
    .unwrap()
}

async fn open_replies(module: &mut DuplexStream) {
    let mut r = vec![0xC1, 0x80, 0x07];
    r.extend_from_slice(&PID);
    r.extend_from_slice(&[0xC1, 0x00, 0x05, 0xFF, 0xFF, 0x00, 0x62, 0x23]);
    r.extend_from_slice(&[0xC1, 0x06, 0x01, 0xC3]);
    module.write_all(&r).await.unwrap();
}

async fn read_n(module: &mut DuplexStream, n: usize) -> Vec<u8> {
    let mut v = vec![0; n];
    module.read_exact(&mut v).await.unwrap();
    v
}

async fn wired(band: &str) -> (E22<DuplexStream, FakePins>, DuplexStream, FakePins) {
    let (host, mut module) = duplex(4096);
    let pins = FakePins::default();
    open_replies(&mut module).await;
    let e = E22::with_io(host, Some(pins.clone()), &cfg(band))
        .await
        .unwrap();
    assert_eq!(read_n(&mut module, OPEN_WRITES.len()).await, OPEN_WRITES);
    (e, module, pins)
}

async fn strapped(freq: Option<f64>) -> (E22<DuplexStream, FakePins>, DuplexStream) {
    let (host, module) = duplex(4096);
    let mut c = cfg("850-930");
    c.freq_mhz = freq;
    (E22::with_io(host, None, &c).await.unwrap(), module)
}

#[tokio::test(start_paused = true)]
async fn open_reads_the_pid_and_writes_the_configuration() {
    let (e, _module, pins) = wired("850-930").await;
    assert_eq!(e.pid(), Some(PID));
    assert_eq!(e.caps(), &ModuleCaps::e22_900());
    assert_eq!(*pins.0.lock().unwrap(), vec![Mode::Config, Mode::Normal]);
    let (e, _module, _) = wired("410-493").await;
    assert_eq!(e.caps(), &ModuleCaps::e22_400());
}

#[tokio::test(start_paused = true)]
async fn open_refuses_other_air_rates_and_bands() {
    let (host, _module) = duplex(64);
    let mut c = cfg("850-930");
    c.air_rate = "9.6k".into();
    let err = E22::<_, FakePins>::with_io(host, None, &c)
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let (host, _module) = duplex(64);
    let err = E22::<_, FakePins>::with_io(host, None, &cfg("863-928"))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[tokio::test(start_paused = true)]
async fn open_fails_on_a_wrong_format_reply() {
    let (host, mut module) = duplex(4096);
    module.write_all(&[0xFF, 0xFF, 0xFF]).await.unwrap();
    let err = E22::with_io(host, Some(FakePins::default()), &cfg("850-930"))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn send_prefixes_the_broadcast_target_and_channel() {
    let (mut e, mut module) = strapped(None).await;
    assert_eq!(e.caps().power_steps_dbm, vec![22]);
    let bytes = frame(7, b"hi");
    e.send(&TxFrame {
        channel_mhz: 865.125,
        power_dbm: 22,
        bytes: bytes.clone(),
    })
    .await
    .unwrap();
    let mut want = vec![0xFF, 0xFF, 15];
    want.extend_from_slice(&bytes);
    assert_eq!(read_n(&mut module, want.len()).await, want);

    let off_grid = TxFrame {
        channel_mhz: 869.525,
        power_dbm: 22,
        bytes: bytes.clone(),
    };
    assert_eq!(
        e.send(&off_grid).await.unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let lower = TxFrame {
        channel_mhz: 865.125,
        power_dbm: 10,
        bytes,
    };
    assert_eq!(
        e.send(&lower).await.unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test(start_paused = true)]
async fn a_power_change_rewrites_reg1_once() {
    let (mut e, mut module, pins) = wired("850-930").await;
    module.write_all(&[0xC1, 0x04, 0x01, 0x21]).await.unwrap();
    let bytes = frame(1, b"x");
    let tx = TxFrame {
        channel_mhz: 866.125,
        power_dbm: 17,
        bytes: bytes.clone(),
    };
    e.send(&tx).await.unwrap();
    e.send(&tx).await.unwrap();
    let mut want = vec![0xC2, 0x04, 0x01, 0x21];
    for _ in 0..2 {
        want.extend_from_slice(&[0xFF, 0xFF, 16]);
        want.extend_from_slice(&bytes);
    }
    assert_eq!(read_n(&mut module, want.len()).await, want);
    assert_eq!(pins.0.lock().unwrap().len(), 4);
    let bad = TxFrame {
        power_dbm: 20,
        ..tx
    };
    assert_eq!(
        e.send(&bad).await.unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test(start_paused = true)]
async fn set_lbt_writes_reg3_bit4() {
    let (mut e, mut module, _) = wired("850-930").await;
    module.write_all(&[0xC1, 0x06, 0x01, 0xD3]).await.unwrap();
    e.set_lbt(true).await.unwrap();
    e.set_lbt(true).await.unwrap();
    assert_eq!(read_n(&mut module, 4).await, [0xC2, 0x06, 0x01, 0xD3]);
}

#[tokio::test]
async fn recv_splits_two_frames_with_rssi_bytes() {
    let (mut e, mut module) = strapped(None).await;
    let (a, b) = (frame(3, b"one"), frame(4, b"two"));
    let mut wire = a.clone();
    wire.push(0xA0);
    wire.extend_from_slice(&b);
    wire.push(0xB5);
    module.write_all(&wire).await.unwrap();
    let want = |bytes: Vec<u8>, rssi| RxEvent::Frame {
        bytes,
        rssi_dbm: Some(rssi),
        snr_db: None,
    };
    assert_eq!(e.recv().await.unwrap(), want(a, -96));
    assert_eq!(e.recv().await.unwrap(), want(b, -75));
}

#[tokio::test]
async fn recv_resyncs_after_garbage() {
    let (mut e, mut module) = strapped(None).await;
    let f = frame(9, b"ok");
    let mut wire = vec![0x55, 0xAA, 0x02, 0x09, 0x33, 0x44, 0x77, 0x88];
    wire.extend_from_slice(&f);
    wire.push(0xC8);
    module.write_all(&wire).await.unwrap();
    match e.recv().await.unwrap() {
        RxEvent::Frame {
            bytes, rssi_dbm, ..
        } => {
            assert_eq!(bytes, f);
            assert_eq!(rssi_dbm, Some(-56));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn ambient_rssi_is_parsed_and_kept_apart_from_frames() {
    let (mut e, mut module) = strapped(Some(865.125)).await;
    let f = frame(2, b"in flight");
    let mut wire = f.clone();
    wire.push(0x90);
    wire.extend_from_slice(&[0xC1, 0x00, 0x01, 0xA0]);
    module.write_all(&wire).await.unwrap();
    assert_eq!(e.ambient_rssi_dbm(865.125).await.unwrap(), Some(-96));
    assert_eq!(read_n(&mut module, 6).await, proto::AMBIENT_QUERY);
    match e.recv().await.unwrap() {
        RxEvent::Frame { bytes, .. } => assert_eq!(bytes, f),
        other => panic!("unexpected {other:?}"),
    }
    // A strapped module cannot be retuned to sample another channel.
    assert_eq!(e.ambient_rssi_dbm(866.125).await.unwrap(), None);
}

#[tokio::test(start_paused = true)]
async fn ambient_rssi_times_out_without_a_reply() {
    let (mut e, _module) = strapped(Some(865.125)).await;
    let err = e.ambient_rssi_dbm(865.125).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn channel_and_rssi_arithmetic() {
    let c900 = ModuleCaps::e22_900();
    assert_eq!(proto::channel_index(&c900, 850.125), Some(0));
    assert_eq!(proto::channel_index(&c900, 930.125), Some(80));
    assert_eq!(proto::channel_index(&c900, 931.125), None);
    assert_eq!(
        proto::channel_index(&ModuleCaps::e22_400(), 433.125),
        Some(23)
    );
    assert_eq!(proto::rssi_dbm(0xFF), -1);
    assert_eq!(proto::reg3(true), 0xD3);
    assert_eq!(pins::chip_index("/dev/gpiochip4").unwrap(), 4);
    assert!(pins::chip_index("gpio4").is_err());
}
