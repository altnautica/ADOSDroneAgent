//! Hardware detection, hot-plug handling and the monitor pass.

use super::*;

/// The sensor plane, in start order: the clock and sensor-bus broker first so
/// the bus is bound before its producers, then the sensor hub and the GNSS
/// receivers. No install step enables these units, so the hardware pass is the
/// only thing that starts them. Each exits 0 when it has nothing to run, the hub
/// unit self-gates on its presence or enable marker, and the profile gate keeps
/// each off the profiles it does not serve.
pub(crate) const SENSOR_PLANE_UNITS: [&str; 3] = ["ados-clock", "ados-sensorhub", "ados-gnss"];

impl Supervisor {
    /// Detect connected hardware and start the matching units.
    pub(super) async fn detect_and_start_hardware(&mut self) {
        for name in SENSOR_PLANE_UNITS {
            if self.index_of(name).is_some() {
                self.start_service(name).await;
            }
        }

        // Start the video service when video is enabled and a camera source
        // exists: a local camera node, a network camera source (the IP-camera
        // case: the feed is an rtsp://… / http://… URL, so there is no
        // /dev/video node to detect), or a `cameras[]` entry that is captured
        // (encoded off the frame bus) or a network URL.
        let declared = self.config.declared_cameras;
        let has_video_source = crate::hardware::has_camera().await
            || self.config.video_network_source.is_some()
            || declared.network
            || declared.captured > 0;
        if self.config.video_enabled && self.index_of("ados-video").is_some() && has_video_source {
            self.start_service("ados-video").await;
        } else if !self.config.video_enabled {
            tracing::info!("video service skipped (video.mode disabled)");
        }

        // Start the vision engine when it is enabled, or when any camera is
        // marked `capture: true` (the engine is then that camera's only owner,
        // so leaving it down leaves the camera dark and the video pipeline's
        // frame-bus input empty), and a camera source exists.
        // The engine feeds the frame stream the follow / designate plugins and
        // the world-model capture consume, so leaving it unstarted breaks that
        // whole pipeline. It is not in any install-time enable set, so the
        // supervisor is the ONLY thing that brings it up — without this branch a
        // `vision.enabled: true` config would silently never start the engine.
        //
        // A vision-enabled config that never runs vision is a misconfiguration,
        // not a silent no-op, so every reason the engine does NOT start is
        // surfaced loudly rather than skipped in silence:
        //   * profile/headless gate — vision runs on the drone profile only and
        //     is excluded from the lean headless core, so on any other node the
        //     unit's binary is absent; starting it would crash-loop. Report it
        //     instead of leaving the operator's `vision.enabled: true` dark.
        //   * no camera source — the engine has no frames to consume.
        if self.config.vision_enabled || declared.captured > 0 {
            if let Some(i) = self.index_of("ados-vision") {
                if !gate_allows(&self.services[i], &self.config) {
                    tracing::warn!(
                        profile = %self.config.profile_wire,
                        headless = self.config.headless_mode,
                        "vision enabled but ados-vision is gated off for this node; \
                         vision runs on the drone profile and is excluded from headless mode"
                    );
                } else if has_video_source {
                    self.start_service("ados-vision").await;
                } else {
                    tracing::warn!(
                        "vision enabled but no camera source configured; ados-vision not started"
                    );
                }
            }
        }

        // A relay / receiver ground station's units (batman, then its WFB
        // plane) are enabled by nothing, so after a reboot they only run if this
        // pass starts them. The direct role's receive plane is this node's radio
        // unit and starts below, behind the adapter check.
        if self.config.is_ground_station() {
            let role = self.config.live_role();
            for unit in crate::role::role_units(&role) {
                let name = crate::role::service_name(unit);
                if RADIO_UNITS.contains(&name) || self.index_of(name).is_none() {
                    continue;
                }
                self.start_service(name).await;
            }
        }

        // Start the radio unit this node owns: the drone TX plane, or a direct
        // ground station's receive plane.
        if crate::hardware::has_wfb_adapter() {
            if let Some(unit) = self.radio_unit() {
                self.start_service(unit).await;
            }
        }
    }

