use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

/// Turns cumulative counters into per-interval differences. Each collector owns
/// one, because modules run at different intervals.
pub struct Deltas<K> {
    prev: HashMap<K, Vec<u64>>,
    t: Option<Instant>,
}

impl<K: Eq + Hash + Clone> Default for Deltas<K> {
    fn default() -> Self {
        Self {
            prev: HashMap::new(),
            t: None,
        }
    }
}

impl<K: Eq + Hash + Clone> Deltas<K> {
    /// Returns the elapsed seconds and, for each key also present last time, the
    /// difference of each counter. `None` on the first call.
    pub fn update(&mut self, counters: HashMap<K, Vec<u64>>) -> Option<(f64, HashMap<K, Vec<u64>>)> {
        self.update_at(counters, Instant::now())
    }

    fn update_at(&mut self, counters: HashMap<K, Vec<u64>>, now: Instant) -> Option<(f64, HashMap<K, Vec<u64>>)> {
        let dt = self
            .t
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|dt| *dt > 0.0);
        let mut out = HashMap::new();
        if dt.is_some() {
            for (k, vals) in &counters {
                if let Some(old) = self.prev.get(k).filter(|old| old.len() == vals.len()) {
                    // a counter that went backwards was reset (device or process replaced)
                    out.insert(
                        k.clone(),
                        vals.iter().zip(old).map(|(v, o)| v.saturating_sub(*o)).collect(),
                    );
                }
            }
        }
        self.prev = counters;
        self.t = Some(now);
        dt.map(|dt| (dt, out))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn c(pairs: &[(&'static str, &[u64])]) -> HashMap<&'static str, Vec<u64>> {
        pairs.iter().map(|(k, v)| (*k, v.to_vec())).collect()
    }

    #[test]
    fn first_call_has_no_rate() {
        // a rate from a single reading would be the counter's lifetime total, not a rate
        let mut d = Deltas::default();
        assert!(d.update(c(&[("eth0", &[100])])).is_none());
    }

    #[test]
    fn differences_over_elapsed_time() {
        let mut d = Deltas::default();
        let t0 = Instant::now();
        d.update_at(c(&[("eth0", &[100, 5])]), t0);
        let (dt, out) = d
            .update_at(c(&[("eth0", &[300, 6])]), t0 + Duration::from_secs(2))
            .unwrap();
        assert_eq!(dt, 2.0);
        assert_eq!(out["eth0"], [200, 1]);
    }

    #[test]
    fn reset_counter_reads_zero_not_a_huge_spike() {
        let mut d = Deltas::default();
        let t0 = Instant::now();
        d.update_at(c(&[("eth0", &[1_000_000])]), t0);
        let (_, out) = d.update_at(c(&[("eth0", &[10])]), t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(out["eth0"], [0]);
    }

    #[test]
    fn new_and_vanished_keys_are_left_out() {
        let mut d = Deltas::default();
        let t0 = Instant::now();
        d.update_at(c(&[("old", &[1])]), t0);
        let (_, out) = d.update_at(c(&[("new", &[5])]), t0 + Duration::from_secs(2)).unwrap();
        assert!(out.is_empty(), "a key seen once has no previous value to differ from");
    }
}
