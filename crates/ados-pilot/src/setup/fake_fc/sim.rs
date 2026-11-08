//! The fake FC's telemetry stream and flight models.

use super::*;

impl FakeFc {
    pub(super) fn telemetry(&mut self, now: u64) {
        if now.saturating_sub(self.last_hb) >= 1000 {
            self.last_hb = now;
            let base = if self.armed {
                MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED
            } else {
                MavModeFlag::empty()
            };
            self.queue.push_back(MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: self.custom_mode,
                mavtype: self.mav_type,
                autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
                base_mode: base | MavModeFlag::MAV_MODE_FLAG_CUSTOM_MODE_ENABLED,
                system_status: MavState::MAV_STATE_STANDBY,
                mavlink_version: 3,
            }));
            // ArduPlane reports the differential-pressure sensor present when one is enabled.
            let derived = if self.params.get("ARSPD_TYPE").is_some_and(|t| *t != 0.0) {
                MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_DIFFERENTIAL_PRESSURE
            } else {
                MavSysStatusSensor::empty()
            };
            self.queue
                .push_back(MavMessage::SYS_STATUS(SYS_STATUS_DATA {
                    onboard_control_sensors_present: self.script.sensors_present.unwrap_or(derived),
                    voltage_battery: (self.battery_v * 1000.0) as u16,
                    battery_remaining: self.battery_pct,
                    ..Default::default()
                }));
            if let Some((dir, spd)) = self.wind {
                self.queue.push_back(MavMessage::WIND(WIND_DATA {
                    direction: dir,
                    speed: spd,
                    speed_z: 0.0,
                }));
            }
            if let Some(rc) = &self.rc {
                let mut d = RC_CHANNELS_DATA {
                    chancount: rc.len() as u8,
                    ..Default::default()
                };
                let ch = [
                    &mut d.chan1_raw,
                    &mut d.chan2_raw,
                    &mut d.chan3_raw,
                    &mut d.chan4_raw,
                    &mut d.chan5_raw,
                    &mut d.chan6_raw,
                    &mut d.chan7_raw,
                    &mut d.chan8_raw,
                ];
                for (slot, v) in ch.into_iter().zip(rc.iter()) {
                    *slot = *v;
                }
                self.queue.push_back(MavMessage::RC_CHANNELS(d));
            }
        }
        if now.saturating_sub(self.last_tel) >= 100 {
            self.last_tel = now;
            let landed = if self.alt < 0.1 {
                MavLandedState::MAV_LANDED_STATE_ON_GROUND
            } else {
                MavLandedState::MAV_LANDED_STATE_IN_AIR
            };
            let vtol_state = match (self.is_vtol(), self.vtol_trans, self.vtol_fw) {
                (false, _, _) => MavVtolState::MAV_VTOL_STATE_UNDEFINED,
                (true, Some((_, true)), _) => MavVtolState::MAV_VTOL_STATE_TRANSITION_TO_FW,
                (true, Some((_, false)), _) => MavVtolState::MAV_VTOL_STATE_TRANSITION_TO_MC,
                (true, None, true) => MavVtolState::MAV_VTOL_STATE_FW,
                (true, None, false) => MavVtolState::MAV_VTOL_STATE_MC,
            };
            self.queue
                .push_back(MavMessage::EXTENDED_SYS_STATE(EXTENDED_SYS_STATE_DATA {
                    vtol_state,
                    landed_state: landed,
                }));
            let (lat, lon) = latlon_of(self.n, self.e);
            self.queue
                .push_back(MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
                    relative_alt: (self.alt * 1000.0) as i32,
                    lat,
                    lon,
                    ..Default::default()
                }));
            self.queue
                .push_back(MavMessage::LOCAL_POSITION_NED(LOCAL_POSITION_NED_DATA {
                    x: self.n,
                    y: self.e,
                    z: -self.alt,
                    ..Default::default()
                }));
            self.queue
                .push_back(MavMessage::ATTITUDE(ATTITUDE_DATA::default()));
            self.queue.push_back(MavMessage::VFR_HUD(VFR_HUD_DATA {
                throttle: 40,
                ..Default::default()
            }));
            self.queue.push_back(MavMessage::VIBRATION(VIBRATION_DATA {
                vibration_x: 8.0,
                vibration_y: 9.0,
                vibration_z: 12.0,
                ..Default::default()
            }));
        }
    }

    pub(super) fn physics(&mut self, now: u64) {
        self.mag_tick(now);
        if self.airspeed_cal_until.is_some_and(|t| now >= t) {
            self.airspeed_cal_until = None;
            let r = if self.script.airspeed_cal_ok {
                MavResult::MAV_RESULT_ACCEPTED
            } else {
                MavResult::MAV_RESULT_FAILED
            };
            self.queue
                .push_back(ack(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION, r));
        } else if self.airspeed_cal_until.is_some() && now % 1000 < TICK {
            self.queue.push_back(ack(
                MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION,
                MavResult::MAV_RESULT_IN_PROGRESS,
            ));
        }
        let dt = TICK as f32 / 1000.0;
        if !self.armed {
            self.alt = 0.0;
            return;
        }
        self.vtol_physics(now, dt);
        // A QuadPlane lands in QLAND once it hovers again.
        let land =
            self.custom_mode == self.mode_land() && !self.vtol_fw && self.vtol_trans.is_none();
        let autotune = self.custom_mode == self.mode_autotune();
        if land || (autotune && self.is_copter() && self.manual_z < 0.3) {
            self.alt = (self.alt - 1.5 * dt).max(0.0);
        } else if let Some(t) = self.takeoff_alt {
            if self.alt < t {
                self.alt = (self.alt + 2.0 * dt).min(t);
            } else if !autotune {
                // An idle-throttle plane (THR_MAX 0, the TECS sink stages) sinks fast.
                let sink = if self.params.get("THR_MAX").is_some_and(|t| *t == 0.0) {
                    5.0
                } else {
                    2.0
                };
                self.alt = (self.alt - sink * dt).max(t);
            }
        }
        if let Some(since) = self.autotune_since {
            let el = now - since;
            if (1000..1000 + TICK).contains(&el) {
                self.queue.push_back(text("AutoTune: Started"));
            }
            if (2000..2000 + TICK).contains(&el) {
                self.queue.push_back(text("AutoTune: Testing"));
            }
            if el >= self.script.autotune_s * 1000 && !self.autotune_success {
                self.autotune_success = true;
                self.queue.push_back(text("AutoTune: Success"));
            }
        }
    }

    /// QuadPlane transitions (2 s) and horizontal flight: fixed-wing circles
    /// the GUIDED point (or where it went fixed-wing) at 60 m and 15 m/s; a
    /// VTOL in GUIDED with Q_GUIDED_MODE 1 flies to its point at 1 m/s.
    pub(super) fn vtol_physics(&mut self, now: u64, dt: f32) {
        if !self.is_vtol() || self.alt < 0.5 {
            return;
        }
        if let Some((since, to_fw)) = self.vtol_trans {
            if now.saturating_sub(since) >= 2000 {
                self.vtol_trans = None;
                self.vtol_fw = to_fw;
            }
            return;
        }
        if self.vtol_fw {
            let (tn, te) = *self.target_ne.get_or_insert((self.n, self.e));
            let (dn, de) = (self.n - tn, self.e - te);
            let d = (dn * dn + de * de).sqrt();
            let (un, ue) = if d > 0.1 {
                (dn / d, de / d)
            } else {
                (1.0, 0.0)
            };
            // Tangent plus a pull onto the 60 m circle.
            let (vn, ve) = (-ue + (60.0 - d) / 20.0 * un, un + (60.0 - d) / 20.0 * ue);
            let v = (vn * vn + ve * ve).sqrt().max(1e-3);
            self.n += 15.0 * vn / v * dt;
            self.e += 15.0 * ve / v * dt;
        } else if self.custom_mode == 15 && self.params.get("Q_GUIDED_MODE") == Some(&1.0) {
            if let Some((tn, te)) = self.target_ne {
                let (dn, de) = (tn - self.n, te - self.e);
                let d = (dn * dn + de * de).sqrt();
                if d > 0.01 {
                    let step = (1.0 * dt).min(d);
                    self.n += dn / d * step;
                    self.e += de / d * step;
                }
            }
        }
    }

    pub(super) fn mag_tick(&mut self, now: u64) {
        if let Some((start, fit)) = self.mag {
            let pct = ((now - start) / 50).min(100) as u8;
            self.queue
                .push_back(MavMessage::MAG_CAL_PROGRESS(MAG_CAL_PROGRESS_DATA {
                    compass_id: 0,
                    cal_mask: 1,
                    cal_status: MagCalStatus::MAG_CAL_RUNNING_STEP_TWO,
                    completion_pct: pct,
                    ..Default::default()
                }));
            if pct >= 100 {
                self.mag = None;
                self.queue
                    .push_back(MavMessage::MAG_CAL_REPORT(MAG_CAL_REPORT_DATA {
                        fitness: fit,
                        compass_id: 0,
                        cal_mask: 1,
                        cal_status: MagCalStatus::MAG_CAL_SUCCESS,
                        autosaved: 1,
                        ..Default::default()
                    }));
            }
        }
    }
}
