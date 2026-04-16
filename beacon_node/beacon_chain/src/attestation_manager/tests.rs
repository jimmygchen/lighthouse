use super::*;
use crate::shuffling_cache::BlockShufflingIds;
use types::test_utils::generate_deterministic_keypairs;
use types::{ChainSpec, Hash256, MinimalEthSpec};

type E = MinimalEthSpec;

fn make_spec() -> Arc<ChainSpec> {
    Arc::new(MinimalEthSpec::default_spec())
}

fn make_manager() -> AttestationManager<E> {
    let spec = make_spec();
    let genesis_block_root = Hash256::repeat_byte(0x42);

    let head_shuffling_ids = BlockShufflingIds {
        current: AttestationShufflingId::from_components(Epoch::new(0), genesis_block_root),
        next: AttestationShufflingId::from_components(Epoch::new(1), genesis_block_root),
        previous: None,
        block_root: genesis_block_root,
    };

    let shuffling_cache = ShufflingCache::new(16, head_shuffling_ids);

    AttestationManager::new(spec, genesis_block_root, shuffling_cache)
}

/// Create a pre-electra attestation with one aggregation bit set (required by the pool).
fn make_attestation(
    spec: &ChainSpec,
    slot: Slot,
    beacon_block_root: Hash256,
    committee_index: u64,
) -> Attestation<E> {
    let source = Checkpoint {
        epoch: slot.epoch(E::slots_per_epoch()),
        root: Hash256::repeat_byte(0xbb),
    };
    let target = Checkpoint {
        epoch: slot.epoch(E::slots_per_epoch()),
        root: Hash256::repeat_byte(0xcc),
    };
    let mut attestation = Attestation::<E>::empty_for_signing(
        committee_index,
        4,
        slot,
        beacon_block_root,
        source,
        target,
        spec,
    )
    .expect("should create attestation");

    // Set one aggregation bit so the pool accepts it.
    match &mut attestation {
        Attestation::Base(att) => att.aggregation_bits.set(0, true).expect("should set bit"),
        Attestation::Electra(att) => att.aggregation_bits.set(0, true).expect("should set bit"),
    }

    attestation
}

#[test]
fn add_to_naive_aggregation_pool_success() {
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);

    let attestation = make_attestation(&spec, slot, Hash256::repeat_byte(0xaa), 0);

    let result = manager.add_to_naive_aggregation_pool(attestation.to_ref());
    assert!(result.is_ok(), "should insert attestation into pool");

    // Verify the attestation is in the pool.
    let pool = manager.naive_aggregation_pool.read();
    let count = pool.iter().count();
    assert_eq!(count, 1, "pool should contain one attestation");
}

#[test]
fn add_to_naive_aggregation_pool_slot_too_low() {
    let manager = make_manager();
    let spec = make_spec();

    // Insert an attestation at a high slot to advance the pool's lowest permissible slot.
    let high_slot = Slot::new(10);
    let attestation_high = make_attestation(&spec, high_slot, Hash256::repeat_byte(0xaa), 0);
    manager
        .add_to_naive_aggregation_pool(attestation_high.to_ref())
        .expect("should insert high-slot attestation");

    // Now try inserting a very old attestation.
    // The pool has seen slot 10 so anything below (10 - SLOTS_RETAINED + 1) = 8 is too low.
    let old_slot = Slot::new(0);
    let attestation_old = make_attestation(&spec, old_slot, Hash256::repeat_byte(0xdd), 0);

    // The manager catches SlotTooLow and returns Ok(()).
    let result = manager.add_to_naive_aggregation_pool(attestation_old.to_ref());
    assert!(
        result.is_ok(),
        "old attestation should be silently dropped, not error"
    );
}

#[test]
fn prune_naive_aggregation_pool_removes_old_entries() {
    let manager = make_manager();
    let spec = make_spec();

    // Insert attestations at different slots.
    for i in 0..5u64 {
        let slot = Slot::new(i);
        let attestation = make_attestation(&spec, slot, Hash256::repeat_byte(i as u8), 0);
        let _ = manager.add_to_naive_aggregation_pool(attestation.to_ref());
    }

    // Before pruning, pool should have some entries.
    let count_before = manager.naive_aggregation_pool.read().iter().count();
    assert!(count_before > 0, "pool should have entries before pruning");

    // Prune at a high slot to remove old entries.
    manager.prune_naive_aggregation_pool(Slot::new(100));

    // After pruning, old entries should be removed.
    let count_after = manager.naive_aggregation_pool.read().iter().count();
    assert_eq!(
        count_after, 0,
        "pool should be empty after aggressive prune"
    );
}

#[test]
fn shuffling_is_compatible_matching_shuffling() {
    let spec = make_spec();
    let genesis_block_root = Hash256::repeat_byte(0x42);

    let head_shuffling_ids = BlockShufflingIds {
        current: AttestationShufflingId::from_components(Epoch::new(0), genesis_block_root),
        next: AttestationShufflingId::from_components(Epoch::new(1), genesis_block_root),
        previous: None,
        block_root: genesis_block_root,
    };
    let shuffling_cache = ShufflingCache::new(16, head_shuffling_ids);
    let manager = AttestationManager::new(spec.clone(), genesis_block_root, shuffling_cache);

    // Build a minimal state at epoch 0 with committee caches.
    let keypairs = generate_deterministic_keypairs(8);
    let mut state = BeaconState::<E>::new(0, Default::default(), &spec);
    for keypair in &keypairs {
        state
            .validators_mut()
            .push(Validator {
                pubkey: keypair.pk.clone().into(),
                activation_epoch: Epoch::new(0),
                exit_epoch: Epoch::max_value(),
                effective_balance: spec.max_effective_balance,
                ..Default::default()
            })
            .expect("should push validator");
        state
            .balances_mut()
            .push(spec.max_effective_balance)
            .expect("should push balance");
    }
    state
        .build_all_committee_caches(&spec)
        .expect("should build committee caches");

    let target_epoch = Epoch::new(0);

    // The block's shuffling ID should match the state's shuffling ID at epoch 0.
    let block_shuffling_id =
        AttestationShufflingId::new(genesis_block_root, &state, RelativeEpoch::Current)
            .expect("should get shuffling id");

    let block_root = genesis_block_root;
    let result =
        manager.shuffling_is_compatible(&block_root, target_epoch, &state, block_shuffling_id);
    assert!(result, "shuffling should be compatible");
}

