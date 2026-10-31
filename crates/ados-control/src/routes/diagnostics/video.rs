//! `GET /api/diag/video`: the per-hop video-pipeline verifier.

use super::*;

// ---------------------------------------------------------------------------
// GET /api/diag/video — the per-hop video-pipeline verifier.
// ---------------------------------------------------------------------------

/// The sampling window (ms) the video harness deltas cumulative counters over.
/// Two reads this far apart give a reliable per-hop "is data flowing NOW" signal
/// for the counters that are cumulative (mediamtx `bytesReceived`, the fan-out
/// `fanout_forwarded` total); the rate fields (decoded packets/s, TX kbps) are
/// read fresh in each sample. 2 s captures a live delta without making the
/// on-demand route feel slow.
pub(super) const VIDEO_SAMPLE_WINDOW_MS: u64 = 2000;

/// `GET /api/diag/video` — a per-hop, reliable-counter verdict on the video
/// pipeline for THIS node's profile, so an operator (or an agent) can attribute
/// a video stall to the exact hop that stopped instead of hand-probing.
///
/// It reads ONLY reliable cumulative/rate counters on the CANONICAL objects (the
/// mediamtx `main` path, the wfb-stats sidecar, the WHEP endpoint) — never
/// tcpdump, `/proc/net/udp` queue depth, or a single snapshot — and samples each
/// twice over `VIDEO_SAMPLE_WINDOW_MS` so a hop is judged by data actually
/// moving, not by a process being alive (the pipeline runbook).
///
/// Drone (video SOURCE) hops: camera→mediamtx (mediamtx `main` bytesReceived
/// delta) and mediamtx→radio-TX (the wfb TX bitrate; a drone injects only what
/// the tap feeds it). Ground-station (video SINK) hops: RF→decode (wfb_rx decoded
/// packets/s + the `link_diag` cause), decode→fan-out (`fanout_forwarded` delta),
/// and fan-out→served-WHEP (mediamtx ingest bytes delta, or the WHEP-serving
/// liveness when the ground mediamtx management API is auth-gated).
pub async fn get_video_diagnostics(State(state): State<AppState>) -> Json<Value> {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, role) = crate::profile::current_profile_and_role(&cfg.agent.profile);

    // Two reads a window apart: each hop is judged by data actually moving between
    // them, not by a process being alive (the pipeline runbook).
    let s0 = VideoSample::take(&state.run_dir).await;
    tokio::time::sleep(std::time::Duration::from_millis(VIDEO_SAMPLE_WINDOW_MS)).await;
    let s1 = VideoSample::take(&state.run_dir).await;

    Json(build_video_diagnostics(&profile, &role, &s0, &s1))
}

/// Assemble the video-diagnostics body from two window samples: the profile-aware
/// hop set (drone = video-source hops, else ground-station = video-sink hops), the
/// upstream-aware verdict, and the response envelope (`profile` / `role` /
/// `canonical_path` / `window_s` / `hops` / `video_dies_at`).
///
/// Pure over the two samples — it reuses [`drone_hops`] / [`gs_hops`] /
/// [`resolve_hops`] and invents no verdict of its own — so the whole pipeline
/// verifier is drivable end to end from a controllable per-hop counter fixture
/// (a synthetic `VideoSample` pair) with no live camera / radio / mediamtx.
pub(super) fn build_video_diagnostics(
    profile: &str,
    role: &Option<String>,
    s0: &VideoSample,
    s1: &VideoSample,
) -> Value {
    let hops = if profile == "drone" {
        drone_hops(s0, s1)
    } else {
        gs_hops(s0, s1)
    };
    let (hop_values, video_dies_at) = resolve_hops(hops);

    json!({
        "profile": profile,
        "role": role,
        "canonical_path": "main",
        "window_s": window_s(),
        "hops": hop_values,
        "video_dies_at": video_dies_at,
    })
}

/// One reading of every video-pipeline source the harness trusts, taken
/// concurrently so a slow probe does not serialise the sample.
pub(super) struct VideoSample {
    /// The mediamtx `main` path cumulative `bytesReceived`, or `None` when the
    /// management API is unreachable / auth-gated.
    pub(super) mtx_bytes_received: Option<i64>,
    /// Whether the WHEP endpoint is bound and serving.
    pub(super) mtx_serving: bool,
    /// The wfb-stats sidecar object (`packets_received`, `bitrate_kbps`,
    /// `link_diag`, `fanout_forwarded`, ...), or `None` when absent/unparseable.
    pub(super) wfb: Option<Map<String, Value>>,
}

