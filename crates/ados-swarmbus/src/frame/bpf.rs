//! The in-kernel classic-BPF filter that picks this fleet's swarm frames out of
//! the video stream sharing the same adapter.
//!
//! Hand-assembled rather than compiled by libpcap because the receive socket is a
//! raw `AF_PACKET` one, not a pcap handle — which is what keeps this crate free of
//! a C library dependency and cross-buildable to the musl SBC target.
//!
//! Doing the match in the kernel rather than userspace is the point: the adapter
//! carries a ~700 packet/s video stream that must never cross the syscall boundary
//! just to be discarded.

use super::{FLEET_OFFSET, MAGIC_OFFSET, SWARM_MAGIC};

/// One classic-BPF instruction, laid out as the kernel's `struct sock_filter`.
///
/// Defined here rather than pulled from `libc` so [`bpf_program`] is a pure
/// function that compiles and is unit-tested on any host; the Linux receive path
/// casts a slice of these straight into `SO_ATTACH_FILTER`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// Instruction count of the program [`bpf_program`] emits.
pub const BPF_PROGRAM_LEN: usize = 12;

// Classic BPF opcode composition, spelled out so the pinned program below can be
// read against the kernel's `bpf_common.h` without a decoder ring.
const LD_B_ABS: u16 = 0x30; // BPF_LD  | BPF_B | BPF_ABS
const LD_H_IND: u16 = 0x48; // BPF_LD  | BPF_H | BPF_IND
const LD_W_IND: u16 = 0x40; // BPF_LD  | BPF_W | BPF_IND
const ALU_LSH_K: u16 = 0x64; // BPF_ALU | BPF_LSH | BPF_K
const ALU_OR_X: u16 = 0x4c; // BPF_ALU | BPF_OR  | BPF_X
const MISC_TAX: u16 = 0x07; // BPF_MISC | BPF_TAX
const JMP_JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

