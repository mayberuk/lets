/// splitmix64.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn next_below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "next_below needs a positive bound");
        self.next_u64() % bound
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn the_same_seed_replays_the_same_sequence() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(1);
        let left: Vec<u64> = (0..32).map(|_| a.next_u64()).collect();
        let right: Vec<u64> = (0..32).map(|_| b.next_u64()).collect();
        assert_eq!(left, right);
    }

    #[test]
    fn a_different_seed_diverges() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let left: Vec<u64> = (0..32).map(|_| a.next_u64()).collect();
        let right: Vec<u64> = (0..32).map(|_| b.next_u64()).collect();
        assert_ne!(left, right);
    }

    #[test]
    fn next_below_stays_under_its_bound() {
        let mut rng = Rng::new(7);
        for _ in 0..1000 {
            assert!(rng.next_below(8192) < 8192);
        }
    }

    #[test]
    fn a_seeded_sequence_is_not_constant() {
        let mut rng = Rng::new(1);
        let first = rng.next_u64();
        assert!((0..16).any(|_| rng.next_u64() != first));
    }
}
