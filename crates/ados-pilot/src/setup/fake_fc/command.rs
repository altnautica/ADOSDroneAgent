//! The fake FC's COMMAND_LONG handling: calibrations, reboot, mode changes,
//! arming (with the autotune gain save) and take-off.

use super::*;

impl FakeFc {
    pub(super) fn command(&mut self, c: &COMMAND_LONG_DATA, now: u64) {
        use MavCmd::*;
        let mut result = MavResult::MAV_RESULT_ACCEPTED;
        match c.command {
            MAV_CMD_REQUEST_MESSAGE if c.param1 as u32 == 148 => {
                if let Some(u2) = self.uid2 {
                    self.raw.push_back(version_frame(self.uid, u2));
                }
                self.queue
                    .push_back(MavMessage::AUTOPILOT_VERSION(AUTOPILOT_VERSION_DATA {
                        uid: self.uid,
                        flight_sw_version: (4 << 24) | (6 << 16) | (2 << 8),
                        ..Default::default()
                    }));
            }
            MAV_CMD_PREFLIGHT_CALIBRATION => {
                if c.param5 == 1.0 {
                    self.accel_pose = Some(1);
                    self.queue
                        .push_back(text("Place vehicle level and press any key."));
                    self.queue
                        .push_back(MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                            command: MAV_CMD_ACCELCAL_VEHICLE_POS,
                            param1: 1.0,
                            ..Default::default()
                        }));
                } else if c.param3 == 1.0 {
                    // ArduPilot: ground pressure, then the airspeed zero as an
                    // in-progress task when a sensor is enabled.
                    if self.params.get("ARSPD_TYPE").is_some_and(|t| *t != 0.0) {
                        self.airspeed_cal_until = Some(now + 2000);
                        result = MavResult::MAV_RESULT_IN_PROGRESS;
                    }
                } else if c.param6 != 0.0 {
                    // ArduPilot handles no p6 calibration (compass-motor or airspeed).
                    result = MavResult::MAV_RESULT_UNSUPPORTED;
                }
            }
            MAV_CMD_ACCELCAL_VEHICLE_POS => {
                if let Some(p) = self.accel_pose {
                    if c.param1 as u8 == p {
                        let next = p + 1;
                        if next > 6 {
                            self.accel_pose = None;
                            self.queue.push_back(text("Calibration successful"));
                            self.queue
                                .push_back(MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                                    command: MAV_CMD_ACCELCAL_VEHICLE_POS,
                                    param1: 16.0,
                                    ..Default::default()
                                }));
                        } else {
                            self.accel_pose = Some(next);
                            self.queue
                                .push_back(MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                                    command: MAV_CMD_ACCELCAL_VEHICLE_POS,
                                    param1: next as f32,
                                    ..Default::default()
                                }));
                        }
                    }
                }
                return;
            }
            MAV_CMD_DO_START_MAG_CAL => {
                let fit = self.script.mag_fitness.pop_front().unwrap_or(8.0);
                self.mag = Some((now, fit));
            }
            MAV_CMD_DO_CANCEL_MAG_CAL => self.mag = None,
            MAV_CMD_SET_MESSAGE_INTERVAL if c.param1 as u32 == 194 => {
                self.rate_streams = c.param2 > 0.0;
            }
            MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN => {
                self.reboots += 1;
                self.rebooting_until = Some(if self.script.reboot_returns {
                    now + 3500
                } else {
                    u64::MAX
                });
            }
            MAV_CMD_DO_SET_MODE => {
                let want = c.param2 as u32;
                // ArduCopter enters AUTOTUNE only from a mode that allows it
                // (STABILIZE 0, ALT_HOLD 2, LOITER 5, POSHOLD 16).
                let allowed =
                    matches!(self.custom_mode, 0 | 2 | 5 | 16) && !self.script.refuse_autotune;
                if self.is_copter() && !self.is_vtol() && want == self.mode_autotune() && !allowed {
                    self.queue
                        .push_back(text("Mode change to Autotune failed: init failed"));
                    result = MavResult::MAV_RESULT_FAILED;
                } else {
                    self.custom_mode = want;
                    if self.is_vtol() {
                        if (17..=22).contains(&want)
                            && (self.vtol_fw || self.vtol_trans.is_some_and(|(_, fw)| fw))
                        {
                            // A Q mode from fixed-wing flight transitions back and hovers where it is.
                            self.vtol_trans = Some((now, false));
                            self.target_ne = None;
                        } else if want == 15 {
                            self.target_ne = Some((self.n, self.e));
                        }
                    }
                    if self.custom_mode == self.mode_autotune() && self.armed {
                        self.autotune_since = Some(now);
                        self.autotune_success = false;
                    }
                }
            }
            MAV_CMD_COMPONENT_ARM_DISARM => {
                if c.param1 == 1.0 {
                    self.armed = true;
                    if !self.is_copter() && self.custom_mode == 13 {
                        self.takeoff_alt =
                            Some(self.params.get("TKOFF_ALT").copied().unwrap_or(50.0));
                    }
                } else if self.alt < 0.2 || c.param2 == 21196.0 {
                    if self.custom_mode == self.mode_autotune() && self.autotune_success {
                        let (axes_p, atc) = if self.is_vtol() {
                            ("Q_AUTOTUNE_AXES", "Q_A")
                        } else {
                            ("AUTOTUNE_AXES", "ATC")
                        };
                        let axes = self.params.get(axes_p).copied().unwrap_or(3.0) as u32;
                        let mut names = Vec::new();
                        let mut scaled: Vec<(String, f32)> = Vec::new();
                        let mut scale = |n: &str, k: f32| {
                            let n = format!("{atc}_{n}");
                            let v = self.params[&n] * k;
                            scaled.push((n, v));
                        };
                        if axes & 1 != 0 {
                            names.push("Roll");
                            scale("RAT_RLL_P", 1.4);
                            scale("ANG_RLL_P", 1.2);
                        }
                        if axes & 2 != 0 {
                            names.push("Pitch");
                            scale("RAT_PIT_P", 1.3);
                        }
                        if axes & 4 != 0 {
                            names.push("Yaw");
                            scale("RAT_YAW_P", 1.5);
                        }
                        // AP_Param::save sends each saved value to the GCS.
                        for (n, v) in scaled {
                            self.set_param_notify(&n, v);
                        }
                        self.queue.push_back(text(&format!(
                            "AutoTune: Saved gains for {}",
                            names.join(" ")
                        )));
                    }
                    self.armed = false;
                    self.autotune_since = None;
                    self.vtol_fw = false;
                    self.vtol_trans = None;
                    self.target_ne = None;
                } else {
                    result = MavResult::MAV_RESULT_DENIED;
                }
            }
            MAV_CMD_NAV_TAKEOFF => {
                if self.armed {
                    self.takeoff_alt = Some(c.param7);
                } else {
                    result = MavResult::MAV_RESULT_DENIED;
                }
            }
            _ => {}
        }
        self.queue.push_back(ack(c.command, result));
    }
}
