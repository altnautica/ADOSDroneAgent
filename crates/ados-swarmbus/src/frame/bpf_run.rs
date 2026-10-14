//! A userspace evaluator for the classic-BPF program [`super::bpf_program`]
//! emits.
//!
//! The loopback transport has no kernel filter to attach, so it runs the same
//! instruction array here instead of restating the predicate by hand. A frame is
//! accepted in userspace exactly when the kernel would have accepted it, and a
//! change to the program changes both backends at once.
//!
//! Only the opcodes the swarm program uses are implemented. Anything else, a load
//! past the end of the frame, or a program that runs off its end rejects the frame
//! (returns 0), which is what the kernel does for an out-of-bounds load; the
//! kernel would refuse an unsupported program at attach time, and failing closed
//! is the userspace equivalent.

use super::bpf::{ALU_LSH_K, ALU_OR_X, JMP_JEQ_K, LD_B_ABS, LD_H_IND, LD_W_IND, MISC_TAX, RET_K};
use super::SockFilter;

/// Run `program` over `frame` with the kernel's socket-filter semantics.
///
/// Returns the number of bytes the filter keeps: `min(ret, frame.len())`, so 0
/// means the frame is dropped.
pub fn bpf_run(program: &[SockFilter], frame: &[u8]) -> usize {
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut pc = 0usize;
    while let Some(ins) = program.get(pc) {
        pc += 1;
        match ins.code {
            LD_B_ABS => match load(frame, ins.k as usize, 1) {
                Some(v) => a = v,
                None => return 0,
            },
            LD_H_IND | LD_W_IND => {
                let width = if ins.code == LD_H_IND { 2 } else { 4 };
                let Some(off) = (x as usize).checked_add(ins.k as usize) else {
                    return 0;
                };
                match load(frame, off, width) {
                    Some(v) => a = v,
                    None => return 0,
                }
            }
            ALU_LSH_K => a = a.checked_shl(ins.k).unwrap_or(0),
            ALU_OR_X => a |= x,
            MISC_TAX => x = a,
            JMP_JEQ_K => pc += usize::from(if a == ins.k { ins.jt } else { ins.jf }),
            RET_K => return (ins.k as usize).min(frame.len()),
            _ => return 0,
        }
    }
    0
}

/// A big-endian load of `width` bytes at `off`, or `None` past the end.
fn load(frame: &[u8], off: usize, width: usize) -> Option<u32> {
    let bytes = frame.get(off..off.checked_add(width)?)?;
    Some(bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{
        bpf_program, build_frame, parse_frame, radiotap_header, FLEET_OFFSET, FRAME_HEADER_LEN,
        IEEE80211_HDR_LEN, MAGIC_OFFSET, RADIOTAP_TX_LEN, SWARM_MAGIC, WFB_MAGIC,
    };

    /// A frame with an arbitrary magic and fleet word in the transmitter address.
    fn frame_with(magic: u16, fleet: u32) -> Vec<u8> {
        let mut f = radiotap_header(0).to_vec();
        let mut mac = [0u8; IEEE80211_HDR_LEN];
        mac[0] = 0x08;
        mac[1] = 0x01;
        mac[MAGIC_OFFSET..MAGIC_OFFSET + 2].copy_from_slice(&magic.to_be_bytes());
        mac[FLEET_OFFSET..FLEET_OFFSET + 4].copy_from_slice(&fleet.to_be_bytes());
        f.extend_from_slice(&mac);
        f.extend_from_slice(&[0u8; 50]);
        f
    }

    #[test]
    fn our_own_fleet_is_accepted_whole() {
        let frame = build_frame(7, 3, &[0xAB; 50]);
        assert_eq!(bpf_run(&bpf_program(7), &frame), frame.len());
    }

    #[test]
    fn a_foreign_fleet_and_a_foreign_magic_are_rejected() {
        let p = bpf_program(1);
        assert_eq!(
            bpf_run(&p, &build_frame(2, 0, &[0; 50])),
            0,
            "foreign fleet"
        );
        assert_eq!(bpf_run(&p, &frame_with(WFB_MAGIC, 1)), 0, "wfb-ng magic");
        assert_eq!(bpf_run(&p, &frame_with(0x0000, 1)), 0, "no magic");
    }

    /// The software filter and the userspace parser agree on every combination of
    /// magic, sender fleet and receiver fleet, so the loopback backend drops in
    /// the filter exactly the frames the kernel would have dropped.
    #[test]
    fn the_filter_agrees_with_the_parser_on_magic_and_fleet() {
        for magic in [SWARM_MAGIC, WFB_MAGIC, 0xAD04] {
            for sender in [0u32, 1, 2, 0xFFFF, 0x0001_0001] {
                let frame = frame_with(magic, sender);
                for receiver in [1u16, 2, 0xFFFF] {
                    let kept = bpf_run(&bpf_program(receiver), &frame) != 0;
                    let parsed = parse_frame(&frame, receiver).is_ok();
                    assert_eq!(kept, parsed, "magic {magic:#x} {sender} -> {receiver}");
                }
            }
        }
    }

    /// A load past the end rejects rather than reading garbage or panicking. The
    /// last byte the program reads is the end of the fleet word.
    #[test]
    fn a_truncated_frame_is_rejected_where_the_kernel_would_reject_it() {
        let frame = build_frame(1, 0, b"body");
        let p = bpf_program(1);
        let needed = RADIOTAP_TX_LEN + FLEET_OFFSET + 4;
        for cut in 0..FRAME_HEADER_LEN {
            let kept = bpf_run(&p, &frame[..cut]);
            assert_eq!(kept != 0, cut >= needed, "a {cut}-byte frame");
        }
    }

    /// The radiotap length is read as a 16-bit little-endian value, so a header
    /// longer than 255 bytes still locates the MAC header.
    #[test]
    fn a_long_radiotap_header_is_walked_by_its_declared_length() {
        let rt_len = 300usize;
        let mut frame = vec![0u8; rt_len];
        frame[2..4].copy_from_slice(&(rt_len as u16).to_le_bytes());
        frame.extend_from_slice(&build_frame(9, 0, b"x")[RADIOTAP_TX_LEN..]);
        assert_eq!(bpf_run(&bpf_program(9), &frame), frame.len());
        assert_eq!(bpf_run(&bpf_program(8), &frame), 0);
    }

    #[test]
    fn an_unknown_opcode_or_a_missing_return_fails_closed() {
        let frame = build_frame(1, 0, b"body");
        let bogus = [SockFilter {
            code: 0xFF,
            jt: 0,
            jf: 0,
            k: u32::MAX,
        }];
        assert_eq!(bpf_run(&bogus, &frame), 0);
        assert_eq!(bpf_run(&bpf_program(1)[..10], &frame), 0, "no ret reached");
        assert_eq!(bpf_run(&[], &frame), 0);
    }

    #[test]
    fn the_return_value_caps_at_the_frame_length() {
        let frame = build_frame(1, 0, b"body");
        let snap = [SockFilter {
            code: RET_K,
            jt: 0,
            jf: 0,
            k: 16,
        }];
        assert_eq!(bpf_run(&snap, &frame), 16);
        assert_eq!(bpf_run(&snap, &frame[..10]), 10);
    }
}
