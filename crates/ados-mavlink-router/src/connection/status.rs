//! Read-only link status accessors the state publish and REST layer read.

use super::*;

impl FcConnection {
    /// The flight controller's `(system, component)` id as latched from its
    /// own HEARTBEAT, or `None` before one has been seen on this process.
    ///
    /// The fleet assigns each aircraft its own system id, so the MAVLink
    /// convention of "the autopilot is 1/1" is wrong on every drone but one.
    /// Callers that address the vehicle use this, never a constant.
    pub fn learned_fc_identity(&self) -> Option<(u8, u8)> {
        if !self.fc_identity_latched.load(Ordering::Acquire) {
            return None;
        }
        Some((
            self.target_system.load(Ordering::Relaxed),
            self.target_component.load(Ordering::Relaxed),
        ))
    }

    /// Record the autopilot's identity from one of its HEARTBEATs.
    ///
    /// Once latched, the router publishes the pair as `fc_system_id` /
    /// `fc_component_id` in the state snapshot extras, so the control plane
    /// addresses the vehicle by the id it actually heartbeats with.
    pub fn latch_fc_identity(&self, system_id: u8, component_id: u8) {
        self.target_system.store(system_id, Ordering::Relaxed);
        self.target_component.store(component_id, Ordering::Relaxed);
        self.fc_identity_latched.store(true, Ordering::Release);
    }

