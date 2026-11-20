//! The run's one source of randomness: SplitMix64, seeded per run, so a seed
//! names a run exactly.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f32) -> bool {
        self.unit() < p
    }

    /// Uniform milliseconds in `[lo, hi]`.
    pub fn ms(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }

    /// Uniform milliseconds in `[lo_s, hi_s]` seconds.
    pub fn secs(&mut self, (lo_s, hi_s): (f32, f32)) -> u64 {
        self.ms((lo_s * 1000.0) as u64, (hi_s * 1000.0) as u64)
    }

    /// One element of `items`.
    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.next_u64() % items.len() as u64) as usize]
    }
}