#[test]
fn shuffling_is_compatible_mismatched_shuffling() {
    let spec = make_spec();
    let genesis_block_root = Hash256::repeat_byte(0x42);

    let head_shuffling_ids = BlockShufflingIds {
        current: AttestationShufflingId::from_components(Epoch::new(0), genesis_block_root),
        next: AttestationShufflingId::from_components(Epoch::new(1), genesis_block_root),
        previous: None,
        block_root: genesis_block_root,
    };
    let shuffling_cache = ShufflingCache::new(16, head_shuffling_ids);
    let manager = AttestationManager::new(spec.clone(), genesis_block_root, shuffling_cache);

    // Build a minimal state at epoch 0.
    let keypairs = generate_deterministic_keypairs(8);
    let mut state = BeaconState::<E>::new(0, Default::default(), &spec);
    for keypair in &keypairs {
        state
            .validators_mut()
            .push(Validator {
                pubkey: keypair.pk.clone().into(),
                activation_epoch: Epoch::new(0),
                exit_epoch: Epoch::max_value(),
                effective_balance: spec.max_effective_balance,
                ..Default::default()
            })
            .expect("should push validator");
        state
            .balances_mut()
            .push(spec.max_effective_balance)
            .expect("should push balance");
    }
    state
        .build_all_committee_caches(&spec)
        .expect("should build committee caches");

    let target_epoch = Epoch::new(0);

    // Use a different decision block root to create a mismatched shuffling ID.
    let different_block_root = Hash256::repeat_byte(0xff);
    let block_shuffling_id =
        AttestationShufflingId::from_components(target_epoch, different_block_root);

    let block_root = Hash256::repeat_byte(0xaa);
    let result =
        manager.shuffling_is_compatible(&block_root, target_epoch, &state, block_shuffling_id);
    assert!(!result, "shuffling should be incompatible");
}

#[test]
fn import_block_update_shuffling_cache_populates_cache() {
    let spec = make_spec();
    let genesis_block_root = Hash256::repeat_byte(0x42);

    let head_shuffling_ids = BlockShufflingIds {
        current: AttestationShufflingId::from_components(Epoch::new(0), genesis_block_root),
        next: AttestationShufflingId::from_components(Epoch::new(1), genesis_block_root),
        previous: None,
        block_root: genesis_block_root,
    };
    let shuffling_cache = ShufflingCache::new(16, head_shuffling_ids);
    let manager = AttestationManager::new(spec.clone(), genesis_block_root, shuffling_cache);

    // Build a minimal state at epoch 0.
    let keypairs = generate_deterministic_keypairs(8);
    let mut state = BeaconState::<E>::new(0, Default::default(), &spec);
    for keypair in &keypairs {
        state
            .validators_mut()
            .push(Validator {
                pubkey: keypair.pk.clone().into(),
                activation_epoch: Epoch::new(0),
                exit_epoch: Epoch::max_value(),
                effective_balance: spec.max_effective_balance,
                ..Default::default()
            })
            .expect("should push validator");
        state
            .balances_mut()
            .push(spec.max_effective_balance)
            .expect("should push balance");
    }
    state
        .build_all_committee_caches(&spec)
        .expect("should build committee caches");

    let block_root = Hash256::repeat_byte(0xab);

    // Before calling import, the cache should not contain the shuffling for block_root.
    let current_id = AttestationShufflingId::new(block_root, &state, RelativeEpoch::Current)
        .expect("should compute current shuffling id");
    let next_id = AttestationShufflingId::new(block_root, &state, RelativeEpoch::Next)
        .expect("should compute next shuffling id");

    assert!(
        !manager.shuffling_cache.read().contains(&current_id),
        "cache should not contain current shuffling before import"
    );
    assert!(
        !manager.shuffling_cache.read().contains(&next_id),
        "cache should not contain next shuffling before import"
    );

    // Run the cache update.
    manager.import_block_update_shuffling_cache(block_root, &mut state);

    // After calling import, the cache should contain both shufflings.
    assert!(
        manager.shuffling_cache.read().contains(&current_id),
        "cache should contain current shuffling after import"
    );
    assert!(
        manager.shuffling_cache.read().contains(&next_id),
        "cache should contain next shuffling after import"
    );
}

#[test]
fn multiple_attestations_same_slot_different_data() {
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);

    // Insert two attestations with different beacon_block_roots.
    let att1 = make_attestation(&spec, slot, Hash256::repeat_byte(0xaa), 0);
    let att2 = make_attestation(&spec, slot, Hash256::repeat_byte(0xbb), 0);

    manager
        .add_to_naive_aggregation_pool(att1.to_ref())
        .expect("should insert att1");
    manager
        .add_to_naive_aggregation_pool(att2.to_ref())
        .expect("should insert att2");

    let pool = manager.naive_aggregation_pool.read();
    let count = pool.iter().count();
    assert_eq!(
        count, 2,
        "pool should contain two attestations with different data"
    );
}
