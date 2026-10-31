use super::*;

impl Default for ReceiverState {
    fn default() -> Self {
        Self {
            role: "receiver".to_string(),
            drone_iface: String::new(),
            listen_port: 5800,
            accept_local_nic: true,
            mesh_iface: "bat0".to_string(),
            relays: Vec::new(),
            fragments_after_dedup: 0,
            fec_repaired: 0,
            output_kbps: 0,
            up: false,
            mesh_adapter: None,
        }
    }
}

impl ReceiverState {
    /// Atomically write the state to `wfb-receiver.json` (Contract-E path). The
    /// run-dir path resolves via `run_path` (honouring the `ADOS_RUN_DIR`
    /// override); the write itself is delegated to `write_to` so tests can target
    /// an explicit temp path without mutating process-global env.
    pub fn write(&self) -> std::io::Result<()> {
        self.write_to(Path::new(&crate::paths::run_path("wfb-receiver.json")))
    }

    /// Atomically write the state to an explicit sidecar path. The path seam that
    /// lets a test write into its own temp dir without touching `ADOS_RUN_DIR`.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        ados_protocol::sidecar::write_json_atomic(path, self, 0o644)
    }

    /// Write the state file AND ship the same body to the logging store as a
    /// single `gs.receiver_state` event. The struct is persisted to disk
    /// directly, so the on-disk sidecar stays byte-identical to `write()`; the
    /// JSON value is built only for the store event (the nested `relays` array
    /// round-trips through `json_object_to_fields`). Best-effort: an absent
    /// logging daemon drops the event without disturbing the poll loop; an I/O
    /// error on the file write is surfaced to the caller exactly as `write()`.
    pub fn write_and_emit(
        &self,
        ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
    ) -> std::io::Result<()> {
        self.write_and_emit_to(
            Path::new(&crate::paths::run_path("wfb-receiver.json")),
            ingest,
        )
    }

    /// [`write_and_emit`](Self::write_and_emit) against an explicit sidecar path.
    /// The path seam that keeps a test's best-effort write inside its own temp
    /// tree without mutating the process-global `ADOS_RUN_DIR`.
    pub fn write_and_emit_to(
        &self,
        path: &Path,
        ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
    ) -> std::io::Result<()> {
        let res = ados_protocol::sidecar::write_json_atomic(path, self, 0o644);
        if let Some(em) = ingest {
            let v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
            em.emit_event(
                "gs.receiver_state",
                ados_protocol::logd::Level::Info,
                crate::wfb_rx::stats::json_object_to_fields(&v),
            );
        }
        res
    }
}

/// Upsert the relays seen this poll (by batman-neighbor MAC) into `state`, refreshing
/// `last_seen_ms` and returning the MACs that were NOT present before (first sight →
/// caller emits `relay_connected`). Pure over the serialized `Vec<RelayStats>`. This is
/// the populate half the superseded packaged receiver never implemented: it only aged
/// relays out.
pub(super) fn upsert_relays(
    state: &mut ReceiverState,
    macs: &[String],
    now_ms: i64,
) -> Vec<String> {
    let mut newly = Vec::new();
    for mac in macs {
        if let Some(r) = state.relays.iter_mut().find(|r| &r.mac == mac) {
            r.last_seen_ms = now_ms;
        } else {
            state.relays.push(RelayStats {
                mac: mac.clone(),
                last_seen_ms: now_ms,
                fragments: None,
            });
            newly.push(mac.clone());
        }
    }
    newly
}

/// Remove relays silent past `RELAY_GRACE_MS`, returning the aged-out MACs so the
/// caller emits `relay_disconnected`.
pub(super) fn age_out_relays(state: &mut ReceiverState, now_ms: i64) -> Vec<String> {
    let mut removed = Vec::new();
    state.relays.retain(|r| {
        let stale = now_ms - r.last_seen_ms > RELAY_GRACE_MS;
        if stale {
            removed.push(r.mac.clone());
        }
        !stale
    });
    removed
}

/// Read the batman-adv neighbor MACs on `mesh_iface` (`batctl -m <if> n -H`):
/// the relays and other mesh peers currently reachable, which the churn
/// watcher folds into the per-relay map. Returns an empty vec on a batctl
/// error so a wedged module never stalls the loop.
pub(super) async fn neighbor_macs(mesh_iface: &str) -> Vec<String> {
    let args = ["-m", mesh_iface, "n", "-H"];
    let (rc, out, _e) = crate::mesh::batctl::run("batctl", &args, Duration::from_secs(3)).await;
    if rc != 0 {
        return Vec::new();
    }
    crate::mesh::batctl::parse_neighbors(&out, mesh_events::now_ms())
        .into_iter()
        .map(|n| n.mac)
        .collect()
}

/// One churn step over the relay map: drop revoked MACs (a revoked relay is
/// never presented as one), upsert the neighbor MACs seen this poll
/// (refreshing existing, adding new) and age out the silent ones. Returns the
/// `(newly_connected, disconnected)` MACs the caller emits; a revoked relay
/// already in the map is reported as disconnected. Pure over the serialized
/// state, so the upsert→emit decision is unit-testable without a running mesh
/// or the cross-process event seam.
pub(super) fn churn_step(
    state: &mut ReceiverState,
    macs: &[String],
    revoked: &BTreeSet<String>,
    now_ms: i64,
) -> (Vec<String>, Vec<String>) {
    let is_revoked = |mac: &str| revoked.iter().any(|r| r.eq_ignore_ascii_case(mac));
    let mut removed = Vec::new();
    state.relays.retain(|r| {
        let gone = is_revoked(&r.mac);
        if gone {
            removed.push(r.mac.clone());
        }
        !gone
    });
    let allowed: Vec<String> = macs
        .iter()
        .filter(|m| !is_revoked(m.as_str()))
        .cloned()
        .collect();
    let newly = upsert_relays(state, &allowed, now_ms);
    removed.extend(age_out_relays(state, now_ms));
    (newly, removed)
}

/// The relay-churn watcher: each `POLL_INTERVAL`, read mesh neighbor MACs and
/// the revocation list, upsert the non-revoked neighbors into the relay map
/// (emit `relay_connected` for new), and age out the silent ones (emit
/// `relay_disconnected`). Runs until cancelled.
pub(super) async fn watch_relay_churn(state: Arc<Mutex<ReceiverState>>, mesh_iface: String) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let macs = neighbor_macs(&mesh_iface).await;
        let revoked = crate::pairing::revocations::load();
        let now = mesh_events::now_ms();
        let (newly, removed) = {
            let mut s = state.lock().await;
            churn_step(&mut s, &macs, &revoked, now)
        };
        for mac in newly {
            mesh_events::emit(
                mesh_events::KIND_RELAY_CONNECTED,
                json!({ "relay_mac": mac }),
            );
        }
        for mac in removed {
            mesh_events::emit(
                mesh_events::KIND_RELAY_DISCONNECTED,
                json!({ "relay_mac": mac }),
            );
        }
    }
}