/// Build the kernel filter that accepts only this fleet's swarm frames.
///
/// The equivalent of wfb-ng's pcap expression `ether[0x0a:2]==0xAD03 &&
/// ether[0x0c:4]==<fleet_id>` (`vendor/wfb-ng/src/rx.cpp:84`).
///
/// The one subtlety is that the radiotap header is **variable length**, so the MAC
/// header's position is not a constant. The first six instructions read radiotap's
/// little-endian `it_len` into the index register — byte-wise, because classic BPF
/// loads are big-endian and there is no byte-swap opcode — after which the two
/// comparisons are index-relative loads at exactly the offsets a pcap `ether[]`
/// expression compiles to.
pub fn bpf_program(fleet_id: u16) -> [SockFilter; BPF_PROGRAM_LEN] {
    let stmt = |code: u16, k: u32| SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code: u16, k: u32, jt: u8, jf: u8| SockFilter { code, jt, jf, k };

    [
        // A = it_len high byte (radiotap's length is little-endian).
        stmt(LD_B_ABS, 3),
        stmt(ALU_LSH_K, 8),
        // X = high << 8.
        stmt(MISC_TAX, 0),
        // A = it_len low byte, then A |= X, giving A = it_len.
        stmt(LD_B_ABS, 2),
        stmt(ALU_OR_X, 0),
        // X = it_len: the MAC header's offset.
        stmt(MISC_TAX, 0),
        // ether[0x0a:2] == SWARM_MAGIC, else jump 3 forward to the reject ret.
        stmt(LD_H_IND, MAGIC_OFFSET as u32),
        jump(JMP_JEQ_K, SWARM_MAGIC as u32, 0, 3),
        // ether[0x0c:4] == fleet_id, else jump 1 forward to the reject ret.
        stmt(LD_W_IND, FLEET_OFFSET as u32),
        jump(JMP_JEQ_K, fleet_id as u32, 0, 1),
        // Accept the whole frame / reject it.
        stmt(RET_K, u32::MAX),
        stmt(RET_K, 0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program is pinned instruction by instruction. A wrong opcode or jump
    /// offset does not fail loudly on a live socket — the kernel accepts a
    /// valid-but-wrong program and the bus simply goes deaf, which is
    /// indistinguishable from "no drones in range".
    #[test]
    fn the_program_is_pinned_instruction_by_instruction() {
        let p = bpf_program(0x1234);
        assert_eq!(p.len(), BPF_PROGRAM_LEN);
        #[rustfmt::skip]
        let expected = [
            SockFilter { code: 0x30, jt: 0, jf: 0, k: 3 },
            SockFilter { code: 0x64, jt: 0, jf: 0, k: 8 },
            SockFilter { code: 0x07, jt: 0, jf: 0, k: 0 },
            SockFilter { code: 0x30, jt: 0, jf: 0, k: 2 },
            SockFilter { code: 0x4c, jt: 0, jf: 0, k: 0 },
            SockFilter { code: 0x07, jt: 0, jf: 0, k: 0 },
            SockFilter { code: 0x48, jt: 0, jf: 0, k: 0x0a },
            SockFilter { code: 0x15, jt: 0, jf: 3, k: 0xAD03 },
            SockFilter { code: 0x40, jt: 0, jf: 0, k: 0x0c },
            SockFilter { code: 0x15, jt: 0, jf: 1, k: 0x1234 },
            SockFilter { code: 0x06, jt: 0, jf: 0, k: u32::MAX },
            SockFilter { code: 0x06, jt: 0, jf: 0, k: 0 },
        ];
        assert_eq!(p, expected);
    }

    /// Every jump target must be in range, and must be either the immediate next
    /// instruction (a fall-through, which classic BPF spells as an offset of 0) or a
    /// terminating `ret`. An off-by-one jump offset yields a program that either
    /// always accepts (the adapter's whole video stream floods userspace) or always
    /// rejects (the bus is silently deaf) — neither of which the kernel rejects,
    /// because both are valid programs.
    #[test]
    fn every_jump_target_is_in_range_and_terminates_or_falls_through() {
        let p = bpf_program(7);
        let accept = BPF_PROGRAM_LEN - 2;
        let reject = BPF_PROGRAM_LEN - 1;
        assert_eq!(p[accept].k, u32::MAX, "accept returns the whole frame");
        assert_eq!(p[reject].k, 0, "reject returns nothing");
        for (i, ins) in p.iter().enumerate() {
            if ins.code != JMP_JEQ_K {
                continue;
            }
            for target in [i + 1 + ins.jt as usize, i + 1 + ins.jf as usize] {
                assert!(target < BPF_PROGRAM_LEN, "jump from {i} runs off the end");
                assert!(
                    target == i + 1 || p[target].code == RET_K,
                    "jump from {i} to {target} neither falls through nor terminates"
                );
            }
        }
        // The magic compare falls through to the fleet compare; the fleet compare
        // falls through to accept; both failures reach the one shared reject.
        assert_eq!(
            7 + 1 + p[7].jt as usize,
            8,
            "magic ok -> load the fleet word"
        );
        assert_eq!(9 + 1 + p[9].jt as usize, accept, "fleet ok -> accept");
        assert_eq!(7 + 1 + p[7].jf as usize, reject, "wrong magic -> reject");
        assert_eq!(9 + 1 + p[9].jf as usize, reject, "wrong fleet -> reject");
        // Nothing can reach `accept` except the fleet compare falling through, so a
        // frame cannot be accepted without both comparisons having passed.
        assert!(
            p[accept - 1].code == JMP_JEQ_K,
            "accept is guarded by a compare"
        );
    }

    /// The filter and the userspace parser must read the same two offsets, or a
    /// frame the kernel accepted would be rejected in userspace (or worse).
    #[test]
    fn the_filter_compares_the_offsets_the_parser_reads() {
        let p = bpf_program(42);
        assert_eq!(p[6].k as usize, MAGIC_OFFSET);
        assert_eq!(p[7].k, SWARM_MAGIC as u32);
        assert_eq!(p[8].k as usize, FLEET_OFFSET);
        assert_eq!(p[9].k, 42);
    }

    /// Only the fleet word varies between programs; everything else is fixed. A
    /// second fleet on the same channel differs by exactly one instruction.
    #[test]
    fn only_the_fleet_word_differs_between_two_fleets() {
        let a = bpf_program(1);
        let b = bpf_program(2);
        let differing: Vec<usize> = (0..BPF_PROGRAM_LEN).filter(|&i| a[i] != b[i]).collect();
        assert_eq!(differing, vec![9]);
    }

    /// `sock_filter` is passed to the kernel as a packed array; a padded or
    /// reordered layout would make the kernel read garbage opcodes.
    #[test]
    fn the_instruction_layout_matches_the_kernel_struct() {
        assert_eq!(std::mem::size_of::<SockFilter>(), 8);
        assert_eq!(std::mem::align_of::<SockFilter>(), 4);
    }
}
