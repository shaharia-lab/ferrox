use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rand::Rng;

/// Lock-free load balancing strategy.
///
/// `select` is called with a slice of booleans indicating which target indices
/// are currently available (circuit breaker closed/half-open and claiming probe slot).
/// Returns `None` if no target is available.
pub enum LbStrategy {
    RoundRobin {
        counter: Arc<AtomicUsize>,
    },
    /// Pre-expanded weight slot array.
    ///
    /// Built once at config load time: weights [70, 30] become a 10-slot array
    /// `[0,0,0,0,0,0,0,1,1,1]` (indices into the targets vec). Counter wraps
    /// modulo `slots.len()` — zero runtime division after construction.
    Weighted {
        slots: Vec<usize>,
        counter: Arc<AtomicUsize>,
    },
    Failover,
    Random,
}

impl LbStrategy {
    pub fn round_robin() -> Self {
        LbStrategy::RoundRobin {
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Build the weighted strategy from a `weights` slice.
    ///
    /// GCD-reduces the weights first, caps the slot array at 100 entries.
    pub fn weighted(weights: &[u32]) -> Self {
        let slots = expand_weights(weights);
        LbStrategy::Weighted {
            slots,
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn failover() -> Self {
        LbStrategy::Failover
    }

    pub fn random() -> Self {
        LbStrategy::Random
    }

    /// Select the index of the next target to try.
    ///
    /// `available[i]` must be `true` for target `i` to be eligible.
    /// Returns `None` if all targets are unavailable.
    pub fn select(&self, available: &[bool]) -> Option<usize> {
        if available.iter().all(|a| !a) {
            return None;
        }

        match self {
            LbStrategy::RoundRobin { counter } => {
                let n = available.len();
                // Try each position once, starting from the current counter
                let start = counter.fetch_add(1, Ordering::Relaxed);
                for i in 0..n {
                    let idx = (start + i) % n;
                    if available[idx] {
                        return Some(idx);
                    }
                }
                None
            }
            LbStrategy::Weighted { slots, counter } => {
                let n = slots.len();
                let start = counter.fetch_add(1, Ordering::Relaxed);
                // Walk the slot array until we find an available target
                for i in 0..n {
                    let slot_idx = (start + i) % n;
                    let target_idx = slots[slot_idx];
                    if target_idx < available.len() && available[target_idx] {
                        return Some(target_idx);
                    }
                }
                // Fallback: pick any available
                available.iter().position(|a| *a)
            }
            LbStrategy::Failover => {
                // Always pick the first available in order
                available.iter().position(|a| *a)
            }
            LbStrategy::Random => {
                let eligible: Vec<usize> = available
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| **a)
                    .map(|(i, _)| i)
                    .collect();
                if eligible.is_empty() {
                    return None;
                }
                let idx = rand::thread_rng().gen_range(0..eligible.len());
                Some(eligible[idx])
            }
        }
    }
}

/// Expand weights into a slot array (GCD-reduced, capped at 100 slots).
///
/// Example: weights [70, 30] → reduced [7, 3] → 10 slots → [0,0,0,0,0,0,0,1,1,1]
fn expand_weights(weights: &[u32]) -> Vec<usize> {
    if weights.is_empty() {
        return vec![];
    }

    // GCD reduce to keep slot array small
    let g = weights.iter().copied().fold(0u32, gcd);
    let reduced: Vec<u32> = weights.iter().map(|w| w / g).collect();
    let total: u32 = reduced.iter().sum();

    // Cap the slot array at 100 entries
    let slots_count = (total as usize).min(100);

    let heaviest = reduced
        .iter()
        .enumerate()
        .max_by_key(|(_, w)| *w)
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut slots = Vec::with_capacity(slots_count);
    for (i, &w) in reduced.iter().enumerate() {
        // Scale proportionally to the target slot count
        let count = (w as f64 / total as f64 * slots_count as f64).round() as usize;
        for _ in 0..count {
            slots.push(i);
        }
    }

    // Trim/pad to exactly slots_count to handle rounding drift
    slots.truncate(slots_count);
    while slots.len() < slots_count {
        slots.push(heaviest);
    }

    slots
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_cycles() {
        let strategy = LbStrategy::round_robin();
        let available = [true, true, true];
        let picks: Vec<_> = (0..6)
            .map(|_| strategy.select(&available).unwrap())
            .collect();
        // Should cycle 0,1,2,0,1,2
        assert_eq!(picks, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn round_robin_skips_unavailable() {
        let strategy = LbStrategy::round_robin();
        let available = [true, false, true];
        for _ in 0..10 {
            let pick = strategy.select(&available).unwrap();
            assert_ne!(pick, 1);
        }
    }

    #[test]
    fn failover_picks_first() {
        let strategy = LbStrategy::failover();
        assert_eq!(strategy.select(&[true, true, true]), Some(0));
        assert_eq!(strategy.select(&[false, true, true]), Some(1));
        assert_eq!(strategy.select(&[false, false, true]), Some(2));
        assert_eq!(strategy.select(&[false, false, false]), None);
    }

    #[test]
    fn weighted_distribution() {
        let strategy = LbStrategy::weighted(&[70, 30]);
        let available = [true, true];
        let mut counts = [0usize; 2];
        for _ in 0..1000 {
            let pick = strategy.select(&available).unwrap();
            counts[pick] += 1;
        }
        // Should be approximately 70/30 split — allow ±10%
        assert!(
            counts[0] > 550 && counts[0] < 850,
            "counts[0]={}",
            counts[0]
        );
        assert!(
            counts[1] > 150 && counts[1] < 450,
            "counts[1]={}",
            counts[1]
        );
    }

    #[test]
    fn all_unavailable_returns_none() {
        let strategy = LbStrategy::round_robin();
        assert_eq!(strategy.select(&[false, false]), None);
    }
}
