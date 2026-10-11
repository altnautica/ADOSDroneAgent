//! The in-memory phone-invite store.

use super::*;

#[derive(Debug, Clone)]
pub(super) enum InviteState {
    Pending,
    Approved(SealedToPeer),
    Rejected,
    /// Fetched or expired: the bundle no longer exists.
    Gone,
}

#[derive(Debug, Clone)]
pub(super) struct Invite {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) phone_pub: [u8; 32],
    pub(super) fingerprint: String,
    /// End of the current window: the operator's (pending) or the phone's
    /// collection window (approved).
    pub(super) expires_at_ms: i64,
    pub(super) state: InviteState,
    /// When the invite reached a terminal state.
    pub(super) terminal_at_ms: Option<i64>,
}

/// Every refusal the invite routes return.
#[derive(Debug, PartialEq, Eq)]
pub enum InviteError {
    PhonePubInvalid,
    TooManyPending,
    NotFound,
    NotPending,
    Expired,
    Material(MaterialError),
    SealFailed,
}

/// What a fetch found.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    Pending,
    Sealed(SealedToPeer),
    Rejected,
    Expired,
}

/// The in-memory invite table.
#[derive(Debug, Default)]
pub struct InviteStore {
    pub(super) invites: Vec<Invite>,
}

impl InviteStore {
    pub const fn new() -> Self {
        Self {
            invites: Vec::new(),
        }
    }

    /// Expire overdue invites and forget old terminal ones.
    pub(super) fn sweep(&mut self, now_ms: i64) {
        for inv in &mut self.invites {
            let live = matches!(inv.state, InviteState::Pending | InviteState::Approved(_));
            if live && now_ms >= inv.expires_at_ms {
                inv.state = InviteState::Gone;
                inv.terminal_at_ms = Some(now_ms);
                tracing::info!(
                    invite_id = %inv.id,
                    phone_fingerprint = %inv.fingerprint,
                    "wfb_phone_invite_expired"
                );
            }
        }
        self.invites
            .retain(|inv| inv.terminal_at_ms.is_none_or(|t| now_ms - t < TOMBSTONE_MS));
    }

    pub(super) fn find(&mut self, id: &str) -> Result<&mut Invite, InviteError> {
        self.invites
            .iter_mut()
            .find(|inv| inv.id == id)
            .ok_or(InviteError::NotFound)
    }

    /// Register a phone's public key as a pending invite.
    pub fn create(
        &mut self,
        phone_pub_b64: &str,
        label: Option<&str>,
        invite_id: String,
        now_ms: i64,
    ) -> Result<Value, InviteError> {
        self.sweep(now_ms);
        let phone_pub: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(phone_pub_b64.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(InviteError::PhonePubInvalid)?;
        // A low-order key yields an all-zero shared secret, which would make the
        // sealed bundle readable by anyone. Refuse it before it is stored.
        if seal_to_peer(&phone_pub, b"", b"").is_err() {
            return Err(InviteError::PhonePubInvalid);
        }
        let pending = self
            .invites
            .iter()
            .filter(|inv| matches!(inv.state, InviteState::Pending))
            .count();
        if pending >= MAX_PENDING {
            return Err(InviteError::TooManyPending);
        }
        let label = label
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("Phone")
            .chars()
            .filter(|c| !c.is_control())
            .take(LABEL_MAX_CHARS)
            .collect::<String>();
        let fingerprint = phone_fingerprint(&phone_pub);
        let expires_at_ms = now_ms + INVITE_TTL_MS;
        tracing::info!(
            invite_id = %invite_id,
            phone_fingerprint = %fingerprint,
            "wfb_phone_invite_created"
        );
        let body = json!({
            "invite_id": invite_id,
            "phone_fingerprint": fingerprint,
            "expires_at_ms": expires_at_ms,
        });
        self.invites.push(Invite {
            id: invite_id,
            label,
            phone_pub,
            fingerprint,
            expires_at_ms,
            state: InviteState::Pending,
            terminal_at_ms: None,
        });
        Ok(body)
    }

    /// The pending invites, oldest first.
    pub fn list_pending(&mut self, now_ms: i64) -> Value {
        self.sweep(now_ms);
        let pending: Vec<Value> = self
            .invites
            .iter()
            .filter(|inv| matches!(inv.state, InviteState::Pending))
            .map(|inv| {
                json!({
                    "invite_id": inv.id,
                    "label": inv.label,
                    "phone_fingerprint": inv.fingerprint,
                    "expires_at_ms": inv.expires_at_ms,
                })
            })
            .collect();
        json!({ "pending": pending })
    }

    pub(super) fn pending_invite(
        &mut self,
        id: &str,
        now_ms: i64,
    ) -> Result<&mut Invite, InviteError> {
        self.sweep(now_ms);
        let inv = self.find(id)?;
        match inv.state {
            InviteState::Pending => Ok(inv),
            InviteState::Gone => Err(InviteError::Expired),
            InviteState::Approved(_) | InviteState::Rejected => Err(InviteError::NotPending),
        }
    }

    /// Approve a pending invite: seal the bundle to the phone's key now.
    pub fn approve(
        &mut self,
        id: &str,
        material: Result<InviteMaterial, MaterialError>,
        now_ms: i64,
    ) -> Result<(), InviteError> {
        let inv = self.pending_invite(id, now_ms)?;
        let material = material.map_err(InviteError::Material)?;
        let sealed = seal_to_peer(
            &inv.phone_pub,
            &invite_context(&inv.id),
            &bundle_plaintext(&material),
        )
        .map_err(|_| InviteError::SealFailed)?;
        inv.state = InviteState::Approved(sealed);
        inv.expires_at_ms = now_ms + INVITE_TTL_MS;
        tracing::info!(
            invite_id = %inv.id,
            phone_fingerprint = %inv.fingerprint,
            "wfb_phone_invite_approved"
        );
        Ok(())
    }

    /// Reject a pending invite.
    pub fn reject(&mut self, id: &str, now_ms: i64) -> Result<(), InviteError> {
        let inv = self.pending_invite(id, now_ms)?;
        inv.state = InviteState::Rejected;
        inv.terminal_at_ms = Some(now_ms);
        tracing::info!(
            invite_id = %inv.id,
            phone_fingerprint = %inv.fingerprint,
            "wfb_phone_invite_rejected"
        );
        Ok(())
    }

    /// The phone's poll. A sealed bundle is handed out once, then deleted.
    pub fn fetch(&mut self, id: &str, now_ms: i64) -> Result<FetchOutcome, InviteError> {
        self.sweep(now_ms);
        let inv = self.find(id)?;
        let outcome = match std::mem::replace(&mut inv.state, InviteState::Gone) {
            InviteState::Pending => {
                inv.state = InviteState::Pending;
                return Ok(FetchOutcome::Pending);
            }
            InviteState::Rejected => {
                inv.state = InviteState::Rejected;
                return Ok(FetchOutcome::Rejected);
            }
            InviteState::Gone => return Ok(FetchOutcome::Expired),
            InviteState::Approved(sealed) => FetchOutcome::Sealed(sealed),
        };
        inv.terminal_at_ms = Some(now_ms);
        tracing::info!(
            invite_id = %inv.id,
            phone_fingerprint = %inv.fingerprint,
            "wfb_phone_invite_fetched"
        );
        Ok(outcome)
    }
}

/// The process-wide invite table.
pub(super) static STORE: parking_lot::Mutex<InviteStore> =
    parking_lot::Mutex::new(InviteStore::new());
