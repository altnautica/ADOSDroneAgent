//! What the setup engine reads of the pilot.

/// What the setup engine watches: readiness, the surface link and what its
/// take-off needs for a supervised lift-off.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetupView {
    pub readiness: Vec<ados_protocol::pilot::Readiness>,
    /// No operator surface heard from for `pilot.link_loss_s`.
    pub link_lost: bool,
    /// `pilot.place_class`: the `ados_profile` step writes its values.
    pub place_class: ados_protocol::pilot::PlaceClass,
    /// `pilot.rc_receiver`: the `ados_profile` step writes its values.
    pub rc_receiver: ados_protocol::pilot::RcReceiver,
    /// The obstacle margin the pilot flies: the `ados_profile` step writes
    /// the FC's proximity stop inside it.
    pub obstacle_margin_m: f32,
    /// `pilot.supervised_liftoff`.
    pub supervised_liftoff: bool,
    /// The airframe's learned trim for the connected FC.
    pub liftoff_trim: Option<crate::airframe::Trim>,
    /// The hover thrust a lift-off ramps against.
    pub liftoff_hover: f32,
    /// The airframe record to file under the FC identity (`None` until the
    /// identity is known); filed again whenever its `rev` moves.
    pub airframe: Option<crate::airframe::Save>,
}
