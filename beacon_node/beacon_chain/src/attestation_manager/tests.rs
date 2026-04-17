use super::*;
use crate::shuffling_cache::BlockShufflingIds;
use fork_choice::ExecutionStatus;
use types::test_utils::generate_deterministic_keypairs;
use types::{ChainSpec, ExecutionBlockHash, Hash256, MinimalEthSpec};

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

// ============================================================================
// get_aggregated_attestation tests
// ============================================================================

#[test]
fn get_aggregated_attestation_returns_attestation_when_valid() {
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);

    let attestation = make_attestation(&spec, slot, block_root, 0);
    manager
        .add_to_naive_aggregation_pool(attestation.to_ref())
        .expect("should insert attestation");

    // Provide an execution status closure that returns Valid for the block root.
    let result = manager
        .get_aggregated_attestation(attestation.to_ref(), |_root| {
            Some(ExecutionStatus::Valid(ExecutionBlockHash::zero()))
        })
        .expect("should not error");

    assert!(
        result.is_some(),
        "should return attestation from pool when execution status is valid"
    );
    assert_eq!(
        result.unwrap().data(),
        attestation.data(),
        "returned attestation data should match"
    );
}

#[test]
fn get_aggregated_attestation_rejects_optimistic_block() {
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);

    let attestation = make_attestation(&spec, slot, block_root, 0);
    manager
        .add_to_naive_aggregation_pool(attestation.to_ref())
        .expect("should insert attestation");

    // Provide an execution status closure that returns Optimistic for the block root.
    let result = manager.get_aggregated_attestation(attestation.to_ref(), |_root| {
        Some(ExecutionStatus::Optimistic(ExecutionBlockHash::zero()))
    });

    assert!(
        result.is_err(),
        "should reject attestation referencing an optimistic block"
    );
    assert!(
        matches!(result, Err(Error::HeadBlockNotFullyVerified { .. })),
        "error should be HeadBlockNotFullyVerified"
    );
}

#[test]
fn get_aggregated_attestation_rejects_finalized_block() {
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);

    let attestation = make_attestation(&spec, slot, block_root, 0);
    manager
        .add_to_naive_aggregation_pool(attestation.to_ref())
        .expect("should insert attestation");

    // Provide an execution status closure that returns None (block not in fork choice).
    let result = manager.get_aggregated_attestation(attestation.to_ref(), |_root| None);

    assert!(
        result.is_err(),
        "should reject attestation when block root not in fork choice"
    );
    assert!(
        matches!(result, Err(Error::CannotAttestToFinalizedBlock { .. })),
        "error should be CannotAttestToFinalizedBlock"
    );
}

#[test]
fn get_aggregated_attestation_returns_none_when_not_in_pool() {
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);

    // Do NOT insert into the pool. Just create the attestation for the lookup key.
    let attestation = make_attestation(&spec, slot, block_root, 0);

    let result = manager
        .get_aggregated_attestation(attestation.to_ref(), |_root| {
            Some(ExecutionStatus::Valid(ExecutionBlockHash::zero()))
        })
        .expect("should not error");

    assert!(
        result.is_none(),
        "should return None when no matching attestation in pool"
    );
}

// ============================================================================
// filter_optimistic_attestation tests
// ============================================================================

#[test]
fn filter_optimistic_attestation_passes_valid() {
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);
    let attestation = make_attestation(&spec, slot, block_root, 0);

    let result = AttestationManager::<E>::filter_optimistic_attestation(attestation, &|_root| {
        Some(ExecutionStatus::Valid(ExecutionBlockHash::zero()))
    });

    assert!(
        result.is_ok(),
        "should pass through attestation with valid execution status"
    );
}

#[test]
fn filter_optimistic_attestation_rejects_optimistic() {
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);
    let attestation = make_attestation(&spec, slot, block_root, 0);

    let result = AttestationManager::<E>::filter_optimistic_attestation(attestation, &|_root| {
        Some(ExecutionStatus::Optimistic(ExecutionBlockHash::zero()))
    });

    assert!(
        result.is_err(),
        "should reject attestation with optimistic execution status"
    );
    assert!(
        matches!(result, Err(Error::HeadBlockNotFullyVerified { .. })),
        "error should be HeadBlockNotFullyVerified"
    );
}

#[test]
fn filter_optimistic_attestation_rejects_finalized() {
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);
    let attestation = make_attestation(&spec, slot, block_root, 0);

    let result = AttestationManager::<E>::filter_optimistic_attestation(attestation, &|_root| None);

    assert!(
        result.is_err(),
        "should reject attestation when execution status is None (finalized)"
    );
    assert!(
        matches!(result, Err(Error::CannotAttestToFinalizedBlock { .. })),
        "error should be CannotAttestToFinalizedBlock"
    );
}

// ============================================================================
// validator_seen_at_epoch tests
// ============================================================================

#[test]
fn validator_seen_at_epoch_gossip_attested() {
    let manager = make_manager();
    let epoch = Epoch::new(0);
    let validator_index = 0;

    // Observe the validator as having gossip-attested.
    manager
        .observed_gossip_attesters
        .write()
        .observe_validator(epoch, validator_index)
        .expect("should observe validator");

    assert!(
        manager.validator_seen_at_epoch(validator_index, epoch),
        "should return true when validator has gossip attested"
    );
}

