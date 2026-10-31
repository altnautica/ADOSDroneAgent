use super::*;

/// Write a minimal `reg_blocked` ground sidecar so the heartbeat + panel show the
/// regulatory conflict while the run loop retries the gate. Carries the reason
/// code and the rendezvous channel under inspection; no receive chain is running,
/// so the link-quality block defaults. Atomic via the Contract E writer.
///
/// When an `IngestEmitter` is passed, the SAME body is shipped to the logging
/// store as a single full-snapshot `link.wfb_status` event right after the file
/// write, so the durable read source and the on-disk sidecar stay in lockstep on
/// this degraded-state write too — not only on the per-line active writes in the
/// stats reader. A store-first read therefore never lags the `reg_blocked` file.
/// Best-effort: an absent logging daemon drops the event without disturbing the
/// retry loop.
pub fn write_reg_blocked_sidecar(
    interface: &str,
    adapter: &GsAdapterInfo,
    channel: u8,
    cfg: &WfbConfig,
    reg: &GsRegSnapshot,
    reason: &str,
    ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    let snap = LinkStats::default();
    // The chain is not running, so the live channel cannot be read; report the
    // rendezvous home for actual/rendezvous/operating.
    let channels = GsChannelTruth {
        actual: channel,
        rendezvous: channel,
        operating: channel,
    };
    // No receive chain is running, so no injection is claimed. The USB facts are
    // physical properties of the selected adapter and stay truthful here — a
    // blocked gate on a slow-USB adapter must still show WHY.
    let adapter = GsAdapterInfo {
        injection_ok: false,
        ..adapter.clone()
    };
    let mut v = build_gs_stats(
        &snap,
        interface,
        &adapter,
        channels,
        reg,
        cfg,
        STATE_REG_BLOCKED,
        crate::acquire::AcquireState::Searching.as_str(),
        false, // not channel-locked
        0.0,   // no valid decodes
        0,     // no reacquire kills
        0,     // no zombie kills
        None,  // no silence window (the chain is not running)
        0.0,   // no inbound video
    );
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "reg_block_reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    let _ = ados_protocol::sidecar::write_json_atomic(
        &ados_protocol::paths::run_path("wfb-stats"),
        &v,
        0o644,
    );
    if let Some(em) = ingest {
        em.emit_event(
            "link.wfb_status",
            ados_protocol::logd::Level::Info,
            json_object_to_fields(&v),
        );
    }
}

/// Write a `no_injection` ground sidecar so a stuck receive plane self-reports
/// its degraded state instead of going silent. The direct-role run loop lands in
/// the no-injection retry arm when an adapter resolved but monitor-mode injection
/// setup did not establish — typically a slow-USB adapter, which is exactly the
/// operator-visible CAUSE this sidecar must carry. No receive chain runs, so the
/// link-quality block defaults and injection is denied; the adapter's chipset +
/// USB speed/degraded facts ride verbatim so the panel shows WHY the link is
/// stuck rather than a blank. Atomic via the Contract E writer.
///
/// The regulatory gate has not run in this arm (it precedes `prepare_interfaces`),
/// so the live reg picture is the honest unknown default — never a stale claim
/// from a prior generation; only the configured posture rides.
///
/// When an `IngestEmitter` is passed, the SAME body ships to the logging store as
/// a single full-snapshot `link.wfb_status` event right after the file write, so
/// the durable read source and the on-disk sidecar stay in lockstep on this
/// degraded-state write too — symmetric with the reg-blocked writer. Best-effort:
/// an absent logging daemon drops the event without disturbing the retry loop.
pub fn write_no_injection_sidecar(
    interface: &str,
    adapter: &GsAdapterInfo,
    channel: u8,
    cfg: &WfbConfig,
    ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    let snap = LinkStats::default();
    // The chain is not running, so the live channel cannot be read; report the
    // rendezvous home for actual/rendezvous/operating.
    let channels = GsChannelTruth {
        actual: channel,
        rendezvous: channel,
        operating: channel,
    };
    // No receive chain is running, so no injection is claimed. The USB facts are
    // physical properties of the selected adapter and stay truthful here — a
    // stuck no-injection arm on a slow-USB adapter must still show WHY.
    let adapter = GsAdapterInfo {
        injection_ok: false,
        ..adapter.clone()
    };
    let v = build_gs_stats(
        &snap,
        interface,
        &adapter,
        channels,
        &configured_posture(),
        cfg,
        STATE_NO_INJECTION,
        crate::acquire::AcquireState::Searching.as_str(),
        false, // not channel-locked
        0.0,   // no valid decodes
        0,     // no reacquire kills
        0,     // no zombie kills
        None,  // no silence window (the chain is not running)
        0.0,   // no inbound video
    );
    let _ = ados_protocol::sidecar::write_json_atomic(
        &ados_protocol::paths::run_path("wfb-stats"),
        &v,
        0o644,
    );
    if let Some(em) = ingest {
        em.emit_event(
            "link.wfb_status",
            ados_protocol::logd::Level::Info,
            json_object_to_fields(&v),
        );
    }
}

