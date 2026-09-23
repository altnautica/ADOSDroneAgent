//! MAVLink outbound classification: the pose-inject and VIO id tables, the
//! whole-buffer frame scan, and the per-frame source-component gate.

use super::*;

// ---------------------------------------------------------------------
// MAVLink classification constants (host_services.py)
// ---------------------------------------------------------------------

/// Message ids the pose-injection path covers. A send whose msg id is in this set
/// demands `estimator.pose.inject` on top of `mavlink.write`.
///
/// Membership is decided by what the message REACHES, not by its name: every id
/// here lands in the flight controller's state estimator, where a fabricated
/// sample moves the vehicle's idea of where it is. `mavlink.write` alone is the
/// permission to talk to the FC; this one is the permission to tell it where it
/// is. An estimator message missing from the set is a plugin holding only the
/// former that can corrupt the position solution of an armed aircraft.
pub const POSE_INJECT_MSG_IDS: &[u32] = &[
    331,   // ODOMETRY
    101,   // GLOBAL_VISION_POSITION_ESTIMATE
    102,   // VISION_POSITION_ESTIMATE
    103,   // VISION_SPEED_ESTIMATE (external-nav velocity)
    11011, // VISION_POSITION_DELTA
    104,   // VICON_POSITION_ESTIMATE
    138,   // ATT_POS_MOCAP (vicon-equivalent attitude path)
    232,   // GPS_INPUT (a MAVLink GPS the estimator fuses)
    113,   // HIL_GPS (the same GPS driver, HIL form)
];

/// Component ids the VIO permission covers. Registering one of these requires
/// `mavlink.component.vio` on top of the matching component kind.
pub const VIO_COMPONENT_IDS: &[i64] = &[197, 198];

// ---------------------------------------------------------------------
// MAVLink frame classification
// ---------------------------------------------------------------------

/// The response for a command that did not reach the router socket, or that the
/// router's PIC gate would drop (`pic_refused`, while an operator holds manual
/// control): `sent: false` plus the reason, so a plugin never reads a dropped
/// command as delivered. A graceful-degrade map like `not_available`, not an
/// error.
pub(super) fn send_refused(err: SendError) -> HostResult {
    Value::Map(vec![
        (Value::from("sent"), Value::Boolean(false)),
        (Value::from("reason"), Value::from(err.reason())),
    ])
}

/// Best-effort MAVLink message id from a raw frame header. Returns `None` when
/// the frame is too short to classify.
pub(crate) fn mavlink_msg_id(frame: &[u8]) -> Option<u32> {
    let stx = *frame.first()?;
    if stx == 0xFD && frame.len() >= 10 {
        // v2: bytes 7..10 little-endian 24-bit msgid.
        let mut id = [0u8; 4];
        id[..3].copy_from_slice(&frame[7..10]);
        return Some(u32::from_le_bytes(id));
    }
    if stx == 0xFE && frame.len() >= 6 {
        // v1: byte 5 is the 8-bit msgid.
        return Some(frame[5] as u32);
    }
    None
}

/// The source component id a frame's header claims (v2 byte 6, v1 byte 4).
/// `None` when the frame is too short to carry one.
pub(super) fn mavlink_component_id(frame: &[u8]) -> Option<u8> {
    match *frame.first()? {
        0xFD => frame.get(6).copied(),
        0xFE => frame.get(4).copied(),
        _ => None,
    }
}

/// What a scan of an outbound buffer found across all of its frames.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OutboundFrames {
    /// At least one frame carries a pose-injection message id.
    pub requires_pose_cap: bool,
    /// Every source component id the frame headers claim.
    pub component_ids: BTreeSet<u8>,
}

/// Classify EVERY frame in an outbound buffer, not just the one at offset 0.
/// `None` when the buffer is not a run of whole frames that each decode on their
/// own, so it cannot be classified; the caller refuses it rather than guessing.
///
/// `mavlink.send` accepts a buffer, and the router forwards it verbatim without
/// parsing — a documented, tested property, since splitting it would corrupt
/// batched traffic. So a caller may hand over several concatenated frames, and
/// reading only the first message id let a plugin holding `mavlink.write` but
/// not `estimator.pose.inject` put one benign frame in front of any number of
/// pose frames: the gate classified the benign id, allowed the send, and every
/// pose reached the flight controller's state estimator. The capability's own
/// description names that as a fly-away risk, and the whole buffer fits
/// hundreds of pose frames. The source component id is read from every header
/// for the same reason: the component gates apply to what each frame claims to
/// be, not to what the caller declares.
///
/// The walk splits on header lengths ([`ados_protocol::aux_mux::split_frames`]),
/// then every piece must decode as exactly one frame of the dialect
/// ([`ados_protocol::mavlink::decode_exact_frame`]): right length, known id,
/// payload within the message, matching checksum, and no incompat flag beyond
/// signing. The reason is the receiver. The flight controller's parser drops a
/// frame it rejects after a byte or two and rescans the rest for a start byte,
/// so any frame it would reject is a container: a benign-looking outer header
/// with a foreign incompat flag or a bad checksum can carry a whole pose frame,
/// or a frame stamped with a reserved component, in its payload, and the FC
/// would take the inner one. Only a frame the FC consumes whole is classified
/// by its own header; everything else is refused.
///
/// A trailing partial frame is unclassifiable for the same reason: the router
/// forwards the buffer INCLUDING that remainder, so treating an unparseable
/// tail as empty would reopen the hole one truncation further along.
pub(crate) fn scan_outbound(msg_bytes: &[u8]) -> Option<OutboundFrames> {
    let frames = ados_protocol::aux_mux::split_frames(msg_bytes);
    let consumed: usize = frames.iter().map(|f| f.len()).sum();
    if consumed != msg_bytes.len() {
        return None;
    }
    let mut scan = OutboundFrames {
        requires_pose_cap: false,
        component_ids: BTreeSet::new(),
    };
    for frame in frames {
        ados_protocol::mavlink::decode_exact_frame(frame).ok()?;
        // The frame decoded whole, so its header is the message it carries.
        let id = mavlink_msg_id(frame).unwrap_or(u32::MAX);
        scan.requires_pose_cap |= POSE_INJECT_MSG_IDS.contains(&id);
        scan.component_ids.insert(mavlink_component_id(frame)?);
    }
    Some(scan)
}

impl RealHost {
    /// Gate the source component ids a `mavlink.send` buffer's frames claim.
    /// A VIO id (see [`VIO_COMPONENT_IDS`]) needs `mavlink.component.vio` and a
    /// reservation this plugin holds; any id another plugin reserved is refused,
    /// so one plugin cannot speak as another's registered component.
    pub(super) fn check_frame_components(
        &self,
        plugin_id: &str,
        component_ids: &BTreeSet<u8>,
        granted_caps: &BTreeSet<String>,
    ) -> Result<(), HostError> {
        let components = self.components.lock().expect("components mutex poisoned");
        for &id in component_ids {
            let comp_id = i64::from(id);
            let vio = VIO_COMPONENT_IDS.contains(&comp_id);
            if vio && !granted_caps.contains("mavlink.component.vio") {
                return Err(HostError::CapabilityDenied(
                    "mavlink.component.vio".to_string(),
                ));
            }
            match components.holder(comp_id) {
                Some(holder) if holder != plugin_id => {
                    return Err(HostError::Rpc(format!(
                        "component_id {comp_id} already reserved by {holder}"
                    )));
                }
                None if vio => {
                    return Err(HostError::Rpc(format!(
                        "component_id {comp_id} not reserved by {plugin_id}; \
                         call mavlink.register_component first"
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }
}
