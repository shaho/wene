//! Byte-budget LRU cache for decoded images. Pure data structure:
//! the caller supplies the cost (estimated bytes) at insert time and
//! the cache evicts least-recently-used entries once the total cost
//! passes the budget. A single entry bigger than the whole budget
//! still stays until something newer pushes it out.

use std::collections::HashMap;
use std::hash::Hash;

pub struct LruCache<K, V> {
    budget: usize,
    total: usize,
    tick: u64,
    entries: HashMap<K, Entry<V>>,
}

struct Entry<V> {
    value: V,
    cost: usize,
    last_used: u64,
}

impl<K: Eq + Hash + Clone, V> LruCache<K, V> {
    pub fn new(budget: usize) -> Self {
        LruCache {
            budget,
            total: 0,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    /// Look up and mark as most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|e| {
            e.last_used = tick;
            &e.value
        })
    }

    /// Peek without touching the use order.
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Insert as most recently used and evict past the budget. The
    /// returned keys were evicted; the key just inserted never is.
    pub fn insert(&mut self, key: K, value: V, cost: usize) -> Vec<K> {
        self.remove(&key);
        self.tick += 1;
        self.total += cost;
        self.entries.insert(
            key.clone(),
            Entry {
                value,
                cost,
                last_used: self.tick,
            },
        );

        let mut evicted = Vec::new();
        while self.total > self.budget && self.entries.len() > 1 {
            // ponytail: O(n) scan per eviction; keep an order queue
            // if profiles ever blame this.
            let oldest = self
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            let Some(oldest) = oldest else { break };
            self.remove(&oldest);
            evicted.push(oldest);
        }
        evicted
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key).map(|e| {
            self.total -= e.cost;
            e.value
        })
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.total = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_past_budget() {
        let mut cache: LruCache<&str, u32> = LruCache::new(100);
        assert!(cache.insert("a", 1, 40).is_empty());
        assert!(cache.insert("b", 2, 40).is_empty());
        // Touch a so b is now the oldest.
        assert_eq!(cache.get(&"a"), Some(&1));
        let evicted = cache.insert("c", 3, 40);
        assert_eq!(evicted, vec!["b"]);
        assert!(cache.contains(&"a"));
        assert!(cache.contains(&"c"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn oversized_entry_stays_until_replaced() {
        let mut cache: LruCache<&str, u32> = LruCache::new(10);
        assert!(cache.insert("big", 1, 50).is_empty());
        assert!(cache.contains(&"big"));
        // The next insert pushes the oversized one out.
        let evicted = cache.insert("next", 2, 50);
        assert_eq!(evicted, vec!["big"]);
        assert!(cache.contains(&"next"));
    }

    #[test]
    fn remove_and_reinsert_keep_totals_right() {
        let mut cache: LruCache<&str, u32> = LruCache::new(100);
        cache.insert("a", 1, 60);
        cache.insert("a", 2, 30); // replace: total is 30, not 90
        assert!(cache.insert("b", 3, 60).is_empty());
        assert_eq!(cache.remove(&"a"), Some(2));
        assert!(cache.insert("c", 4, 40).is_empty());
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }
}
