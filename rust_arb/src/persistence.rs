use std::collections::HashMap;

/// Tracks how many consecutive detection cycles an opportunity has been observed.
/// Only opportunities that persist for >= `required_snapshots` consecutive cycles
/// are considered actionable, filtering out spurious single-tick anomalies.
pub struct PersistenceTracker {
    required: u32,
    /// Key: (canonical_id, yes_venue_str) — identifies a unique arb direction
    counts: HashMap<(String, String), u32>,
}

impl PersistenceTracker {
    pub fn new(required_snapshots: u32) -> Self {
        Self {
            required: required_snapshots,
            counts: HashMap::new(),
        }
    }

    /// Call this at the end of each cycle with the set of opportunity keys seen.
    /// Returns the set of keys that have now persisted >= required cycles.
    pub fn update(&mut self, seen: &[(String, String)]) -> Vec<(String, String)> {
        // Decay keys not seen this cycle
        self.counts.retain(|k, _| seen.contains(k));

        // Increment counts for seen opportunities
        for key in seen {
            *self.counts.entry(key.clone()).or_insert(0) += 1;
        }

        // Return keys that have reached the persistence threshold
        self.counts
            .iter()
            .filter(|(_, &count)| count >= self.required)
            .map(|(k, _)| k.clone())
            .collect()
    }
}
