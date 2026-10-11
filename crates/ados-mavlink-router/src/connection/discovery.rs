//! FC port discovery and the transport open.

use super::*;

impl FcConnection {
    /// Open the configured (or discovered) transport. A configured port that
    /// starts with `tcp:` or `udp:` opens a network MAVLink transport; otherwise
    /// the serial discovery + baud-probe path runs. Returns the read/write halves
    /// plus the port label and (serial only) baud on success, and records in
    /// `opened_unproven` whether the link was opened without probe evidence.
    pub(super) async fn open(&self) -> Option<(BoxedReadHalf, BoxedWriteHalf, String, u32)> {
        self.opened_unproven.store(false, Ordering::Relaxed);
        // MAVLink-over-ELRS ingest: a resolved source REPLACES the configured
        // port and discovery entirely. `radio.crsf.mode: mavlink` is the
        // operator's explicit statement that the RC module is the MAVLink
        // bearer (the same precedence the pin already wins over a
        // contradictory `mavlink.serial_port` in RC mode), and falling
        // through on an open failure would latch some other port while the
        // module sat unused. A failed open is a failed attempt: the run
        // loop's backoff owns the retry and the `source_unreachable` hint
        // names the cause. No CRSF is parsed on this lane — the module
        // firmware owns the air protocol; the host side is plain MAVLink
        // bytes through the ordinary read loop and its heartbeat gate.
        if let Some(src) = self.cfg.crsf_mavlink_source() {
            return match src {
                CrsfMavlinkSource::Serial { device } => {
                    let device = device.to_string();
                    match open_serial(&device, CRSF_MAVLINK_BAUD) {
                        Some(stream) => Some(split_serial(stream, device, CRSF_MAVLINK_BAUD)),
                        None => {
                            tracing::warn!(device = %device, "fc_crsf_mavlink_serial_open_failed");
                            None
                        }
                    }
                }
                CrsfMavlinkSource::BackpackUdp { port } => open_udp_listen(port).await,
            };
        }

        // SITL / network transport: detected from the configured connection
        // string, never from serial discovery (baud is not meaningful here).
        let configured = self.cfg.serial_port.trim();
        if let Some(spec) = parse_net_spec(configured) {
            return self.open_net(spec).await;
        }

        let candidates = self.candidate_ports();
        // A configured baud skips the probe.
        if self.cfg.baud_rate != 0 && !self.cfg.serial_port.is_empty() {
            for cand in candidates {
                if let Some(stream) = open_serial(&cand.name, self.cfg.baud_rate) {
                    return Some(split_serial(stream, cand.name, self.cfg.baud_rate));
                }
            }
            return None;
        }

        // Pass 1: probe EVERY candidate, best-ranked first (see
        // `candidate_ports`), before any port is opened on a guess. A port that
        // stayed silent is set aside rather than opened, so an idle onboard
        // UART that enumerates ahead of the FC can never latch the router
        // while the FC sits unprobed behind it.
        let sole = candidates.len() == 1;
        let mut silent: Vec<(FcCandidate, bool)> = Vec::new();
        for cand in candidates {
            // Whether the sweep heard a talking FC (MAVLink or MSP) on this
            // port, even if the follow-up open failed.
            let mut sweep_heard_fc = false;
            for &baud in BAUD_CANDIDATES {
                match probe_baud(&cand.name, baud).await {
                    ProbeOutcome::Heartbeat => {
                        sweep_heard_fc = true;
                        if let Some(stream) = open_serial(&cand.name, baud) {
                            return Some(split_serial(stream, cand.name, baud));
                        }
                    }
                    ProbeOutcome::Msp => {
                        // The FC is emitting MSP, not MAVLink — no baud will yield
                        // a HEARTBEAT. Open here so the read loop surfaces the
                        // msp_detected hint, and stop sweeping the remaining bauds.
                        sweep_heard_fc = true;
                        if let Some(stream) = open_serial(&cand.name, baud) {
                            return Some(split_serial(stream, cand.name, baud));
                        }
                        break;
                    }
                    ProbeOutcome::None => {}
                }
            }
            silent.push((cand, sweep_heard_fc));
        }

        // Pass 2: nothing proved itself, so open on a guess only where a guess
        // is reasonable — the sole candidate, or a USB one (an FC's USB-CDC
        // port, or a Betaflight / iNav board that stays silent until a ground
        // station polls it). The link it yields is unproven: `run` closes it
        // if it shows no HEARTBEAT and no MSP within `UNPROVEN_LINK_TIMEOUT`.
        let eligible: Vec<(FcCandidate, bool)> = silent
            .into_iter()
            .filter(|(cand, _)| fallback_open_allowed(sole, cand))
            .collect();
        let eligible_count = eligible.len();
        for (idx, (cand, sweep_heard_fc)) in eligible.into_iter().enumerate() {
            // A silent port behind a known RC-bridge USB id (CP2102 / CH340 /
            // ESP32-S3 — the bridges an ExpressLRS TX module enumerates behind)
            // is skipped by the no-evidence fallback open ONLY when the RC lane
            // is genuinely in play as an UNPINNED `crsf_rc` module — the one case
            // where a silent bridge candidate is more likely the RC transmitter
            // than the flight controller (a pinned module is already removed by
            // `candidate_ports`, and a `mavlink`-mode module is the FC carrier we
            // WANT to open). The skip is otherwise fatal to a budget MSP flight
            // controller: `probe_baud` is listen-only and a Betaflight / iNav FC
            // is SILENT until the GCS polls it, so `sweep_heard_fc` is always
            // false for a silent MSP FC, and an ungated skip would strand it (it
            // never opens, so `fcReachable` never goes true). The last remaining
            // eligible candidate is never skipped for the same reason — better
            // to try it than to guarantee no FC.
            let is_last = idx + 1 == eligible_count;
            if !sweep_heard_fc && self.skip_silent_bridge_candidate(cand.usb, is_last) {
                if let Some((vid, pid)) = cand.usb {
                    tracing::info!(
                        port = %cand.name,
                        vid = format_args!("{vid:04x}"),
                        pid = format_args!("{pid:04x}"),
                        "silent_rc_bridge_fallback_skipped"
                    );
                }
                continue;
            }
            // Last-ditch: open at the fallback baud without a positive probe.
            if let Some(stream) = open_serial(&cand.name, BAUD_FALLBACK) {
                self.opened_unproven.store(true, Ordering::Relaxed);
                return Some(split_serial(stream, cand.name, BAUD_FALLBACK));
            }
        }
        None
    }

