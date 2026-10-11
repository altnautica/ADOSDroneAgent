use super::*;

/// Tunes fine; the TX-power apply runs the real bounded ramp over a
/// scripted driver; the readback is fixed.
struct FakeRadiate {
    accept_from_mbm: Option<i32>,
    readback: Option<f32>,
}

#[async_trait::async_trait]
impl RadiateIo for FakeRadiate {
    async fn cycle_and_tune(&self, _iface: &str, _channel: u8) -> bool {
        true
    }
    async fn apply(&self, iface: &str, dbm: i8, max_dbm: i8, unrestricted: bool) -> Option<i8> {
        let io = ScriptedSet {
            accept_from_mbm: self.accept_from_mbm,
            readback: self.readback,
        };
        crate::adapter::apply_tx_power_with(&io, iface, dbm, max_dbm, unrestricted).await
    }
    async fn read(&self, _iface: &str) -> Option<f32> {
        self.readback
    }
}

struct ScriptedSet {
    accept_from_mbm: Option<i32>,
    readback: Option<f32>,
}

#[async_trait::async_trait]
impl crate::adapter::TxPowerIo for ScriptedSet {
    async fn set_fixed_mbm(&self, _iface: &str, mbm: i32) -> bool {
        self.accept_from_mbm.is_some_and(|from| mbm >= from)
    }
    async fn read_dbm(&self, _iface: &str) -> Option<f32> {
        self.readback
    }
}

#[tokio::test(start_paused = true)]
async fn ensure_radiating_rejects_all_steps_returns_none() {
    // Every step is rejected while the PHY reads back 20 dBm (the driver
    // default): that is not a power this service set, so no value.
    let io = FakeRadiate {
        accept_from_mbm: None,
        readback: Some(20.0),
    };
    assert_eq!(
        ensure_radiating_with(&io, "wlan1", 149, 5, 10, false).await,
        None
    );
}

#[tokio::test(start_paused = true)]
async fn readback_mismatch_reports_live() {
    // 5 dBm accepted, but the PHY reads back 10 dBm: report what it runs.
    let io = FakeRadiate {
        accept_from_mbm: Some(0),
        readback: Some(10.0),
    };
    assert_eq!(
        ensure_radiating_with(&io, "wlan1", 149, 5, 10, false).await,
        Some(10)
    );
}

#[tokio::test(start_paused = true)]
async fn readback_within_tolerance_reports_the_step() {
    let io = FakeRadiate {
        accept_from_mbm: Some(0),
        readback: Some(5.5),
    };
    assert_eq!(
        ensure_radiating_with(&io, "wlan1", 149, 5, 10, false).await,
        Some(5)
    );
}
