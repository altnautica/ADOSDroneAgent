//! `detect`: firmware, version, vehicle class and the full parameter table.

use serde_json::{Map, Value};

use crate::setup::fcstate::Fw;
use crate::setup::schema::obj;
use crate::setup::step::{step_core, ActErr, Cx, Step, StepCore};

/// AUTOPILOT_VERSION message id.
const MSG_AUTOPILOT_VERSION: u32 = 148;
const RETRY_MS: u64 = 2000;
const TIMEOUT_MS: u64 = 90_000;

#[derive(Default)]
pub struct Detect {
    core: StepCore,
    started_ms: u64,
    asked_ms: u64,
}

impl Step for Detect {
    step_core!("detect");

    fn begin(&mut self, cx: &mut Cx) {
        self.core.running();
        self.started_ms = cx.now;
        self.asked_ms = cx.now;
        cx.out.request_message(MSG_AUTOPILOT_VERSION);
        cx.params.request_all(&mut cx.out.msgs, cx.now);
    }

    fn tick(&mut self, cx: &mut Cx) {
        self.core.progress = Some(cx.params.download_progress());
        self.core.set_live("params", cx.params.count());
        if cx.fc.uid.is_none() && cx.now.saturating_sub(self.asked_ms) >= RETRY_MS {
            self.asked_ms = cx.now;
            cx.out.request_message(MSG_AUTOPILOT_VERSION);
        }
        if let Some(fw) = cx.fc.firmware() {
            self.core.set_live("firmware", fw.as_str());
            if fw == Fw::Other {
                self.core.fail("setup.blocker.unsupported_firmware");
                return;
            }
        }
        let ready = cx.fc.firmware().is_some() && cx.fc.uid.is_some() && cx.params.complete();
        if ready {
            self.core.done(obj([
                ("firmware", Value::from(cx.adapter.fw.as_str())),
                (
                    "firmware_version",
                    cx.fc
                        .version_string()
                        .map(Value::from)
                        .unwrap_or(Value::Null),
                ),
                ("vehicle_class", Value::from(cx.adapter.class.as_str())),
                (
                    "uid",
                    cx.fc
                        .identity()
                        .map(|(u, _)| Value::from(u))
                        .unwrap_or(Value::Null),
                ),
                (
                    "uid_source",
                    cx.fc
                        .identity()
                        .map(|(_, s)| serde_json::json!(s))
                        .unwrap_or(Value::Null),
                ),
                ("params", Value::from(cx.params.count())),
                (
                    "mission_state_inferred",
                    Value::from(
                        cx.adapter.fw == Fw::ArduPilot
                            && cx.fc.version().is_some_and(|v| v < (4, 5)),
                    ),
                ),
            ]));
        } else if cx.now.saturating_sub(self.started_ms) > TIMEOUT_MS {
            self.core.fail("setup.blocker.detect_timeout");
        }
    }

    fn action(
        &mut self,
        _id: &str,
        _args: &Map<String, Value>,
        _cx: &mut Cx,
    ) -> Result<(), ActErr> {
        Err("unknown_action")
    }
}
