use super::*;

/// Build the `wfb_rx -a` aggregator args. With `accept_local_nic` every local
/// receiving adapter is appended so its fragments are aggregated too; without
/// it the receiver trusts only relay forwards.
///
/// `link_id` is the transmitting drone's (`link_id(fleet_id, slot)`). The
/// aggregator checks every session against the `channel_id` built from it, so
/// without `-i` it sat on wfb-ng's default link id 0 and accepted no fleet
/// drone's stream.
pub fn aggregate_args(
    drone_ifaces: &[String],
    listen_port: u16,
    accept_local_nic: bool,
    rx_key: &Path,
    link_id: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "0".into(),
        "-i".into(),
        link_id.to_string(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        "5600".into(),
        "-a".into(),
        listen_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
    ];
    if accept_local_nic {
        args.extend(drone_ifaces.iter().cloned());
    }
    args
}

/// The aggregator's stderr log. Its stats go to stdout (read below); stderr
/// carries only diagnostics, and a file never fills the way an unread pipe does.
pub(super) const AGGREGATOR_LOG: &str = "/run/ados/wfb-gs-aggregator.log";

/// Spawn the FEC-combine aggregator in its own process group (setsid/killpg).
/// stdout is piped: `wfb_rx` prints its per-interval `PKT` stats line there
/// (`vendor/wfb-ng/src/rx.cpp` `Aggregator::dump_stats`, `IPC_MSG` = stdout).
pub async fn spawn_aggregator(
    drone_ifaces: &[String],
    listen_port: u16,
    accept_local_nic: bool,
    link_id: u32,
) -> std::io::Result<GroupChild> {
    let rx_key = Path::new(ados_radio::paths::WFB_RX_KEY);
    let args = aggregate_args(drone_ifaces, listen_port, accept_local_nic, rx_key, link_id);
    crate::wfb_rx::spawn_wfb("wfb_rx", &args, StdoutMode::Piped, Some(AGGREGATOR_LOG))
}

/// Resolves once the drone this ground station serves is no longer
/// `current`, so the aggregator can be rebuilt on the new link.
pub(super) async fn wait_for_link_change(current: u32) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if crate::fleet_hero::served_video_link_id() != Some(current) {
            return;
        }
    }
}

/// Parse one aggregator stdout line, or `None` when it is not an 11-field `PKT`
/// stats line (the `RX_ANT` lines and anything else are ignored).
pub fn parse_receiver_stats_line(line: &str) -> Option<AggregatorPkt> {
    let mut cols = line.trim_end().split('\t');
    let _ts = cols.next()?;
    if cols.next()? != "PKT" {
        return None;
    }
    let f: Vec<&str> = cols.next()?.split(':').collect();
    if f.len() != 11 {
        return None;
    }
    Some(AggregatorPkt {
        unique: f[5].parse().ok()?,
        fec_recovered: f[6].parse().ok()?,
        bytes_out: f[10].parse().ok()?,
    })
}

/// Tail the aggregator's stdout, folding its stats into shared state and
/// counting stats lines (the delta counter the stall window checks). The
/// `RX_ANT` lines of an interval are buffered and folded into the per-relay
/// counts when that interval's `PKT` line arrives. Returns when the pipe
/// closes.
pub(super) async fn tail_aggregator_stats(
    stdout: tokio::process::ChildStdout,
    state: Arc<Mutex<ReceiverState>>,
    lines_seen: Arc<AtomicU64>,
) {
    let mesh_iface = state.lock().await.mesh_iface.clone();
    let mut lines = ados_proc::lines::BoundedLines::new(stdout);
    let mut interval_ant: Vec<String> = Vec::new();
    while let Some(line) = lines.next_line().await {
        if let Some(pkt) = parse_receiver_stats_line(&line) {
            lines_seen.fetch_add(1, Ordering::Relaxed);
            let per_source = rx_ant::per_source_packets(&rx_ant::parse_rx_ant(
                interval_ant.iter().map(String::as_str),
            ));
            interval_ant.clear();
            let arp = rx_ant::read_arp(&mesh_iface);
            let mut s = state.lock().await;
            fold_aggregator_pkt(&mut s, pkt);
            fold_relay_fragments(&mut s, &per_source, &arp);
        } else if rx_ant::parse_rx_ant_line(&line).is_some() {
            interval_ant.push(line);
        }
    }
}

/// Resolve when the aggregator has exited or its stats stream has stayed flat
/// for [`STATS_SILENCE_WINDOW`]; returns the reason. One arm of the
/// supervision select.
pub(super) async fn watch_aggregator(
    proc: &mut GroupChild,
    lines_seen: &AtomicU64,
) -> &'static str {
    let mut last = lines_seen.load(Ordering::Relaxed);
    let mut last_advance = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !proc.is_running() {
            return "exited";
        }
        let now_lines = lines_seen.load(Ordering::Relaxed);
        if now_lines > last {
            last = now_lines;
            last_advance = Instant::now();
        } else if last_advance.elapsed() >= STATS_SILENCE_WINDOW {
            return "stats_silent";
        }
    }
}
