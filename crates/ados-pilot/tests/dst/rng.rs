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

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }

    /// Zero-mean noise with standard deviation `sd` (a sum of four uniforms:
    /// bell-shaped, bounded at ±3.5 sd).
    pub fn noise(&mut self, sd: f32) -> f32 {
        let s: f32 = (0..4).map(|_| self.unit()).sum::<f32>() - 2.0;
        s * sd * 3f32.sqrt()
    }
}
