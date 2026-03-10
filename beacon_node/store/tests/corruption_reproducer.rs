//! Integration tests exercising beacon state rebase and committee cache integrity.
//!
//! These tests aim to reproduce or detect the kind of state corruption observed
//! on mainnet at slot 13847332 (v8.1.2). They are marked `#[ignore]` so they
//! only run when explicitly selected.

use genesis::{DEFAULT_ETH1_BLOCK_HASH, generate_deterministic_keypairs, interop_genesis_state};
use std::collections::HashSet;
use types::{BeaconState, ChainSpec, EthSpec, Hash256, MinimalEthSpec, RelativeEpoch};

type E = MinimalEthSpec;

/// Helper: create a genesis state with `validator_count` validators and all caches built.
fn genesis_state_with_caches(validator_count: usize) -> (BeaconState<E>, ChainSpec) {
    let spec = E::default_spec();
    let keypairs = generate_deterministic_keypairs(validator_count);
    let mut state = interop_genesis_state::<E>(
        &keypairs,
        0,
        Hash256::from_slice(DEFAULT_ETH1_BLOCK_HASH),
        None,
        &spec,
    )
    .expect("should create interop genesis state");
    state.build_caches(&spec).expect("should build caches");
    (state, spec)
}

/// Test 3: Verify that `rebase_on` preserves state integrity.
///
/// Creates two states from the same genesis, rebases one onto the other, and
/// checks that the tree hash root, validator data, and committee caches are
/// all preserved.
#[test]
#[ignore]
fn rebase_on_preserves_state_integrity() {
    let validator_count = 256;
    let (mut state, spec) = genesis_state_with_caches(validator_count);

    // Compute the canonical root before rebasing.
    let root_before = state.canonical_root().expect("should compute root before");

    // Snapshot validator pubkeys and balances before rebase.
    let pubkeys_before: Vec<_> = state
        .validators()
        .iter()
        .map(|v| v.pubkey.clone())
        .collect();
    let balances_before: Vec<u64> = state.balances().to_vec();

    // Clone the state to serve as the "finalized" base for rebasing.
    let base = state.clone();

    // Perform the rebase (this is the code path used in `put_state`).
    state
        .rebase_on(&base, &spec)
        .expect("rebase_on should succeed");

    // 1. Tree hash root must be unchanged after rebase.
    let root_after = state.canonical_root().expect("should compute root after");
    assert_eq!(
        root_before, root_after,
        "tree hash root must be identical after rebase_on"
    );

    // 2. Validator count must be unchanged.
    assert_eq!(
        state.validators().len(),
        validator_count,
        "validator count must be preserved after rebase"
    );

    // 3. Every validator pubkey must match.
    for (i, validator) in state.validators().iter().enumerate() {
        assert_eq!(
            validator.pubkey,
            *pubkeys_before
                .get(i)
                .expect("pubkey index should be in bounds"),
            "validator pubkey mismatch at index {i}"
        );
    }

    // 4. All balances must match.
    for (i, balance) in state.balances().iter().enumerate() {
        assert_eq!(
            *balance,
            *balances_before
                .get(i)
                .expect("balance index should be in bounds"),
            "balance mismatch at index {i}"
        );
    }

    // 5. Committee caches must still be initialized and yield consistent results.
    for rel_epoch in [
        RelativeEpoch::Previous,
        RelativeEpoch::Current,
        RelativeEpoch::Next,
    ] {
        assert!(
            state.committee_cache_is_initialized(rel_epoch),
            "committee cache for {rel_epoch:?} should be initialized after rebase"
        );
    }

    // 6. Verify committees are non-empty and indices are in bounds.
    let current_epoch = state.current_epoch();
    for slot in current_epoch.slot_iter(E::slots_per_epoch()) {
        let committees = state
            .get_beacon_committees_at_slot(slot)
            .expect("should get committees at slot");

        assert!(
            !committees.is_empty(),
            "should have at least one committee at slot {slot}"
        );

        for committee in &committees {
            assert!(
                !committee.committee.is_empty(),
                "committee at slot {slot} index {} should not be empty",
                committee.index
            );
            for &idx in committee.committee {
                assert!(
                    idx < validator_count,
                    "validator index {idx} out of bounds (max {validator_count})"
                );
            }
        }
    }

    // 7. Rebuild caches from scratch and verify the root is still the same.
    state.drop_all_caches().expect("should drop caches");
    state.build_caches(&spec).expect("should rebuild caches");
    let root_rebuilt = state
        .canonical_root()
        .expect("should compute root after rebuild");
    assert_eq!(
        root_before, root_rebuilt,
        "tree hash root must match after full cache rebuild"
    );
}