    /// Open a TCP or UDP MAVLink transport for SITL. TCP connects to the peer;
    /// UDP binds a local socket and connects it to the peer so plain send/recv
    /// reach it. Baud is reported as 0 for network transports.
    pub(super) async fn open_net(
        &self,
        spec: NetSpec,
    ) -> Option<(BoxedReadHalf, BoxedWriteHalf, String, u32)> {
        match spec {
            NetSpec::Tcp(addr) => match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    let (rd, wr) = tokio::io::split(stream);
                    let label = format!("tcp:{addr}");
                    tracing::info!(addr = %addr, "fc_tcp_connecting");
                    Some((Box::pin(rd), Box::pin(wr), label, 0))
                }
                Err(e) => {
                    tracing::warn!(addr = %addr, error = %e, "fc_tcp_connect_failed");
                    None
                }
            },
            NetSpec::Udp(addr) => {
                let sock = match UdpSocket::bind(("0.0.0.0", 0)).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "fc_udp_bind_failed");
                        return None;
                    }
                };
                if let Err(e) = sock.connect(&addr).await {
                    tracing::warn!(addr = %addr, error = %e, "fc_udp_connect_failed");
                    return None;
                }
                let sock = std::sync::Arc::new(sock);
                let (rd, wr) = tokio::io::split(UdpAdapter { sock: sock.clone() });
                let label = format!("udp:{addr}");
                tracing::info!(addr = %addr, "fc_udp_bound");
                Some((Box::pin(rd), Box::pin(wr), label, 0))
            }
        }
    }

    /// The RC lane is opted in as a plain RC transmitter (`crsf_rc`) with no
    /// pinned device — the only configuration where a silent, RC-bridge-VID
    /// serial candidate might actually be the RC module rather than a flight
    /// controller. A pinned module is already removed by
    /// [`Self::candidate_ports`], and a `mavlink`-mode module is the FC carrier
    /// this router WANTS to open, so both sit outside this case. A DISABLED lane
    /// is never in play at all.
    pub(super) fn crsf_rc_lane_unpinned(&self) -> bool {
        self.cfg.crsf_enabled
            && self.cfg.crsf_mode == "crsf_rc"
            && self.cfg.crsf_device.trim().is_empty()
    }

    /// Whether a silent serial candidate (no heartbeat / MSP on the passive baud
    /// sweep) should be skipped by the last-ditch fallback open because it is
    /// most likely an unpinned `crsf_rc` RC transmitter module.
    ///
    /// True only when the RC lane is an unpinned `crsf_rc` lane
    /// ([`Self::crsf_rc_lane_unpinned`]), the candidate is behind a known
    /// RC-bridge USB id, AND it is not the sole/last remaining candidate. The
    /// last-candidate exemption and the lane gate are the safety valves: a
    /// silent MSP flight controller (Betaflight / iNav stays silent until the
    /// GCS polls it) leaves no sweep evidence either, so an over-eager skip
    /// would strand a real FC and leave `fcReachable` false forever.
    pub(super) fn skip_silent_bridge_candidate(
        &self,
        usb: Option<(u16, u16)>,
        is_last: bool,
    ) -> bool {
        if is_last || !self.crsf_rc_lane_unpinned() {
            return false;
        }
        matches!(usb, Some((vid, pid)) if is_rc_bridge_usb_id(vid, pid))
    }

    /// The serial ports the FC link may open, best candidate first (see
    /// [`candidate_rank`]). The pinned CRSF/ELRS device
    /// (`radio.crsf.device`) is excluded on BOTH paths — an RC transmitter
    /// module's port must never be opened or baud-swept by the router, even when
    /// the FC port is (mis)configured to the same node — but ONLY while the CRSF
    /// lane is opted in (`radio.crsf.enabled`): the pin is meaningful only for an
    /// active lane, so a stale pin left over from a DISABLED lane must never drop
    /// the flight controller.
    pub(super) fn candidate_ports(&self) -> Vec<FcCandidate> {
        // A stale device pin from a disabled lane names nothing this router owns,
        // so it must not exclude the FC (an operator who turns the CRSF lane off
        // but leaves `radio.crsf.device` set would otherwise lose the flight
        // controller whenever the two happen to match).
        let crsf_pin = if self.cfg.crsf_enabled {
            self.cfg.crsf_device.trim()
        } else {
            ""
        };
        if !self.cfg.serial_port.is_empty() {
            if !crsf_pin.is_empty() && same_device(&self.cfg.serial_port, crsf_pin) {
                tracing::warn!(
                    port = %self.cfg.serial_port,
                    "fc_serial_port_is_the_pinned_crsf_device_no_fc_candidates"
                );
                return Vec::new();
            }
            return vec![FcCandidate {
                name: self.cfg.serial_port.clone(),
                usb: None,
            }];
        }
        match tokio_serial::available_ports() {
            Ok(ports) => {
                let mut candidates: Vec<FcCandidate> = ports
                    .into_iter()
                    .filter(|p| is_candidate_port(&p.port_type, &p.port_name))
                    .filter(|p| crsf_pin.is_empty() || !same_device(&p.port_name, crsf_pin))
                    .map(|p| FcCandidate {
                        usb: match &p.port_type {
                            tokio_serial::SerialPortType::UsbPort(info) => {
                                Some((info.vid, info.pid))
                            }
                            _ => None,
                        },
                        name: p.port_name,
                    })
                    .collect();
                // Stable: equal ranks keep the enumeration order.
                candidates.sort_by_key(candidate_rank);
                candidates
            }
            Err(_) => Vec::new(),
        }
    }
}