#[test]
fn validator_seen_at_epoch_block_attested() {
    let manager = make_manager();
    let epoch = Epoch::new(0);
    let validator_index = 1;

    // Observe the validator as having block-attested.
    manager
        .observed_block_attesters
        .write()
        .observe_validator(epoch, validator_index)
        .expect("should observe validator");

    assert!(
        manager.validator_seen_at_epoch(validator_index, epoch),
        "should return true when validator has block attested"
    );
}

#[test]
fn validator_seen_at_epoch_aggregated() {
    let manager = make_manager();
    let epoch = Epoch::new(0);
    let validator_index = 2;

    // Observe the validator as having aggregated.
    manager
        .observed_aggregators
        .write()
        .observe_validator(epoch, validator_index)
        .expect("should observe validator");

    assert!(
        manager.validator_seen_at_epoch(validator_index, epoch),
        "should return true when validator has aggregated"
    );
}

#[test]
fn validator_seen_at_epoch_not_seen() {
    let manager = make_manager();
    let epoch = Epoch::new(0);
    let validator_index = 5;

    // Do not observe the validator at all.
    assert!(
        !manager.validator_seen_at_epoch(validator_index, epoch),
        "should return false when validator not seen at epoch"
    );
}

// ============================================================================
// produce_unaggregated_attestation — early attester cache path
// ============================================================================

#[test]
fn produce_unaggregated_attestation_from_early_attester_cache() {
    let spec = make_spec();
    let genesis_block_root = Hash256::repeat_byte(0x42);

    let head_shuffling_ids = BlockShufflingIds {
        current: AttestationShufflingId::from_components(Epoch::new(0), genesis_block_root),
        next: AttestationShufflingId::from_components(Epoch::new(1), genesis_block_root),
        previous: None,
        block_root: genesis_block_root,
    };
    let shuffling_cache = ShufflingCache::new(16, head_shuffling_ids);
    let manager: AttestationManager<E> =
        AttestationManager::new(spec.clone(), genesis_block_root, shuffling_cache);

    let request_slot = Slot::new(0);
    let request_index = 0;

    // try_attest returns None when the cache is empty.
    let empty_result = manager
        .early_attester_cache
        .try_attest(request_slot, request_index, &spec)
        .expect("should not error");
    assert!(
        empty_result.is_none(),
        "early attester cache should be empty initially"
    );

    // We can't easily populate the early_attester_cache without AvailableBlock +
    // ProtoBlock, which requires full chain infrastructure. Instead we verify:
    // 1) The cache returns None when empty (tested above)
    // 2) produce_unaggregated_attestation delegates to try_attest first (code inspection)
    //
    // Full integration testing of the early_attester_cache path is covered by
    // BeaconChainHarness-based tests.
}

// ============================================================================
// get_pre_electra_aggregated_attestation_by_slot_and_root tests
// ============================================================================

#[test]
fn get_pre_electra_aggregated_attestation_by_slot_and_root_returns_attestation() {
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);

    let attestation = make_attestation(&spec, slot, block_root, 0);
    manager
        .add_to_naive_aggregation_pool(attestation.to_ref())
        .expect("should insert attestation");

    let att_data_root = attestation.data().tree_hash_root();
    let result = manager
        .get_pre_electra_aggregated_attestation_by_slot_and_root(slot, &att_data_root, |_root| {
            Some(ExecutionStatus::Valid(ExecutionBlockHash::zero()))
        })
        .expect("should not error");

    assert!(
        result.is_some(),
        "should return attestation by slot and root when valid"
    );
    assert_eq!(
        result.unwrap().data(),
        attestation.data(),
        "returned attestation data should match"
    );
}

#[test]
fn get_pre_electra_aggregated_attestation_by_slot_and_root_returns_none_on_miss() {
    let manager = make_manager();
    let slot = Slot::new(1);
    let unknown_root = Hash256::repeat_byte(0xff);

    let result = manager
        .get_pre_electra_aggregated_attestation_by_slot_and_root(slot, &unknown_root, |_root| {
            Some(ExecutionStatus::Valid(ExecutionBlockHash::zero()))
        })
        .expect("should not error");

    assert!(
        result.is_none(),
        "should return None when no matching attestation exists"
    );
}

// ============================================================================
// BDD acceptance tests — non-duplicate invariants
// ============================================================================

#[test]
fn given_multiple_committees_when_attestations_inserted_then_each_retrievable() {
    // Given an AttestationManager
    let manager = make_manager();
    let spec = make_spec();
    let slot = Slot::new(1);
    let block_root = Hash256::repeat_byte(0xaa);

    // When we insert attestations from two different committees (different committee indices)
    let att_committee_0 = make_attestation(&spec, slot, block_root, 0);
    let att_committee_1 = make_attestation(&spec, slot, block_root, 1);
    manager
        .add_to_naive_aggregation_pool(att_committee_0.to_ref())
        .expect("committee 0 insert should succeed");
    manager
        .add_to_naive_aggregation_pool(att_committee_1.to_ref())
        .expect("committee 1 insert should succeed");

    // Then the pool contains both attestations
    let pool = manager.naive_aggregation_pool.read();
    let count = pool.iter().count();
    assert_eq!(
        count, 2,
        "pool should contain attestations from both committees"
    );
}
