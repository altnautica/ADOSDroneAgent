//! Who commands motion (C2 authority table) and the single motion lease (C1).

use ados_protocol::pilot::{Goal, Holder, LeaseRevoked, Owner};

/// A lease with no goal for this long is revoked.
pub const LEASE_GOAL_TIMEOUT_MS: u64 = 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub id: u64,
    pub holder: Holder,
    pub granted_ms: u64,
    pub last_goal_ms: u64,
    pub goal: Option<Goal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthErr {
    Busy,
    NotArmed,
    WrongOwner(Owner),
}

#[derive(Debug, Clone)]
pub struct Authority {
    owner: Owner,
    lease: Option<Lease>,
    next_id: u64,
    /// Owner to return to when tuning ends.
    before_tuning: Option<Owner>,
}

impl Default for Authority {
    fn default() -> Self {
        Self {
            owner: Owner::Idle,
            lease: None,
            next_id: 1,
            before_tuning: None,
        }
    }
}

impl Authority {
    pub fn owner(&self) -> Owner {
        self.owner
    }

    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    pub fn lease_mut(&mut self) -> Option<&mut Lease> {
        self.lease.as_mut()
    }

    /// Change owner. Leaving `Skill` drops the lease and reports the
    /// revocation for the holder.
    pub fn set_owner(&mut self, owner: Owner, reason: &str) -> Option<LeaseRevoked> {
        if self.owner == Owner::Tuning && owner != Owner::Tuning {
            self.before_tuning = None;
        }
        self.owner = owner;
        if owner != Owner::Skill {
            return self.revoke(reason);
        }
        None
    }

    /// Grant the lease (caller checked the preconditions).
    pub fn grant(&mut self, holder: Holder, now_ms: u64) -> Result<u64, (AuthErr, Option<Holder>)> {
        if let Some(l) = &self.lease {
            return Err((AuthErr::Busy, Some(l.holder.clone())));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.lease = Some(Lease {
            id,
            holder,
            granted_ms: now_ms,
            last_goal_ms: now_ms,
            goal: None,
        });
        self.owner = Owner::Skill;
        Ok(id)
    }

    /// Release by the holder; true if it was the live lease.
    pub fn release(&mut self, lease_id: u64) -> bool {
        if self.lease.as_ref().is_some_and(|l| l.id == lease_id) {
            self.lease = None;
            if self.owner == Owner::Skill {
                self.owner = Owner::Pilot;
            }
            true
        } else {
            false
        }
    }

    /// Drop the lease, if any, reporting it.
    pub fn revoke(&mut self, reason: &str) -> Option<LeaseRevoked> {
        let l = self.lease.take()?;
        if self.owner == Owner::Skill {
            self.owner = Owner::Pilot;
        }
        Some(LeaseRevoked {
            lease_id: l.id,
            reason: reason.to_owned(),
        })
    }

    /// Store a goal from the holder. False when `lease_id` is not live.
    pub fn goal(&mut self, lease_id: u64, goal: Goal, now_ms: u64) -> bool {
        match &mut self.lease {
            Some(l) if l.id == lease_id => {
                l.goal = Some(goal);
                l.last_goal_ms = now_ms;
                true
            }
            _ => false,
        }
    }

    /// Revoke a lease whose goals stopped.
    pub fn expire(&mut self, now_ms: u64) -> Option<LeaseRevoked> {
        let stale = self
            .lease
            .as_ref()
            .is_some_and(|l| now_ms.saturating_sub(l.last_goal_ms) > LEASE_GOAL_TIMEOUT_MS);
        if stale {
            self.revoke("goal-timeout")
        } else {
            None
        }
    }

    /// Enter the setup wizard's tuning owner (AUTOTUNE modes). The pilot stops
    /// sending setpoints and hands operator sticks to the setup engine.
    pub fn enter_tuning(&mut self) -> Result<Option<LeaseRevoked>, AuthErr> {
        if self.owner == Owner::Tuning {
            return Ok(None);
        }
        self.before_tuning = Some(self.owner);
        let revoked = self.revoke("tuning");
        self.owner = Owner::Tuning;
        Ok(revoked)
    }

    /// Leave tuning; the owner goes back to what it was, or `FcMode`.
    pub fn exit_tuning(&mut self) {
        if self.owner == Owner::Tuning {
            self.owner = match self.before_tuning.take() {
                Some(Owner::Skill) | None => Owner::FcMode,
                Some(o) => o,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder() -> Holder {
        Holder {
            plugin_id: "p".into(),
            skill_id: "s".into(),
        }
    }

    #[test]
    fn one_lease_at_a_time() {
        let mut a = Authority::default();
        let id = a.grant(holder(), 0).unwrap();
        assert_eq!(a.owner(), Owner::Skill);
        assert_eq!(a.grant(holder(), 0).unwrap_err().0, AuthErr::Busy);
        assert!(a.release(id));
        assert_eq!(a.owner(), Owner::Pilot);
        assert!(!a.release(id));
    }

    #[test]
    fn owner_change_revokes_with_reason() {
        let mut a = Authority::default();
        let id = a.grant(holder(), 0).unwrap();
        let r = a.set_owner(Owner::FcMode, "fc-mode").unwrap();
        assert_eq!((r.lease_id, r.reason.as_str()), (id, "fc-mode"));
        assert!(a.lease().is_none());
    }

    #[test]
    fn silent_lease_expires_after_one_second() {
        let mut a = Authority::default();
        let id = a.grant(holder(), 0).unwrap();
        assert!(a.expire(900).is_none());
        let r = a.expire(1001).unwrap();
        assert_eq!((r.lease_id, r.reason.as_str()), (id, "goal-timeout"));
    }

    #[test]
    fn tuning_round_trip() {
        let mut a = Authority::default();
        a.set_owner(Owner::Pilot, "");
        a.enter_tuning().unwrap();
        assert_eq!(a.owner(), Owner::Tuning);
        a.exit_tuning();
        assert_eq!(a.owner(), Owner::Pilot);
    }
}
