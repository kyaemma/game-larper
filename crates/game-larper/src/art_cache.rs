//! A bounded, least-recently-used cache for decoded artwork.
//!
//! The artwork files on disk are the source of truth, so memory only holds what the UI is
//! likely to draw again soon. Two limits apply at once: an estimated decoded size in bytes and
//! a number of entries. Pinned entries (what is on screen, selected, running or queued) are
//! never evicted, so the cache can exceed its limits by exactly the pinned set and no more.

use std::collections::HashMap;

/// Decoded artwork held in memory. Typical entries are 128x128 or 231x87 RGBA, 65-80 KiB each,
/// so this keeps a few hundred of them: several full result lists of revisit headroom.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// A second guard so many tiny images cannot slip past the byte estimate.
pub const MAX_ENTRIES: usize = 256;

/// Estimated decoded size: four bytes per pixel, whatever the pixel format Slint keeps.
pub fn decoded_cost(width: u32, height: u32) -> usize {
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4)
}

struct Entry<V> {
    value: V,
    /// What the image was fetched for. A different identity means the catalog changed the
    /// artwork source, so the picture is stale.
    identity: String,
    cost: usize,
    used: u64,
}

pub struct ArtCache<V> {
    entries: HashMap<String, Entry<V>>,
    bytes: usize,
    clock: u64,
    max_bytes: usize,
    max_entries: usize,
}

impl<V> Default for ArtCache<V> {
    fn default() -> Self {
        Self::new(MAX_BYTES, MAX_ENTRIES)
    }
}

impl<V> ArtCache<V> {
    pub fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            max_bytes,
            max_entries,
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The cached picture, marked as just used.
    pub fn get(&mut self, id: &str) -> Option<&V> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(id).map(|entry| {
            entry.used = clock;
            &entry.value
        })
    }

    /// Store a picture, then evict least-recently-used entries until the limits hold again.
    /// `pinned` entries and the one just inserted are never evicted.
    pub fn insert(
        &mut self,
        id: &str,
        identity: String,
        value: V,
        cost: usize,
        pinned: impl Fn(&str) -> bool,
    ) {
        self.clock += 1;
        if let Some(old) = self.entries.insert(
            id.to_string(),
            Entry {
                value,
                identity,
                cost,
                used: self.clock,
            },
        ) {
            self.bytes -= old.cost;
        }
        self.bytes += cost;
        while self.bytes > self.max_bytes || self.entries.len() > self.max_entries {
            let victim = self
                .entries
                .iter()
                .filter(|(key, _)| key.as_str() != id && !pinned(key))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.bytes -= entry.cost;
            }
        }
    }

    /// Drop every picture whose game is gone or whose artwork source changed.
    /// `current` returns a game's present identity.
    pub fn reconcile(&mut self, current: impl Fn(&str) -> Option<String>) {
        let mut freed = 0;
        self.entries.retain(|id, entry| {
            let keep = current(id).is_some_and(|identity| identity == entry.identity);
            if !keep {
                freed += entry.cost;
            }
            keep
        });
        self.bytes -= freed;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }

    fn fill(cache: &mut ArtCache<u32>, ids: &[&str], cost: usize) {
        for (index, id) in ids.iter().enumerate() {
            cache.insert(id, format!("{id}-v1"), index as u32, cost, none);
        }
    }

    #[test]
    fn decoded_cost_is_four_bytes_per_pixel() {
        assert_eq!(decoded_cost(128, 128), 65_536);
        assert_eq!(decoded_cost(231, 87), 80_388);
        assert_eq!(decoded_cost(u32::MAX, u32::MAX), usize::MAX);
    }

    #[test]
    fn evicts_least_recently_used_when_over_the_byte_budget() {
        let mut cache = ArtCache::new(300, 100);
        fill(&mut cache, &["a", "b", "c"], 100);
        assert_eq!(cache.bytes(), 300);
        // Touching "a" makes "b" the oldest.
        assert!(cache.get("a").is_some());
        cache.insert("d", "d-v1".into(), 9, 100, none);
        assert!(cache.contains("a") && cache.contains("c") && cache.contains("d"));
        assert!(!cache.contains("b"));
        assert_eq!(cache.bytes(), 300);
    }

    #[test]
    fn entry_count_is_a_second_limit() {
        let mut cache = ArtCache::new(usize::MAX, 3);
        fill(&mut cache, &["a", "b", "c", "d", "e"], 1);
        assert_eq!(cache.len(), 3);
        assert!(!cache.contains("a") && !cache.contains("b"));
        assert!(cache.contains("e"));
    }

    #[test]
    fn pinned_entries_survive_pressure_and_the_cache_may_exceed_its_budget() {
        let mut cache = ArtCache::new(200, 100);
        fill(&mut cache, &["sel", "run"], 100);
        let pinned = |id: &str| id == "sel" || id == "run";
        for id in ["x", "y", "z"] {
            cache.insert(id, format!("{id}-v1"), 0, 100, pinned);
        }
        assert!(cache.contains("sel") && cache.contains("run"));
        // Only the newest unpinned entry stays; everything else unpinned went first.
        assert!(cache.contains("z"));
        assert!(!cache.contains("x") && !cache.contains("y"));
        assert_eq!(cache.bytes(), 300);
    }

    #[test]
    fn the_entry_just_inserted_is_never_its_own_victim() {
        let mut cache = ArtCache::new(10, 100);
        cache.insert("big", "v1".into(), 1, 1_000, none);
        assert!(cache.contains("big"));
        cache.insert("next", "v1".into(), 2, 1_000, none);
        assert!(!cache.contains("big") && cache.contains("next"));
        assert_eq!(cache.bytes(), 1_000);
    }

    #[test]
    fn replacing_an_entry_does_not_leak_its_cost() {
        let mut cache = ArtCache::new(1_000, 100);
        cache.insert("a", "v1".into(), 1, 400, none);
        cache.insert("a", "v2".into(), 2, 100, none);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 100);
        assert_eq!(cache.get("a"), Some(&2));
    }

    #[test]
    fn reconcile_drops_changed_sources_and_vanished_games() {
        let mut cache = ArtCache::new(1_000, 100);
        cache.insert("same", "same-v1".into(), 1, 10, none);
        cache.insert("changed", "changed-v1".into(), 2, 20, none);
        cache.insert("gone", "gone-v1".into(), 3, 40, none);
        cache.reconcile(|id| match id {
            "same" => Some("same-v1".to_string()),
            "changed" => Some("changed-v2".to_string()),
            _ => None,
        });
        assert!(cache.contains("same"));
        assert!(!cache.contains("changed") && !cache.contains("gone"));
        assert_eq!(cache.bytes(), 10);
    }

    #[test]
    fn clear_empties_everything() {
        let mut cache = ArtCache::new(1_000, 100);
        fill(&mut cache, &["a", "b"], 10);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
        assert!(!cache.contains("a"));
    }
}
