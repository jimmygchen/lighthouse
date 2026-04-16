use super::*;
use fixed_bytes::FixedBytesExtended;
use std::time::Duration;
use types::{Hash256, MinimalEthSpec};

type E = MinimalEthSpec;

// -----------------------------------------------------------------------
// Construction tests
// -----------------------------------------------------------------------

#[test]
fn new_creates_empty_caches() {
    let bw = BlockWorkflow::<E>::new();

    // Block times cache should be empty.
    assert!(
        bw.block_times_cache.read().cache.is_empty(),
        "block_times_cache should start empty"
    );

    // Envelope times cache should be empty.
    assert!(
        bw.envelope_times_cache.read().cache.is_empty(),
        "envelope_times_cache should start empty"
    );

    // Pre-finalization block cache should contain nothing.
    assert!(
        !bw.pre_finalization_block_cache.contains(Hash256::random()),
        "pre_finalization_block_cache should start empty"
    );
}

// -----------------------------------------------------------------------
// Pruning tests
// -----------------------------------------------------------------------

#[test]
fn prune_caches_removes_old_entries() {
    let bw = BlockWorkflow::<E>::new();

    let old_slot = Slot::new(10);
    let recent_slot = Slot::new(100);
    let current_slot = Slot::new(110);

    let old_root = Hash256::repeat_byte(1);
    let recent_root = Hash256::repeat_byte(2);

    // Insert entries at different slots into the block times cache.
    bw.block_times_cache.write().set_time_observed(
        old_root,
        old_slot,
        Duration::from_secs(10),
        None,
        None,
    );
    bw.block_times_cache.write().set_time_observed(
        recent_root,
        recent_slot,
        Duration::from_secs(100),
        None,
        None,
    );

    assert_eq!(
        bw.block_times_cache.read().cache.len(),
        2,
        "should have 2 entries before pruning"
    );

    // Prune: entries with slot <= current_slot - 64 should be removed.
    // current_slot = 110, cutoff slot = 46. old_slot=10 < 46, so removed.
    // recent_slot = 100 > 46, so kept.
    bw.prune_caches(current_slot);

    let cache = bw.block_times_cache.read();
    assert_eq!(cache.cache.len(), 1, "old entry should be pruned");
    assert!(
        cache.cache.contains_key(&recent_root),
        "recent entry should survive pruning"
    );
    assert!(
        !cache.cache.contains_key(&old_root),
        "old entry should be removed"
    );
}

#[test]
fn default_is_equivalent_to_new() {
    let bw1 = BlockWorkflow::<E>::new();
    let bw2 = BlockWorkflow::<E>::default();

    // Both should produce empty caches.
    assert!(bw1.block_times_cache.read().cache.is_empty());
    assert!(bw2.block_times_cache.read().cache.is_empty());
    assert!(bw1.envelope_times_cache.read().cache.is_empty());
    assert!(bw2.envelope_times_cache.read().cache.is_empty());
}
