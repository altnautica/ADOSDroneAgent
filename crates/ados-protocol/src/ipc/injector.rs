//! The autonomous-injector declaration a writer sends on a command socket.

/// Control-frame prefix a writer uses to declare that it is an AUTONOMOUS
/// INJECTOR: a producer of commands generated on the node rather than by a
/// human operator (the plugin host driving guided setpoints, the MSP relay).
/// The rest of the payload is the writer's `client_id`, optionally followed by
/// `;ticket=<ticket>` — the attestation the FC-write path verifies against the
/// pairing key so a spoofed `client_id` cannot claim the injector lane.
///
/// Like [`super::IPC_DECLARE_OFF_BOX_SOURCE`] this is one-way and downward: declaring
/// injector can only ever SUBJECT the writer to the PIC arbiter (an operator
/// holding the manual-control claim refuses it), never grant it authority. A
/// writer that does not declare is treated as the operator/human path and is
/// NEVER gated — the load-bearing invariant. Sticky for the connection, never
/// forwarded to the flight controller.
pub const IPC_DECLARE_INJECTOR_PREFIX: &[u8] = b"\x00ados-ipc/injector=";

/// A writer's declared autonomous-injector identity (see
/// [`IPC_DECLARE_INJECTOR_PREFIX`]). Carried raw to the FC-write path, which
/// verifies `ticket` against the pairing key before treating `client_id` as the
/// attested injector; a `None` ticket is an unverified claim that can never win
/// the lane away from a human holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectorClaim {
    pub client_id: String,
    pub ticket: Option<String>,
}

/// Build the [`IPC_DECLARE_INJECTOR_PREFIX`] control frame a writer sends to
/// declare itself an autonomous injector. The inverse of
/// [`parse_injector_declaration`], kept beside it so a producer's declaration
/// and the router's parse cannot drift on the wire format. `ticket` is the
/// pairing-key attestation for `client_id`; omit it on an unpaired node.
pub fn encode_injector_declaration(client_id: &str, ticket: Option<&str>) -> Vec<u8> {
    let mut out = IPC_DECLARE_INJECTOR_PREFIX.to_vec();
    out.extend_from_slice(client_id.as_bytes());
    if let Some(t) = ticket {
        out.extend_from_slice(b";ticket=");
        out.extend_from_slice(t.as_bytes());
    }
    out
}

/// Parse an [`IPC_DECLARE_INJECTOR_PREFIX`] control frame into an
/// [`InjectorClaim`]. `None` when the payload is not an injector declaration or
/// carries an empty client id (a declaration with no identity is not a claim).
pub fn parse_injector_declaration(payload: &[u8]) -> Option<InjectorClaim> {
    let body = payload.strip_prefix(IPC_DECLARE_INJECTOR_PREFIX)?;
    let text = std::str::from_utf8(body).ok()?;
    let (client_id, ticket) = match text.split_once(";ticket=") {
        Some((id, tkt)) => (id, (!tkt.is_empty()).then(|| tkt.to_string())),
        None => (text, None),
    };
    if client_id.is_empty() {
        return None;
    }
    Some(InjectorClaim {
        client_id: client_id.to_string(),
        ticket,
    })
}
