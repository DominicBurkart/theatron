//! A minimal, deterministic PRNG for reproducible simulations.
//!
//! # Examples
//!
//! ```
//! use theatron::prng::Xorshift64;
//! use rand_core::RngCore;
//!
//! let mut rng = Xorshift64::new(42);
//! let a = rng.next_u64();
//! let mut rng2 = Xorshift64::new(42);
//! assert_eq!(a, rng2.next_u64());
//! ```

use rand_core::{Error, RngCore, impls};

/// A fast, deterministic 64-bit xorshift PRNG.
///
/// Suitable for simulation use where reproducibility from a seed matters
/// more than cryptographic strength. Seed 0 is mapped to 1 to avoid the
/// fixed point.
#[derive(Clone, Copy)]
pub struct Xorshift64(u64);

impl Xorshift64 {
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }
}

impl RngCore for Xorshift64 {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        impls::fill_bytes_via_next(self, dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn seed_zero_becomes_one() {
        let mut a = Xorshift64::new(0);
        let mut b = Xorshift64::new(1);
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn deterministic_sequence() {
        let mut a = Xorshift64::new(42);
        let mut b = Xorshift64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn nonzero_output() {
        let mut rng = Xorshift64::new(1);
        for _ in 0..1000 {
            assert_ne!(rng.next_u64(), 0);
        }
    }

    #[test]
    fn fill_bytes_deterministic() {
        let mut a = Xorshift64::new(7);
        let mut b = Xorshift64::new(7);
        let mut buf_a = [0u8; 16];
        let mut buf_b = [0u8; 16];
        a.fill_bytes(&mut buf_a);
        b.fill_bytes(&mut buf_b);
        assert_eq!(buf_a, buf_b);
    }

    /// Pins the exact output sequence for seed 1 against the (<<13, >>7, <<17) triplet.
    /// Any accidental change to the shift constants will break this test, protecting
    /// the reproducibility guarantee for saved simulation seeds.
    #[test]
    fn known_sequence() {
        let mut rng = Xorshift64::new(1);
        assert_eq!(rng.next_u64(), 1_082_269_761);
        assert_eq!(rng.next_u64(), 1_152_992_998_833_853_505);
        assert_eq!(rng.next_u64(), 11_177_516_664_432_764_457);
    }

    /// `next_u32` truncates the upper 32 bits of `next_u64`.
    #[test]
    fn next_u32_consistent_with_next_u64() {
        let mut rng_u32 = Xorshift64::new(5);
        let mut rng_u64 = Xorshift64::new(5);
        // next_u32 must equal the low 32 bits of next_u64
        assert_eq!(rng_u32.next_u32(), rng_u64.next_u64() as u32);
    }

    /// `try_fill_bytes` must succeed and produce the same bytes as `fill_bytes`.
    #[test]
    fn try_fill_bytes_matches_fill_bytes() {
        let mut a = Xorshift64::new(9);
        let mut b = Xorshift64::new(9);
        let mut buf_a = [0u8; 16];
        let mut buf_b = [0u8; 16];
        a.try_fill_bytes(&mut buf_a)
            .expect("try_fill_bytes should never fail");
        b.fill_bytes(&mut buf_b);
        assert_eq!(buf_a, buf_b);
    }

    proptest! {
        #[test]
        fn nonzero_seed_produces_nonzero_first(seed in 1u64..u64::MAX) {
            let mut rng = Xorshift64::new(seed);
            prop_assert_ne!(rng.next_u64(), 0);
        }
    }
}
