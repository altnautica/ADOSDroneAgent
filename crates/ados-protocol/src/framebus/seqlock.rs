//! Volatile access to the per-slot seq guard of the shared frame ring.

/// Write `value` into `region[off..off+8]` as a little-endian u64 through
/// per-byte volatile stores. Volatile prevents the compiler from eliding or
/// reordering the seq-guard stamps relative to the plain data copy, which the
/// surrounding `fence`s then order across CPUs for a cross-process reader. The
/// region is an `mmap` shared with a separate reader process, so a plain
/// `copy_from_slice` of the guard is not sufficient on a weakly-ordered CPU.
#[inline]
pub(super) fn store_seq_volatile(region: &mut [u8], off: usize, value: u64) {
    let bytes = value.to_le_bytes();
    let base = region.as_mut_ptr();
    for (i, b) in bytes.iter().enumerate() {
        // SAFETY: the caller validated `off + 8 <= region.len()` (every call
        // site sizes the slot via `check_region` first), so `off + i` is in
        // bounds for `i < 8`.
        unsafe { base.add(off + i).write_volatile(*b) };
    }
}

/// Read `region[off..off+8]` as a little-endian u64 through per-byte volatile
/// loads. The counterpart to [`store_seq_volatile`]; volatile keeps the guard
/// loads from being hoisted or fused with the data copy, and the caller pairs
/// them with `fence(Acquire)`.
#[inline]
pub(super) fn load_seq_volatile(region: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    let base = region.as_ptr();
    for (i, b) in bytes.iter_mut().enumerate() {
        // SAFETY: the caller validated `off + 8 <= region.len()`, so `off + i`
        // is in bounds for `i < 8`.
        *b = unsafe { base.add(off + i).read_volatile() };
    }
    u64::from_le_bytes(bytes)
}