/// Test 4: Committee cache consistency at scale.
///
/// Builds committee caches for a state with many validators, verifies all
/// constraints (sizes, no duplicates, in-bounds indices), then clones the
/// state, force-rebuilds caches, and checks that results are identical.
#[test]
#[ignore]
fn committee_cache_consistency_at_scale() {
    let validator_count = 256;
    let (mut state, spec) = genesis_state_with_caches(validator_count);

    // Build all committee caches explicitly.
    state
        .build_all_committee_caches(&spec)
        .expect("should build all committee caches");

    // Collect all validator indices assigned across the current epoch.
    let current_epoch = state.current_epoch();
    let mut all_assigned_indices: Vec<usize> = Vec::new();

    for slot in current_epoch.slot_iter(E::slots_per_epoch()) {
        let committees = state
            .get_beacon_committees_at_slot(slot)
            .expect("should get committees at slot");

        // Every slot must have at least one committee.
        assert!(
            !committees.is_empty(),
            "slot {slot} must have at least one committee"
        );

        for committee in &committees {
            // Committees must not be empty.
            assert!(
                !committee.committee.is_empty(),
                "committee at slot {slot} index {} must not be empty",
                committee.index
            );

            // All indices must be in bounds.
            for &idx in committee.committee {
                assert!(
                    idx < validator_count,
                    "validator index {idx} out of bounds at slot {slot} committee {}",
                    committee.index
                );
            }

            // No duplicate indices within a single committee.
            let unique: HashSet<usize> = committee.committee.iter().copied().collect();
            assert_eq!(
                unique.len(),
                committee.committee.len(),
                "duplicate indices in committee at slot {slot} index {}",
                committee.index
            );

            all_assigned_indices.extend_from_slice(committee.committee);
        }
    }

    // Each active validator should appear exactly once across the whole epoch.
    let unique_total: HashSet<usize> = all_assigned_indices.iter().copied().collect();
    assert_eq!(
        unique_total.len(),
        all_assigned_indices.len(),
        "each validator should appear exactly once across all committees in the epoch"
    );

    // Verify the active validator count matches what the cache reports.
    let cache = state
        .committee_cache(RelativeEpoch::Current)
        .expect("should get current committee cache");
    assert_eq!(
        cache.active_validator_count(),
        unique_total.len(),
        "active validator count from cache should match assigned count"
    );

    // Clone the state and force-rebuild committee caches from scratch.
    let mut rebuilt_state = state.clone();
    for rel_epoch in [
        RelativeEpoch::Previous,
        RelativeEpoch::Current,
        RelativeEpoch::Next,
    ] {
        rebuilt_state
            .force_build_committee_cache(rel_epoch, &spec)
            .expect("should force rebuild committee cache");
    }

    // Verify that the rebuilt caches produce identical committees.
    for rel_epoch in [
        RelativeEpoch::Previous,
        RelativeEpoch::Current,
        RelativeEpoch::Next,
    ] {
        let epoch = rel_epoch.into_epoch(state.current_epoch());
        for slot in epoch.slot_iter(E::slots_per_epoch()) {
            let original_committees = state
                .get_beacon_committees_at_slot(slot)
                .expect("should get original committees");
            let rebuilt_committees = rebuilt_state
                .get_beacon_committees_at_slot(slot)
                .expect("should get rebuilt committees");

            assert_eq!(
                original_committees.len(),
                rebuilt_committees.len(),
                "committee count mismatch at slot {slot}"
            );

            for (orig, rebuilt) in original_committees.iter().zip(rebuilt_committees.iter()) {
                assert_eq!(orig.slot, rebuilt.slot, "committee slot mismatch");
                assert_eq!(orig.index, rebuilt.index, "committee index mismatch");
                assert_eq!(
                    orig.committee, rebuilt.committee,
                    "committee members mismatch at slot {slot} index {}",
                    orig.index
                );
            }
        }
    }
}
