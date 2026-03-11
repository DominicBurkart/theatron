use rand_core::{Error, RngCore, impls};

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

    proptest! {
        #[test]
        fn nonzero_seed_produces_nonzero_first(seed in 1u64..u64::MAX) {
            let mut rng = Xorshift64::new(seed);
            prop_assert_ne!(rng.next_u64(), 0);
        }
    }
}
