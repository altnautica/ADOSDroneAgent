//! Sensing reads for the cameras, sensors and storage pages: the sensor, clock
//! and storage routes plus the world-engine capture state. Each read is
//! independent: a `present: false` body (or a route not served) leaves its
//! block absent, a failed request leaves it unreachable.

use super::*;
use crate::pages::reading::Reading;
use crate::pages::sensing_ctx::{
    CaptureCamera, CaptureInfo, ClockInfo, ExposureInfo, GnssSummary, HubInfo, SensingCtx,
    SensorInfo, SensorsInfo, StorageInfo,
};

/// The world-engine capture state route behind the plugin proxy.
pub(super) const CAPTURE_STATE_PATH: &str =
    "/api/plugins/com.altnautica.world-engine/x/atlas/capture/state";

impl StateSource {
    /// Fetch every sensing route and map them into a [`SensingCtx`].
    pub(super) fn fetch_sensing(&self) -> SensingCtx {
        sensing_ctx(
            self.fetch("/api/v1/sensors"),
            self.fetch("/api/v1/clock"),
            self.fetch("/api/v1/storage"),
            self.fetch(CAPTURE_STATE_PATH),
        )
    }
}

/// Map the four fetched reads into a [`SensingCtx`].
pub(super) fn sensing_ctx(
    sensors: Reading<Value>,
    clock: Reading<Value>,
    storage: Reading<Value>,
    capture: Reading<Value>,
) -> SensingCtx {
    SensingCtx {
        sensors: sensors.and_then(|v| present(Some(&v)).map(sensors_info)),
        clock: clock.and_then(|v| present(Some(&v)).map(clock_info)),
        storage: storage.and_then(|v| present(Some(&v)).map(storage_info)),
        capture: capture.and_then(|v| v.is_object().then(|| capture_info(&v))),
    }
}

/// The body when it is an object not marked `present: false`.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| v.is_object() && v.get("present").and_then(Value::as_bool) != Some(false))
}

/// A child that is a JSON object (null and absent both read `None`).
fn obj<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|c| c.is_object())
}

/// A ns sigma; negative wire values mean unknown.
fn sigma(v: &Value) -> Option<f64> {
    f64_field(v, "sigma_ns").filter(|s| *s >= 0.0)
}