/// Write a `blocked_unpaired` ground sidecar so a ground station with no receive
/// key self-reports instead of going completely silent.
///
/// The pairing gate sits at the very top of the run loop, ahead of adapter
/// resolution, and used to `continue` without writing anything — so a ground
/// station in this state published NO sidecar at all. That is not a degraded
/// reading, it is the absence of one, and it hid an entire half of a live
/// failure: a drone transmitting into a void on a key from a peer that had been
/// reflashed, facing a ground station that had no key and said nothing. The
/// journal knew. Nothing else did.
///
/// Nothing has been examined at this point, so every verdict is the honest
/// unknown: no adapter record (hence null chipset, null injection verdict, null
/// USB facts), no regulatory picture, no link quality. The one thing this write
/// asserts is WHY the plane is deaf, which is the whole point.
///
/// When an `IngestEmitter` is passed the same body ships to the logging store as
/// a `link.wfb_status` event, symmetric with the reg-blocked and no-injection
/// writers, so the durable read source never lags the file.
pub fn write_blocked_unpaired_sidecar(
    interface: &str,
    channel: u8,
    cfg: &WfbConfig,
    ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    write_unexamined_sidecar(interface, channel, cfg, STATE_BLOCKED_UNPAIRED, ingest);
}

/// Write a `no_adapter` ground sidecar: the receive key is present but no
/// receive adapter could be found (unplugged, or never attached).
///
/// Without this write the sidecar kept the previous generation's body, so an
/// adapter pulled mid-session left `state: active` on every surface that did
/// not apply its own staleness rule. Like the unpaired arm nothing has been
/// examined, so every hardware verdict is the honest unknown.
pub fn write_no_adapter_sidecar(
    interface: &str,
    channel: u8,
    cfg: &WfbConfig,
    ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    write_unexamined_sidecar(interface, channel, cfg, STATE_NO_ADAPTER, ingest);
}

/// The shared body of the blocked arms that examined no hardware: `state`
/// names why the plane is deaf, and nothing else is claimed.
pub(super) fn write_unexamined_sidecar(
    interface: &str,
    channel: u8,
    cfg: &WfbConfig,
    state: &str,
    ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    let v = unexamined_stats(interface, channel, cfg, state, &configured_posture());
    let _ = ados_protocol::sidecar::write_json_atomic(
        &ados_protocol::paths::run_path("wfb-stats"),
        &v,
        0o644,
    );
    if let Some(em) = ingest {
        em.emit_event(
            "link.wfb_status",
            ados_protocol::logd::Level::Info,
            json_object_to_fields(&v),
        );
    }
}

pub(super) fn unexamined_stats(
    interface: &str,
    channel: u8,
    cfg: &WfbConfig,
    state: &str,
    reg: &GsRegSnapshot,
) -> serde_json::Value {
    let snap = LinkStats::default();
    // No chain, no adapter, no live channel: report the rendezvous home.
    let channels = GsChannelTruth {
        actual: channel,
        rendezvous: channel,
        operating: channel,
    };
    build_gs_stats(
        &snap,
        interface,
        // Default = nothing resolved: null chipset, and therefore null injection
        // and null USB verdicts rather than confident booleans about hardware
        // this arm never looked at.
        &GsAdapterInfo::default(),
        channels,
        reg,
        cfg,
        state,
        crate::acquire::AcquireState::Searching.as_str(),
        false, // not channel-locked
        0.0,   // no valid decodes
        0,     // no reacquire kills
        0,     // no zombie kills
        None,  // no silence window (the chain is not running)
        0.0,   // no inbound video
    )
}