    pub async fn heartbeat_age_s(&self) -> Option<f64> {
        self.last_heartbeat_at
            .lock()
            .await
            .map(|t| t.elapsed().as_secs_f64())
    }
    /// Whether a fresh HEARTBEAT has decoded within [`HEARTBEAT_TIMEOUT`]. The
    /// demo loop has no real HEARTBEAT clock but drives synthetic telemetry, so
    /// it reports alive whenever the transport is open (the port label `demo`).
    pub async fn mavlink_alive(&self) -> bool {
        if &*self.port.lock().await == "demo" {
            return self.connected.load(Ordering::Relaxed);
        }
        match *self.last_heartbeat_at.lock().await {
            Some(t) => t.elapsed() < HEARTBEAT_TIMEOUT,
            None => false,
        }
    }
    /// The gated truth: the FC is connected only when the transport is open AND
    /// a fresh HEARTBEAT has been decoded. "Presence is not proof" — an open
    /// serial port at a configured baud is not enough; the autopilot must be
    /// talking. Replaces the old transport-open-only `connected()` for the
    /// published `fc_connected` extra.
    pub async fn connected(&self) -> bool {
        self.transport_open() && self.mavlink_alive().await
    }
    /// A human-actionable hint about why the FC link is not alive, computed from
    /// the current liveness plus recent MSP evidence:
    ///
    ///   - `none` — the link is alive (or a demo run, or there is no transport),
    ///     so there is nothing to explain.
    ///   - `msp_detected` — the transport is open, no HEARTBEAT decoded, and MSP
    ///     frame starts were seen on the byte stream within the recent window:
    ///     the FC is configured for MSP, not MAVLink, on this port. The operator
    ///     fix is to set the FC's serial-port protocol to MAVLink.
    ///   - `no_heartbeat` — the transport is open, no HEARTBEAT decoded, and no
    ///     MSP traffic was seen: a wrong baud, an unpowered / not-yet-booted FC,
    ///     wrong wiring, or a serial protocol other than MAVLink/MSP.
    ///   - `source_unreachable` — the transport is NOT open and a configured
    ///     (non-auto) source failed to open: the configured endpoint (a wrong /
    ///     down `tcp:`/`udp:` host or an absent serial device) is unreachable.
    ///     The operator fix is the source config, not plugging in an FC.
    ///
    /// Derived (not latched) so it self-corrects: the moment a HEARTBEAT decodes
    /// it reads `none`, the MSP evidence ages out via [`MSP_HINT_TTL`], and the
    /// unreachable flag clears the moment a transport opens.
    pub async fn link_hint(&self) -> &'static str {
        if &*self.port.lock().await == "demo" {
            return "none";
        }
        if self.mavlink_alive().await {
            return "none";
        }
        if !self.transport_open() {
            // A configured (non-auto) source whose last open attempt failed is
            // an unreachable / wrong endpoint — the operator's fix is the source
            // config, not "plug in an FC". Auto-discovery stays silent: with no
            // configured endpoint there is nothing to be unreachable.
            if self.open_failed.load(Ordering::Relaxed) && self.source() != "auto" {
                return "source_unreachable";
            }
            return "none";
        }
        // A Betaflight/iNav FC identified by its USB descriptor speaks MSP, not
        // MAVLink, and is silent until polled — so it never trips the passive
        // byte sniff below. The descriptor is the reliable signal that "no
        // heartbeat" is really "this is an MSP FC", not a broken link.
        if self.fc_variant.lock().await.is_some() {
            return "msp_detected";
        }
        let msp_recent = self
            .last_msp_at
            .lock()
            .await
            .map(|t| t.elapsed() < MSP_HINT_TTL)
            .unwrap_or(false);
        if msp_recent {
            "msp_detected"
        } else {
            "no_heartbeat"
        }
    }

    /// The FC firmware family identified from the opened port's USB descriptor
    /// (`betaflight` / `inav`), or `None` for a MAVLink / unknown FC.
    pub async fn fc_variant(&self) -> Option<String> {
        self.fc_variant.lock().await.clone()
    }
    pub async fn port(&self) -> String {
        self.port.lock().await.clone()
    }
    pub fn baud(&self) -> u32 {
        self.baud.load(Ordering::Relaxed)
    }
    /// The configured FC transport class for the snapshot: `serial`, `udp`,
    /// `tcp`, or `auto` (empty `serial_port` → discovery). Derived from the
    /// config's `source` field, falling back to the shape of `serial_port` for a
    /// config that predates the explicit field (a `udp:`/`tcp:` prefix is a
    /// network transport; a non-empty path is serial; empty is auto-detect).
    ///
    /// A resolved MAVLink-over-ELRS ingest owns the source slot first and
    /// reports its live carrier class — `serial` for the module's USB-serial
    /// port, `udp` for the WiFi-backpack listen. Both are true statements
    /// about the open transport; the provenance (the `radio.crsf` block) is
    /// visible on the port label (`fc_port` = the pinned device / the
    /// `udpin:` bind) and on the RC lane's own status surface.
    pub fn source(&self) -> &'static str {
        match self.cfg.crsf_mavlink_source() {
            Some(CrsfMavlinkSource::Serial { .. }) => return "serial",
            Some(CrsfMavlinkSource::BackpackUdp { .. }) => return "udp",
            None => {}
        }
        match self.cfg.source.trim().to_ascii_lowercase().as_str() {
            "serial" => "serial",
            "udp" => "udp",
            "tcp" => "tcp",
            "auto" => "auto",
            // Unset / unknown: infer from the connection string shape.
            _ => {
                let port = self.cfg.serial_port.trim();
                if port.is_empty() {
                    "auto"
                } else if port.starts_with("udp:") {
                    "udp"
                } else if port.starts_with("tcp:") {
                    "tcp"
                } else {
                    "serial"
                }
            }
        }
    }
    /// Whether the resolved FC source is the MAVLink-over-ELRS ingest with its
    /// host->FC command-down direction gated closed (telemetry-only). True
    /// ONLY when the source is a [`CrsfMavlinkSource`] AND the explicit
    /// `radio.crsf.mavlink_command_enabled` marker is off — the default and
    /// only current state. Every other FC source (serial / UDP / TCP /
    /// discovery) returns false, so their command paths are untouched. When
    /// true, [`Self::run`] installs no writer, so the send scheduler's
    /// heartbeat, stream requests, param sweep, and any forwarded client bytes
    /// are all suppressed for this source (send_bytes is a no-op with no
    /// writer). Setting the marker flips this false, restoring the writer for
    /// the bench gate. Because `open()` resolves a `CrsfMavlinkSource` iff
    /// `crsf_mavlink_source()` is `Some`, this predicate is exactly "the open
    /// transport is that source AND its command marker is off".
    ///
    /// Surfaced on the FC status snapshot (`fc_command_down_gated`) so a status
    /// consumer can report "telemetry connected, commands gated" honestly: with
    /// no writer a GCS command over this source is silently dropped, and a plain
    /// `fc_connected: true` would mislead the operator into thinking it reached
    /// the FC.
    pub fn command_down_gated(&self) -> bool {
        self.cfg.crsf_mavlink_source().is_some() && !self.cfg.crsf_mavlink_command_enabled
    }
    /// Install the aux-uplink sender a ground station uses in place of a local
    /// FC writer. Called once, from the daemon's aux wiring, only when this node is a
    /// ground station — a drone always has (or is trying to open) its own FC
    /// writer and must never fall back to radiating a client's bytes onto its
    /// own aux uplink transmitter.
    pub async fn set_aux_uplink(&self, sender: crate::aux_uplink::AuxUplinkSender) {
        *self.aux_uplink.lock().await = Some(sender);
    }
    pub fn param_priming(&self) -> bool {
        self.param_priming.load(Ordering::Relaxed)
    }
    pub fn param_sweep_timed_out(&self) -> bool {
        self.param_sweep_timed_out.load(Ordering::Relaxed)
    }
    pub fn param_sweep_send_failed(&self) -> bool {
        self.param_sweep_send_failed.load(Ordering::Relaxed)
    }
}