/// A serial candidate for the FC link: the device path plus, when it came from
/// USB enumeration, the backing USB id. The pinned-port path carries no id (an
/// explicitly configured FC port is opened as configured, no vendor gating).
pub(super) struct FcCandidate {
    pub(super) name: String,
    pub(super) usb: Option<(u16, u16)>,
}

/// USB vendor ids flight controllers enumerate under: pid.codes (ArduPilot
/// ChibiOS boards), 3D Robotics (PX4 / Pixhawk), CubePilot, Holybro, and
/// STMicroelectronics (the STM32 virtual COM port Betaflight, iNav and many
/// ArduPilot boards present).
pub(super) const FC_USB_VENDORS: [u16; 5] = [0x1209, 0x26AC, 0x2DAE, 0x3162, 0x0483];

/// Probe order for discovered serial candidates, lowest first: a USB device
/// from a known flight-controller vendor, then any USB CDC-ACM port (the class
/// an FC's native USB presents), then other USB serial bridges, then onboard
/// UARTs. Onboard UARTs come last because an idle console, GPS or modem UART
/// is the most common silent port on a companion computer.
pub(super) fn candidate_rank(cand: &FcCandidate) -> u8 {
    let acm = cand.name.contains("ttyACM") || cand.name.contains("usbmodem");
    match cand.usb {
        Some((vid, _)) if FC_USB_VENDORS.contains(&vid) => 0,
        _ if acm => 1,
        Some(_) => 2,
        None => 3,
    }
}

/// Whether a candidate that stayed silent through the whole baud sweep may be
/// opened anyway at the fallback baud: only when it is the sole candidate, or
/// a USB device. An onboard UART among several candidates is never opened on
/// a guess.
pub(super) fn fallback_open_allowed(sole: bool, cand: &FcCandidate) -> bool {
    sole || cand.usb.is_some()
}