fn sensors_info(v: &Value) -> SensorsInfo {
    SensorsInfo {
        hub: obj(v, "hub").map(|h| HubInfo {
            connected: bool_field(h, "connected"),
            transport: string_field(h, "transport"),
            fw: string_field(h, "fw"),
            board: string_field(h, "board"),
            crc_errors: i64_field(h, "crc_errors"),
            dropped: i64_field(h, "dropped"),
        }),
        fc_connected: obj(v, "fc").map(|f| bool_field(f, "connected")),
        sensors: v
            .get("sensors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|s| SensorInfo {
                        id: string_field(s, "id").unwrap_or_default(),
                        kind: string_field(s, "kind").unwrap_or_default(),
                        role: string_field(s, "role"),
                        source: string_field(s, "source"),
                        rate_hz: f64_field(s, "rate_hz"),
                        expected_hz: f64_field(s, "expected_hz"),
                        age_ms: f64_field(s, "age_ms"),
                        sigma_ns: sigma(s),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        gnss: obj(v, "gnss").map(|g| GnssSummary {
            fix: string_field(g, "fix"),
            sats: i64_field(g, "sats"),
        }),
    }
}

fn clock_info(v: &Value) -> ClockInfo {
    ClockInfo {
        timebase: string_field(v, "timebase"),
        locked: bool_field(v, "locked"),
        sigma_ns: sigma(v),
    }
}

fn storage_info(v: &Value) -> StorageInfo {
    let volumes = v.get("volumes").and_then(Value::as_array);
    let vol = volumes.and_then(|a| {
        a.iter()
            .find(|x| bool_field(x, "is_capture"))
            .or_else(|| a.first())
    });
    let cap = obj(v, "capture");
    StorageInfo {
        capture_root: string_field(v, "capture_root"),
        free_bytes: vol.and_then(|x| f64_field(x, "free_bytes")),
        total_bytes: vol.and_then(|x| f64_field(x, "total_bytes")),
        used_bytes: cap.and_then(|c| f64_field(c, "used_bytes")),
        budget_bytes: cap.and_then(|c| f64_field(c, "budget_bytes")),
        sessions: cap.and_then(|c| i64_field(c, "sessions")),
        uploaded: cap.and_then(|c| i64_field(c, "uploaded")),
        state: cap.and_then(|c| string_field(c, "state")),
        write_mbps: obj(v, "write_probe").and_then(|w| f64_field(w, "mbps")),
    }
}

fn capture_info(v: &Value) -> CaptureInfo {
    CaptureInfo {
        state: string_field(v, "state"),
        per_camera: obj(v, "frames")
            .and_then(|f| f.get("per_camera"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|c| CaptureCamera {
                        id: string_field(c, "id").unwrap_or_default(),
                        fps: f64_field(c, "fps"),
                        dropped: i64_field(c, "dropped"),
                        trigger: string_field(c, "trigger"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        exposure: obj(v, "exposure").map(|e| ExposureInfo {
            locked: bool_field(e, "locked"),
            applied: bool_field(e, "applied"),
            exposure_us: f64_field(e, "exposure_us"),
            gain: f64_field(e, "gain"),
            mode: string_field(e, "mode"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sensors_body() -> Value {
        json!({"present":true,"rig_id":"visor-pro-01","calib_version":"2026-10-19a",
         "hub":{"connected":true,"transport":"serial:/dev/ttyACM0","fw":"ados-sensorhub 0.1.0","board":"matek_h743_slim_v3","proto_ver":1,"uptime_s":321,"crc_errors":0,"dropped":0},
         "fc":{"connected":true,"autopilot":"ardupilot","highres_imu":true},
         "sensors":[{"id":"imu0","kind":"imu","role":"nav","source":"hub","rate_hz":1000.0,"expected_hz":1000.0,"last_t_ns":123456789000_u64,"age_ms":1.0,"sigma_ns":420000},
                    {"id":"cam_fl","kind":"camera","role":"front_left","source":"vision","rate_hz":20.0,"expected_hz":20.0,"last_t_ns":123456780000_u64,"age_ms":9.0,"sigma_ns":420000},
                    {"id":"gnss0","kind":"gnss","role":"nav","source":"hub","rate_hz":10.0,"expected_hz":null,"last_t_ns":0,"age_ms":null,"sigma_ns":-1}],
         "gnss":{"fix":"3d","sats":14,"hdop":0.8,"rtk":"none"}})
    }

    fn clock_body() -> Value {
        json!({"present":true,"timebase":"host_monotonic","locked":true,"sigma_ns":420000,"t_ns":123456789000_u64,
         "utc_ns":null,"sources":[]})
    }

    fn storage_body() -> Value {
        json!({"present":true,"capture_root":"/mnt/nvme0/ados/capture","volumes":[
           {"mount":"/","device":"mmcblk0","fs":"ext4","total_bytes":32000000000_u64,"free_bytes":1000,"rotational":false,"is_capture":false},
           {"mount":"/mnt/nvme0","device":"nvme0n1","fs":"ext4","total_bytes":1000204886016_u64,"free_bytes":812345678901_u64,"rotational":false,"is_capture":true}],
         "capture":{"used_bytes":123456789012_u64,"budget_bytes":850174153113_u64,"budget_pct":85,"floor_free_bytes":5368709120_u64,"sessions":12,"uploaded":9,"evict_unuploaded":false,"state":"ok"},
         "write_probe":{"mbps":1450.2,"measured_at_ns":120000000000_u64}})
    }

    fn capture_body() -> Value {
        json!({"state":"recording","session_id":"visor-1-20261019T101500Z-3fa2","started_at_ns":120000000000_u64,"duration_s":312.4,
         "frames":{"total":12480,"dropped_pct":0.2,"per_camera":[{"id":"cam_fl","fps":20.0,"dropped":3}]},
         "exposure":{"locked":true,"applied":false,"exposure_us":4000,"gain":2.0,"mode":"walking"},
         "storage":{"free_bytes":812345678901_u64,"est_remaining_s":14400}})
    }

    #[test]
    fn full_fixtures_map_into_sensing_ctx() {
        let ctx = sensing_ctx(
            Reading::Present(sensors_body()),
            Reading::Present(clock_body()),
            Reading::Present(storage_body()),
            Reading::Present(capture_body()),
        );
        let s = ctx.sensors.into_present().expect("sensors");
        let hub = s.hub.expect("hub");
        assert!(hub.connected);
        assert_eq!(hub.fw.as_deref(), Some("ados-sensorhub 0.1.0"));
        assert_eq!(hub.crc_errors, Some(0));
        assert_eq!(s.fc_connected, Some(true));
        assert_eq!(s.sensors.len(), 3);
        assert_eq!(s.sensors[0].rate_pct(), Some(100.0));
        assert_eq!(s.sensors[1].role.as_deref(), Some("front_left"));
        assert_eq!(s.sensors[2].expected_hz, None);
        assert_eq!(s.sensors[2].age_ms, None);
        assert_eq!(s.sensors[2].sigma_ns, None);
        let g = s.gnss.expect("gnss");
        assert_eq!((g.fix.as_deref(), g.sats), (Some("3d"), Some(14)));

        let k = ctx.clock.into_present().expect("clock");
        assert!(k.locked);
        assert_eq!(k.sigma_ns, Some(420000.0));
        assert_eq!(k.timebase.as_deref(), Some("host_monotonic"));

        let st = ctx.storage.into_present().expect("storage");
        assert_eq!(st.total_bytes, Some(1000204886016.0));
        assert_eq!(st.free_bytes, Some(812345678901.0));
        assert_eq!(st.pending_upload(), Some(3));
        assert_eq!(st.state.as_deref(), Some("ok"));
        assert_eq!(st.write_mbps, Some(1450.2));

        let cap = ctx.capture.into_present().expect("capture");
        assert_eq!(cap.per_camera[0].fps, Some(20.0));
        assert_eq!(cap.per_camera[0].dropped, Some(3));
        let e = cap.exposure.expect("exposure");
        assert!(e.locked && !e.applied);
        assert_eq!(e.exposure_us, Some(4000.0));
    }

    #[test]
    fn absent_services_and_missing_routes_map_to_absent() {
        let ctx = sensing_ctx(
            Reading::Present(
                json!({"present":false,"hub":null,"fc":null,"sensors":[],"gnss":null}),
            ),
            Reading::Present(json!({"present":false})),
            Reading::Absent,
            Reading::Absent,
        );
        assert_eq!(ctx, SensingCtx::default());
    }

    #[test]
    fn failed_reads_stay_unreachable() {
        let ctx = sensing_ctx(
            Reading::Unreachable,
            Reading::Present(clock_body()),
            Reading::Unreachable,
            Reading::Unreachable,
        );
        assert_eq!(ctx.sensors, Reading::Unreachable);
        assert!(ctx.clock.is_present());
        assert_eq!(ctx.storage, Reading::Unreachable);
        assert_eq!(ctx.capture, Reading::Unreachable);
    }

    #[test]
    fn null_blocks_map_to_none() {
        let body = json!({"present":true,"hub":null,"fc":null,"sensors":[],"gnss":null});
        let s = sensing_ctx(
            Reading::Present(body),
            Reading::Absent,
            Reading::Absent,
            Reading::Absent,
        )
        .sensors
        .into_present()
        .expect("sensors");
        assert_eq!((s.hub, s.fc_connected, s.gnss), (None, None, None));
        let st = json!({"present":true,"capture_root":"/c","volumes":[],"capture":{},"write_probe":null});
        let st = sensing_ctx(
            Reading::Absent,
            Reading::Absent,
            Reading::Present(st),
            Reading::Absent,
        )
        .storage
        .into_present()
        .expect("storage");
        assert_eq!(
            (st.free_bytes, st.write_mbps, st.pending_upload()),
            (None, None, None)
        );
    }
}
