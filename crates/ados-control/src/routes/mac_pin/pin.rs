//! `POST /api/v1/network/mac/pin`: pin an interface MAC, optionally applied live.

use super::*;

/// The `POST /api/v1/network/mac/pin` request body: a required interface, an optional explicit
/// MAC, and an optional live-apply flag (default false).
#[derive(Debug, Deserialize)]
pub struct MacPinRequest {
    pub iface: String,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub apply_now: bool,
}

/// The outcome of the optional `apply_now` live re-tag: whether it was applied +
/// the human note the body carries. Split out so the gate decisions are tested
/// without the `ip` IO.
pub(super) struct ApplyOutcome {
    pub(super) applied_live: bool,
    pub(super) note: String,
}

/// The default `note` when `apply_now` is false (or unset): the pin lands on the
/// next reconcile.
pub(super) const NOTE_PINNED_NEXT_BOOT: &str =
    "pinned for next boot; the agent writes the .link on its next reconcile";

/// `POST /api/v1/network/mac/pin` → pin a stable MAC on an adapter.
///
/// Resolves the MAC (body or state-file candidate), validates + normalises it,
/// merges the override into the config, and (optionally + gated) re-tags the live
/// interface. Degrades to the documented `400`/`500` error-object bodies on each
/// guard; never panics on a seam fault.
pub async fn post_mac_pin(
    State(state): State<AppState>,
    Json(req): Json<MacPinRequest>,
) -> Response {
    post_mac_pin_at(&state.pairing_paths.config, &state_file_path(), req).await
}

/// The pin logic against explicit config + state-file paths. The public handler
/// resolves both from the app state / env; this takes them directly so a test can
/// point them at temp paths without mutating process-global env.
pub(super) async fn post_mac_pin_at(
    config_path: &Path,
    state_path: &Path,
    req: MacPinRequest,
) -> Response {
    // 0. The adapter's stable identity: overrides are keyed by it, never by name.
    let key = match adapter_key_or_400(state_path, &req.iface) {
        Ok(k) => k,
        Err(resp) => return *resp,
    };
    // 1. Resolve the MAC: the body value (trimmed) wins; an empty body MAC falls
    //    back to the state file's learner-proposed value for the interface.
    let mut mac = req.mac.as_deref().unwrap_or("").trim().to_string();
    if mac.is_empty() {
        if let Some(proposed) = proposed_mac_for(state_path, &req.iface) {
            mac = proposed;
        }
    }
    if mac.is_empty() {
        return error_object(
            StatusCode::BAD_REQUEST,
            "E_NO_MAC",
            "provide a MAC, or pin a candidate that already has a proposed value",
        );
    }

    // 2. Validate against the 6-octet colon/dash form, then normalise to lowercase
    //    colon form.
    if !mac_is_valid(&mac) {
        return error_object(
            StatusCode::BAD_REQUEST,
            "E_BAD_MAC",
            format!("malformed MAC: {mac}"),
        );
    }
    let mac = normalise_mac(&mac);

    // 3. Merge the override into the config + persist. A fault is the E_PERSIST 500.
    if let Err(e) = config_set_override(config_path, &key, &mac) {
        return error_object(StatusCode::INTERNAL_SERVER_ERROR, "E_PERSIST", e);
    }
    let persisted = true;

    // 4. The optional live re-tag, gated three ways (config flag, mgmt-iface
    //    determinable, the named iface is NOT the mgmt iface).
    //
    //    The management-interface probe is resolved HERE, on the handler, and
    //    injected into the gate: `apply_now_outcome` decides over facts it is
    //    given, the way `routes::reachable_addr` takes its route table as a
    //    parameter, so both refusal branches stay testable without a host
    //    default route. It costs one read-only `ip route get` on the flag-off
    //    path (which previously short-circuited before resolving), which is the
    //    price of keeping all three gate decisions in one place. The gate is
    //    async only because the live re-tag it may reach spawns `ip` off the
    //    reactor.
    let outcome = if req.apply_now {
        let mgmt_iface = default_route_iface().await;
        apply_now_outcome(config_path, &req.iface, &mac, mgmt_iface.as_deref()).await
    } else {
        ApplyOutcome {
            applied_live: false,
            note: NOTE_PINNED_NEXT_BOOT.to_string(),
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "iface": req.iface,
            "adapterKey": key,
            "mac": mac,
            "persisted": persisted,
            "appliedLive": outcome.applied_live,
            "note": outcome.note,
        })),
    )
        .into_response()
}

/// The `apply_now` decision: refuse when the config flag is off, when the
/// management interface cannot be determined, or when the named interface IS the
/// management interface — each with its fixed note;
/// otherwise re-tag the live interface and report success (or the failure note).
///
/// `mgmt_iface` is the already-resolved default-route interface (`None` when it
/// could not be determined), injected by the caller so every refusal branch is
/// a function of its inputs.
pub(super) async fn apply_now_outcome(
    config_path: &Path,
    iface: &str,
    mac: &str,
    mgmt_iface: Option<&str>,
) -> ApplyOutcome {
    if !apply_live_allowed(config_path) {
        return ApplyOutcome {
            applied_live: false,
            note: "live re-tag not permitted (set network.mac_pin.apply_live_allowed=true); pinned for next boot"
                .to_string(),
        };
    }
    // An undeterminable route is "uncertain" and refuses the live re-tag for
    // safety (never falls through to it).
    let mgmt_iface = match mgmt_iface {
        Some(m) => m,
        None => {
            return ApplyOutcome {
                applied_live: false,
                note: "could not determine the management interface; refusing the live re-tag for safety; pinned for next boot"
                    .to_string(),
            };
        }
    };
    if iface == mgmt_iface {
        return ApplyOutcome {
            applied_live: false,
            note: format!(
                "refusing to re-tag {iface} live: it carries the management route; pinned for next boot"
            ),
        };
    }
    match apply_live(iface, mac).await {
        Ok(()) => ApplyOutcome {
            applied_live: true,
            note: "applied to the live interface now".to_string(),
        },
        Err(e) => ApplyOutcome {
            applied_live: false,
            note: format!("live re-tag failed ({e}); pinned for next boot"),
        },
    }
}