    /// The radio unit this process starts and restarts for this node's profile
    /// and live role, or `None` when it owns none (workstation, compute, or a
    /// relay / receiver ground station). Resolved through the same gate every
    /// start goes through, so it cannot disagree with it.
    pub(super) fn radio_unit(&self) -> Option<&'static str> {
        RADIO_UNITS.iter().copied().find(|name| {
            self.index_of(name)
                .is_some_and(|i| gate_allows(&self.services[i], &self.config))
        })
    }

    /// The unit that owns a hot-plugged device class on this node.
    pub(super) fn hotplug_unit(&self, kind: crate::hotplug::DevKind) -> Option<&'static str> {
        use crate::hotplug::DevKind;
        let name = match kind {
            DevKind::Camera => "ados-video",
            DevKind::Fc => "ados-mavlink",
            DevKind::Radio => self.radio_unit()?,
            // The CRSF lane service. Kept separate from Fc so an RC-module
            // replug never restarts the FC link.
            DevKind::Elrs => "ados-crsf",
        };
        self.index_of(name).map(|_| name)
    }

    /// Restart the service that owns a hot-plugged device class. A radio edge
    /// goes to the node's own radio unit ([`Self::radio_unit`]), and is held
    /// off while a bind session owns the adapter, exactly like the monitor's
    /// auto-restart.
    pub async fn handle_hotplug(&mut self, kind: crate::hotplug::DevKind) {
        let Some(name) = self.hotplug_unit(kind) else {
            return;
        };
        if self.restart_blocked_by_bind(name).await {
            tracing::info!(
                service = name,
                ?kind,
                "hot-plug restart held: a bind owns the radio"
            );
            return;
        }
        // Coalesce re-enumeration storms: a device that drops and re-appears
        // within the debounce window (DFU → flight, a flaky cable) must not
        // issue a second `systemctl restart` while the first is still settling.
        // The coalesced edge leaves a trailing restart the monitor pass runs
        // once the window has passed.
        if !self
            .hotplug_coord
            .should_restart(kind, std::time::Instant::now())
        {
            tracing::debug!(service = name, ?kind, "hot-plug restart coalesced");
            return;
        }
        tracing::info!(service = name, ?kind, "hot-plug triggered restart");
        self.restart_service(name).await;
    }

    /// Run the trailing restart owed to every device class whose last edge was
    /// coalesced, so a device that came back inside the debounce window gets a
    /// restart after it returned.
    pub(super) async fn run_trailing_hotplug_restarts(&mut self) {
        for kind in self.hotplug_coord.due_trailing(std::time::Instant::now()) {
            let Some(name) = self.hotplug_unit(kind) else {
                continue;
            };
            if self.restart_blocked_by_bind(name).await {
                continue;
            }
            tracing::info!(service = name, ?kind, "hot-plug trailing restart");
            self.restart_service(name).await;
            self.progress.mark();
        }
    }

    /// Whether the monitor should skip auto-restarting `name` because a bind
    /// handshake owns the radio adapter. Real now that the FSM is in-process:
    /// only the radio units are gated, and only while a bind is live.
    pub(super) async fn restart_blocked_by_bind(&self, name: &str) -> bool {
        // `session_active` (not `is_active`) so the gate also holds during the
        // bind's `Idle` stop→`OpeningTunnel` setup window: the normal radio unit
        // is stopped + the injection iface re-prepared there, and a monitor pass
        // landing in that window would otherwise see the unit inactive-but-tracked-
        // Running and auto-restart it, re-claiming the adapter mid-bind.
        RADIO_UNITS.contains(&name) && self.bind.session_active()
    }

    /// One monitor pass: the service half, then the prevention/repair
    /// reconcilers, in order.
    ///
    /// Every stage boundary stamps [`MonitorProgress`], which is what feeds the
    /// systemd watchdog. A slow-but-advancing recovery pass keeps the unit
    /// healthy; a pass wedged inside one stage stops stamping and systemd
    /// restarts the unit rather than reporting `active` while death-detection,
    /// auto-restart and hot-plug handling are all dead. Each subprocess these
    /// backends shell is separately bounded by [`crate::oscmd`], so a stall
    /// here is a real wedge and not a slow external command.
    pub async fn monitor_pass(&mut self) {
        self.reconcile_services().await;
        self.progress.mark();
        self.run_trailing_hotplug_restarts().await;
        self.progress.mark();

        // Regulatory-domain reconcile (PREVENTION): re-assert the configured
        // wanted domain when a self-managed injection PHY has left a foreign
        // baked country as the global domain (which breaks the onboard WiFi's
        // data path). Channel-safety-validated so it never caps the WFB radio;
        // a cheap no-op when the domain is already in sync. Runs before the
        // reactive self-heal so a freshly-reconciled domain heads off the break
        // the self-heal would otherwise have to repair.
        self.reg_reconciler.tick().await;
        self.progress.mark();

        // WiFi power-save runtime reconcile (PREVENTION): the FullMAC onboard-WiFi
        // driver re-enables 802.11 power-save after an NM reconnect / hotplug /
        // driver reload, which drops unicast frames on an idle link so the box
        // silently falls off the LAN. Re-assert `power_save off` on every station
        // interface and record the verified per-interface state for the heartbeat.
        // Cheap (one `iw get` per iface; a `set` only on a real drift).
        self.wifi_powersave.tick().await;
        self.progress.mark();

        // Reactive network self-heal: detect + rebuild an onboard managed-WiFi
        // link whose data path died under the radio bring-up, so the box keeps a
        // working failover. Independent of service state; a no-op when there is
        // no onboard managed WiFi or the WiFi is healthy. Kept as the backstop
        // for a link that still needs an explicit rebuild after a domain drift.
        self.wifi_selfheal.tick().await;
        self.progress.mark();

        // Management-link guardian (REACTIVE backstop for the WHOLE link): detect
        // a dead operator management link (no carrier / no lease / unreachable
        // gateway) and walk a bounded, self-restoring software repair ladder
        // without a reboot, across NetworkManager and systemd-networkd. Runs
        // after the per-connection WiFi self-heal so the cheaper fix gets first
        // crack; cheap when the link is healthy, one repair rung per tick.
        self.mgmt_guardian.tick().await;
        self.progress.mark();

        // Onboard-WiFi heartbeat reach-back (LAST resort): when the wired
        // primary is physically down for a sustained window, declare a
        // heartbeat-only fallback so the box stays visible to the GCS. Runs
        // after the guardian (which repairs the link while it physically
        // exists); cheap when the wired primary is up.
        self.mgmt_failover.tick().await;
        self.progress.mark();

        // USB-rehome self-heal (LAST resort for a slow-port, not-radiating
        // adapter): the reconciler decides; if it authorizes an attempt, the
        // supervisor quiesces the radio unit, rebinds the USB device, and brings
        // the unit back (its startup re-probes the adapter). The next decide()
        // re-checks the fresh stats to confirm. Cheap when the adapter is fine.
        //
        // Never rehome during a bind: the bind sequence owns the adapter (it
        // stops the normal wfb unit and drives monitor mode + the key-transfer
        // tunnel), so yanking the USB device or restarting the wfb unit under it
        // would corrupt the handshake — the same exclusion `restart_blocked_by_bind`
        // applies to ordinary restarts. Gating the whole stop/rebind/start
        // sequence here also keeps the post-rebind start out of the bind window.
        // `session_active` (not `is_active`) so the rehome is also held off during
        // the bind's `Idle` setup window, before the data-plane state advances.
        if !self.bind.session_active() {
            if let Some(plan) = self.usb_rehome.decide().await {
                let unit = plan.unit;
                self.stop_service(unit).await;
                self.progress.mark();
                self.wait_for_stop(&[unit], Duration::from_secs(5)).await;
                crate::usb_rehome::execute_rebind(&plan).await;
                self.progress.mark();
                self.start_service(unit).await;
            }
        }
        self.progress.mark();

        // Camera USB-recovery (force re-enumeration of an absent/wedged camera
        // that failed its cold-boot port-enable). No unit stop: the video
        // pipeline re-discovers via udev once the device re-enumerates. Cheap
        // when the camera is present; drone-only (gated on a fresh camera-state).
        if let Some(plan) = self.camera_usb_recovery.decide().await {
            crate::usb_rehome::camera::execute_camera_recovery(&plan).await;
        }
        self.progress.mark();

        // Disk janitor (hourly, not per-tick): reclaim the apt cache, over-long
        // plugin logs and audit trail, aged recordings, and — once free space is
        // short — the package index, journal history, and older quarantined
        // stores. Cheap on the ticks it is not due, and a no-op on the ones it
        // is when nothing is over its cap. The capture category stamps progress
        // between sessions, so a long eviction does not starve the watchdog.
        self.janitor.tick(&self.progress).await;
        self.progress.mark();
    }
}
