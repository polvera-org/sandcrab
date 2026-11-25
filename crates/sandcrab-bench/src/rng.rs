//! Tiny seeded PRNG (SplitMix64). Deterministic across platforms so fixtures, cell order and
//! bootstrap resamples are reproducible from the recorded `seed`. Not cryptographic.

#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    #[must_use]
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

    /// Uniform-ish integer in `0..n` (`n > 0`; modulo bias is irrelevant here).
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "Rng::below(0)");
        (self.next_u64() % n as u64) as usize
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    /// Fisher–Yates.
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }

    /// Independent child stream (e.g. one per fixture) derived from this one.
    pub fn fork(&mut self) -> Self {
        Self(self.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }

    #[test]
    fn shuffle_is_permutation() {
        let mut v: Vec<u32> = (0..50).collect();
        Rng::new(7).shuffle(&mut v);
        let mut s = v.clone();
        s.sort_unstable();
        assert_eq!(s, (0..50).collect::<Vec<_>>());
        assert_ne!(v, s, "seed 7 should move something");
    }

    #[test]
    fn fill_partial_chunk() {
        let mut buf = [0u8; 13];
        Rng::new(3).fill(&mut buf);
        assert!(buf.iter().any(|b| *b != 0));
    }
}
