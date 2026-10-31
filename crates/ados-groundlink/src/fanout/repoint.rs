use super::*;

/// How often the fan-out re-reads the published hero selection.
///
/// Slow on purpose: the read costs a tmpfs stat in the common case (no
/// selection published, single-drone fleet) and the operator-visible cost of a
/// second's delay between clicking a drone and its video arriving is nothing
/// next to the encoder respawn that switch already waits on.
pub const HERO_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Which fleet slot the fan-out should be serving right now.
///
/// The published hero selection wins when it is live — the sidecar names a slot
/// AND the device that holds it, and the fleet registry still agrees. Anything
/// else falls back to `fallback_slot`, the generation's primary (the lowest
/// registered slot), which is the whole behaviour before hero selection existed
/// and remains correct for the single-drone fleet that never publishes one.
///
/// Requiring the registry to agree is what makes a stale selection harmless: a
/// hero that has since unpaired no longer matches its slot, so the fan-out
/// returns to the primary instead of listening forever on a port that nothing
/// transmits to.
pub fn resolve_fanout_slot(fallback_slot: u8, hero_path: &Path, registry_path: &Path) -> u8 {
    let Some(hero) = crate::fleet_hero::read_hero_from(hero_path) else {
        return fallback_slot;
    };
    let still_registered = crate::fleet::FleetRegistry::load(registry_path)
        .slots()
        .any(|s| s.slot == hero.slot && s.device_id == hero.device_id);
    if still_registered {
        hero.slot
    } else {
        fallback_slot
    }
}

/// Run the fan-out, re-pointing it whenever the slot it should serve changes.
///
/// The inner [`run_fanout`] loop owns one bound socket, so following a new hero
/// means binding a different port — and rebinding is the ONLY thing that
/// happens on a change: while the answer from `resolve` is unchanged the loop is
/// never touched, so a fleet with a settled hero forwards every datagram without
/// interruption. `resolve` returning the same slot forever (the absent-sidecar
/// case) therefore costs one poll per interval and no rebind at all.
///
/// The counters are supervised, not merely published: a generation whose
/// `forwarded` total freezes for [`FANOUT_STALL_WINDOW`] after having forwarded
/// at least one datagram is rebound through the same path a fatal socket error
/// takes, because a fan-out that is alive with a frozen counter is doing no
/// work at all and process liveness is not proof of work.
///
/// Returns only when `resolve` cannot be served at all is not a case: a fatal
/// socket error backs off and re-resolves rather than ending the fan-out for the
/// generation, because going permanently dark is a worse answer than retrying a
/// bind that is very likely transient. The stall rebind takes the same bounded
/// `REBIND_BACKOFF` — a fixed retry, never an exponential ladder and never a
/// terminal give-up state that would need a shell on the vehicle to clear.
pub async fn run_repointing_fanout<R, P>(
    resolve: R,
    port_for: P,
    targets: &[SocketAddr],
    counters: &FanoutCounters,
    poll: Duration,
) -> std::io::Result<()>
where
    R: Fn() -> u8,
    P: Fn(u8) -> u16,
{
    loop {
        let slot = resolve();
        let listen: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port_for(slot)).into();
        // The forwarded total at this bind's start, so the stall watchdog's
        // "has anything arrived yet" gate is about THIS socket rather than the
        // process lifetime.
        let baseline = counters.forwarded();
        tokio::select! {
            res = run_fanout(listen, targets, counters) => {
                match res {
                    // `run_fanout` does not return on success; treat it as a
                    // clean stop rather than inventing a reason to loop.
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            slot,
                            listen = %listen,
                            "fanout_socket_failed_rebinding"
                        );
                        tokio::time::sleep(REBIND_BACKOFF).await;
                    }
                }
            }
            next = wait_for_slot_change(&resolve, slot, poll) => {
                tracing::info!(from = slot, to = next, "fanout_repointed_to_hero_slot");
            }
            frozen_at = watch_for_fanout_stall(
                counters,
                baseline,
                FANOUT_STALL_WINDOW,
                FANOUT_SAMPLE_INTERVAL,
            ) => {
                tracing::warn!(
                    slot,
                    listen = %listen,
                    stall_window_s = FANOUT_STALL_WINDOW.as_secs_f64(),
                    forwarded = frozen_at,
                    "fanout_forward_stalled_rebinding"
                );
                tokio::time::sleep(REBIND_BACKOFF).await;
            }
        }
    }
}

/// Resolve on a slow tick until the answer differs from `current`, then return
/// the new slot. Never returns while the selection is unchanged, which is what
/// leaves the running fan-out undisturbed.
pub(super) async fn wait_for_slot_change<R: Fn() -> u8>(
    resolve: &R,
    current: u8,
    poll: Duration,
) -> u8 {
    loop {
        tokio::time::sleep(poll).await;
        let next = resolve();
        if next != current {
            return next;
        }
    }
}

/// The default ground-station fan-out wiring: listen on the hero's video egress,
/// forward to the mediamtx ingest and the LCD tap, all on localhost.
/// `fallback_slot` is the generation's primary, served whenever no live hero
/// selection is published. `counters` is the shared handle the stats reader also
/// holds so the sidecar surfaces the forwarded/drop totals.
///
/// The two downstream ports are single-stream surfaces (one mediamtx ingest, one
/// LCD tap), so exactly one slot is fanned out at a time: the operator's hero.
/// Apart from the receive chain's primary, only the hero slot's video receiver
/// runs (see `wfb_rx::SlotReceivers`), and nothing reads any slot's video port
/// but this one: another drone's video is not served by this ground station
/// until it is made the hero. That is what makes serving the
/// RIGHT slot here the whole of what the operator sees, and why the fleet
/// roster marks the served slot as `video_hero`.
pub async fn run_default_fanout(
    fallback_slot: u8,
    counters: FanoutCounters,
) -> std::io::Result<()> {
    let hero_path = PathBuf::from(crate::fleet_hero::hero_path());
    let registry_path = PathBuf::from(crate::fleet::FLEET_REGISTRY_PATH);
    let targets = [
        (std::net::Ipv4Addr::LOCALHOST, MEDIAMTX_PORT).into(),
        (std::net::Ipv4Addr::LOCALHOST, LCD_PORT).into(),
    ];
    run_repointing_fanout(
        || resolve_fanout_slot(fallback_slot, &hero_path, &registry_path),
        crate::wfb_rx::video_rx_port,
        &targets,
        &counters,
        HERO_POLL_INTERVAL,
    )
    .await
}