impl VideoSample {
    /// Sample every trusted source once: the live mediamtx `main` counters (over
    /// HTTP) and the wfb-stats sidecar under the node's run dir. Delegates the
    /// sidecar read to [`VideoSample::from_run_dir`] so the assembly is drivable
    /// from an explicit run dir in a test.
    pub(super) async fn take(run_dir: &Path) -> Self {
        let (mtx_bytes_received, mtx_serving) = tokio::join!(
            crate::routes::status_full::mediamtx_main_bytes_received(),
            crate::routes::status_full::mediamtx_whep_serving(),
        );
        Self::from_run_dir(run_dir, mtx_bytes_received, mtx_serving)
    }

    /// Build a sample from an explicit run dir (the wfb-stats sidecar source) plus
    /// the two mediamtx readings. The wfb-stats sidecar is the canonical
    /// received-side counter object (`packets_received` / `fanout_forwarded` /
    /// `tx_bytes_per_s` / `link_diag`); reading it from an explicit dir — the
    /// `build_mesh_block_at` pure-core idiom — lets a test drive the verifier from a
    /// controllable sidecar fixture + injected mediamtx counters, with no live
    /// pipeline and no process-global env. `take()` resolves the live sources.
    pub(super) fn from_run_dir(
        run_dir: &Path,
        mtx_bytes_received: Option<i64>,
        mtx_serving: bool,
    ) -> Self {
        Self {
            mtx_bytes_received,
            mtx_serving,
            wfb: crate::sidecar::read_object(&run_dir.join(ados_protocol::paths::WFB_STATS_JSON)),
        }
    }

    /// A numeric field from the wfb sidecar, as `f64`.
    pub(super) fn wfb_num(&self, key: &str) -> Option<f64> {
        self.wfb.as_ref()?.get(key).and_then(Value::as_f64)
    }

    /// A boolean field from the wfb sidecar. `None` when absent or non-boolean,
    /// so "the radio never said" stays distinct from "the radio said false".
    pub(super) fn wfb_bool(&self, key: &str) -> Option<bool> {
        self.wfb.as_ref()?.get(key).and_then(Value::as_bool)
    }

    /// A string field from the wfb sidecar.
    pub(super) fn wfb_str(&self, key: &str) -> Option<String> {
        self.wfb
            .as_ref()?
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// One hop's raw finding before the upstream-aware verdict is applied.
/// `flowing`: `Some(true)` data moved, `Some(false)` this hop read zero,
/// `None` the source was unreadable (verdict `unknown`).
pub(super) struct HopFinding {
    pub(super) name: &'static str,
    pub(super) label: &'static str,
    pub(super) method: &'static str,
    pub(super) metric: String,
    pub(super) flowing: Option<bool>,
    pub(super) detail: Option<String>,
}

/// The drone (video-source) hop findings.
pub(super) fn drone_hops(s0: &VideoSample, s1: &VideoSample) -> Vec<HopFinding> {
    // Hop 1: camera → encoder → mediamtx, by the mediamtx `main` bytesReceived
    // delta (the canonical video-into-mediamtx counter).
    let cam = match (s0.mtx_bytes_received, s1.mtx_bytes_received) {
        (Some(a), Some(b)) => HopFinding {
            name: "camera_to_mediamtx",
            label: "Camera → encoder → mediamtx",
            method: "mediamtx /v3/paths/get/main bytesReceived delta",
            metric: format!("{} bytes in {:.1}s", (b - a).max(0), window_s()),
            flowing: Some(b > a),
            detail: None,
        },
        _ => hop_unknown(
            "camera_to_mediamtx",
            "Camera → encoder → mediamtx",
            "mediamtx /v3/paths/get/main bytesReceived delta",
            "mediamtx management API unreachable",
        ),
    };
    // Hop 2: mediamtx → wfb tap → wfb_tx → radio, by the TX INJECTION RATE
    // `tx_bytes_per_s` (the bytes wfb_tx is actually injecting; a drone injects
    // only what the tap feeds it, so a non-zero TX rate proves the tap → TX leg is
    // flowing). NOT `bitrate_kbps` — on a drone that is the RX-DECODE bitrate,
    // which is 0 because a drone is the video SOURCE and decodes no inbound
    // stream, so reading it here falsely reports a healthy transmitter as stalled
    // (a false negative caught on the bench). RF-radiated is a separate concern
    // proved on the receiver's link_diag, not here (this is the injection hop).
    let tx_bps = s1
        .wfb_num("tx_bytes_per_s")
        .or_else(|| s0.wfb_num("tx_bytes_per_s"));
    // Reception is read, never assumed. This detail line used to be a hardcoded
    // sentence claiming reception WAS confirmed, attached whenever any byte was
    // injected — so a drone transmitting into a void reported "all hops flowing"
    // while its own link diagnostic, on the same box in the same session, said
    // the link was unverified. An advancing TX counter is not proof that anything
    // received the bytes; only the received side can prove that.
    let rx_proof = reception_proof(s1, s0);
    let radio = match tx_bps {
        Some(bps) => HopFinding {
            name: "mediamtx_to_radio_tx",
            label: "Tap → wfb_tx (radio injection)",
            method: "wfb-stats tx_bytes_per_s + reception proof (channel_locked / rf_unverified)",
            metric: match rx_proof {
                ReceptionProof::Confirmed => {
                    format!("{} B/s injected, reception confirmed", bps.max(0.0) as i64)
                }
                ReceptionProof::Unconfirmed => format!(
                    "{} B/s injected, reception UNCONFIRMED",
                    bps.max(0.0) as i64
                ),
                ReceptionProof::Unknown => format!("{} B/s injected", bps.max(0.0) as i64),
            },
            // A dead TX leg is stalled whatever the receiver says — nothing is
            // being transmitted, so reception is not the question. Injecting with
            // no CONFIRMED reception is not a flowing hop either: `None` resolves
            // to `unknown`, which stops the chain and names this hop as where
            // video dies, the honest reading when the bytes may be going nowhere.
            flowing: match (bps > 0.0, rx_proof) {
                (false, _) => Some(false),
                (true, ReceptionProof::Confirmed) => Some(true),
                (true, _) => None,
            },
            detail: Some(
                match rx_proof {
                    ReceptionProof::Confirmed => {
                        "a receiver decoded this transmission (peer-reported reception)"
                    }
                    ReceptionProof::Unconfirmed => {
                        "transmitting, but nothing has confirmed receiving it — check the \
                         receiver is powered, on this channel, and holding the matching key"
                    }
                    ReceptionProof::Unknown => {
                        "the radio did not report a reception verdict, so delivery is unproven"
                    }
                }
                .to_string(),
            ),
        },
        None => hop_unknown(
            "mediamtx_to_radio_tx",
            "Tap → wfb_tx (radio injection)",
            "wfb-stats tx_bytes_per_s (TX injection rate)",
            "wfb-stats sidecar unavailable",
        ),
    };
    vec![cam, radio]
}

/// Whether anything has confirmed receiving what this node transmits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReceptionProof {
    /// A receiver decoded this node's transmission.
    Confirmed,
    /// The radio is transmitting and nothing has confirmed reception.
    Unconfirmed,
    /// The radio reported no verdict either way.
    Unknown,
}

/// Read the received-side verdict the radio already computes, newest sample
/// first.
///
/// `channel_locked` is set from the radio's own `rx_proven`, and `rf_unverified`
/// is its explicit "transmitting with zero confirmed reception" state. Both are
/// derived from decoded peer traffic — frames that passed error correction AND
/// decryption — which is why they can prove delivery when a TX byte counter
/// cannot.
pub(super) fn reception_proof(s1: &VideoSample, s0: &VideoSample) -> ReceptionProof {
    let locked = s1
        .wfb_bool("channel_locked")
        .or_else(|| s0.wfb_bool("channel_locked"));
    let unverified = s1
        .wfb_bool("rf_unverified")
        .or_else(|| s0.wfb_bool("rf_unverified"));
    match (locked, unverified) {
        (Some(true), _) => ReceptionProof::Confirmed,
        (_, Some(true)) => ReceptionProof::Unconfirmed,
        (Some(false), Some(false)) => ReceptionProof::Unconfirmed,
        _ => ReceptionProof::Unknown,
    }
}

/// The ground-station (video-sink) hop findings.
pub(super) fn gs_hops(s0: &VideoSample, s1: &VideoSample) -> Vec<HopFinding> {
    // Hop 1: RF → wfb_rx decode, by decoded packets/s, with the link_diag cause
    // surfaced when nothing is decoding (deaf / mis_keyed / jammed / searching).
    let pkts = s1
        .wfb_num("packets_received")
        .or_else(|| s0.wfb_num("packets_received"));
    let link_diag = s1.wfb_str("link_diag").or_else(|| s0.wfb_str("link_diag"));
    let decode = match pkts {
        Some(p) => HopFinding {
            name: "rf_to_decode",
            label: "RF → wfb_rx decode",
            method: "wfb_rx decoded packets/s + link_diag",
            metric: format!("{p:.0} pkt/s decoded"),
            flowing: Some(p > 0.0),
            detail: link_diag.map(|d| format!("link_diag={d}")),
        },
        None => hop_unknown(
            "rf_to_decode",
            "RF → wfb_rx decode",
            "wfb_rx decoded packets/s + link_diag",
            "wfb-stats sidecar unavailable",
        ),
    };
    // Hop 2: decode → fan-out, by the fanout_forwarded delta.
    let ff0 = s0.wfb_num("fanout_forwarded");
    let ff1 = s1.wfb_num("fanout_forwarded");
    let fanout = match (ff0, ff1) {
        (Some(a), Some(b)) => HopFinding {
            name: "decode_to_fanout",
            label: "wfb_rx → fan-out",
            method: "fanout_forwarded delta",
            metric: format!(
                "{} datagrams in {:.1}s",
                (b - a).max(0.0) as i64,
                window_s()
            ),
            flowing: Some(b > a),
            detail: None,
        },
        _ => hop_unknown(
            "decode_to_fanout",
            "wfb_rx → fan-out",
            "fanout_forwarded delta",
            "fanout_forwarded not present in wfb-stats",
        ),
    };
    // Hop 3: fan-out → mediamtx-gs ingest → served WHEP. Prefer the mediamtx
    // ingest bytes delta; when the ground mediamtx management API is auth-gated
    // (bytesReceived unreadable) fall back to the WHEP-serving liveness.
    let serve = match (s0.mtx_bytes_received, s1.mtx_bytes_received) {
        (Some(a), Some(b)) => HopFinding {
            name: "fanout_to_served",
            label: "Fan-out → mediamtx → WHEP",
            method: "mediamtx main bytesReceived delta",
            metric: format!("{} bytes in {:.1}s", (b - a).max(0), window_s()),
            flowing: Some(b > a),
            detail: None,
        },
        _ => HopFinding {
            name: "fanout_to_served",
            label: "Fan-out → mediamtx → WHEP",
            method: "WHEP endpoint serving (mediamtx API auth-gated)",
            metric: if s1.mtx_serving {
                "WHEP serving".to_string()
            } else {
                "WHEP not serving".to_string()
            },
            // A serving WHEP endpoint proves mediamtx is up, not that frames are
            // flowing into it: serving reads `unknown`, and only a WHEP endpoint
            // that is not serving at all proves the hop stalled.
            flowing: (!s1.mtx_serving).then_some(false),
            detail: Some("ingest byte counter unavailable; WHEP liveness only".to_string()),
        },
    };
    vec![decode, fanout, serve]
}

/// A hop whose source could not be read: verdict `unknown`, with the reason.
pub(super) fn hop_unknown(
    name: &'static str,
    label: &'static str,
    method: &'static str,
    reason: &str,
) -> HopFinding {
    HopFinding {
        name,
        label,
        method,
        metric: reason.to_string(),
        flowing: None,
        detail: None,
    }
}

/// The sampling window in seconds, for the metric strings.
pub(super) fn window_s() -> f64 {
    VIDEO_SAMPLE_WINDOW_MS as f64 / 1000.0
}

/// Apply the upstream-aware verdict to each hop and locate where video dies.
///
/// Walking the hops in flow order, a hop is `flowing` when data moved, `stalled`
/// when it read zero WHILE its upstream was flowing (the fault is here),
/// `no_upstream` when it read zero because the upstream was already dead (not
/// this hop's fault), and `unknown` when the source was unreadable. `video_dies_at`
/// is the first `stalled` hop; if none stalled it is the first hop that broke the
/// chain (`no_upstream`/`unknown` boundary), or `null` when everything flows.
pub(super) fn resolve_hops(findings: Vec<HopFinding>) -> (Vec<Value>, Value) {
    let mut out = Vec::with_capacity(findings.len());
    let mut upstream_flowing = true;
    let mut dies_at: Option<&'static str> = None;

    for f in &findings {
        let verdict = match f.flowing {
            None => "unknown",
            Some(true) => "flowing",
            Some(false) if upstream_flowing => "stalled",
            Some(false) => "no_upstream",
        };
        if dies_at.is_none() && (verdict == "stalled" || verdict == "unknown") {
            dies_at = Some(f.name);
        }
        let mut obj = Map::new();
        obj.insert("name".to_string(), json!(f.name));
        obj.insert("label".to_string(), json!(f.label));
        obj.insert("verdict".to_string(), json!(verdict));
        obj.insert("flowing".to_string(), json!(f.flowing));
        obj.insert("metric".to_string(), json!(f.metric));
        obj.insert("method".to_string(), json!(f.method));
        if let Some(d) = &f.detail {
            obj.insert("detail".to_string(), json!(d));
        }
        out.push(Value::Object(obj));

        // The chain continues only while a hop is confirmed flowing; a stalled /
        // unknown / no_upstream hop means everything downstream is starved.
        upstream_flowing = verdict == "flowing";
    }

    (out, dies_at.map(|s| json!(s)).unwrap_or(Value::Null))
}
