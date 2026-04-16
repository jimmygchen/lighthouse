use super::*;
use crate::sync_committee_verification::VerifiedSyncCommitteeMessage;
use fixed_bytes::FixedBytesExtended;
use genesis::{DEFAULT_ETH1_BLOCK_HASH, interop_genesis_state};
use types::*;

type E = MinimalEthSpec;

const VALIDATOR_COUNT: usize = 16;

/// Create a minimal spec suitable for testing at genesis.
fn test_spec() -> ChainSpec {
    E::default_spec()
}

/// Create a spec with Altair enabled at epoch 0.
fn altair_spec() -> ChainSpec {
    let mut spec = test_spec();
    spec.altair_fork_epoch = Some(Epoch::new(0));
    spec
}

/// Create a `SyncCommitteeManager` backed by the default spec.
fn new_manager() -> SyncCommitteeManager<E> {
    let spec = Arc::new(test_spec());
    let op_pool = Arc::new(OperationPool::default());
    SyncCommitteeManager::new(spec, op_pool)
}

// -----------------------------------------------------------------------
// Aggregation pool tests
// -----------------------------------------------------------------------

#[test]
fn get_contribution_returns_none_for_empty_pool() {
    let manager = new_manager();
    let data = SyncContributionData {
        slot: Slot::new(0),
        beacon_block_root: Hash256::zero(),
        subcommittee_index: 0,
    };
    assert!(
        manager
            .get_aggregated_sync_committee_contribution(&data)
            .is_none(),
        "empty pool should return None"
    );
}

#[test]
fn add_to_naive_sync_aggregation_pool_stores_contribution() {
    let manager = new_manager();
    let slot = Slot::new(1);
    let block_root = Hash256::from_low_u64_be(42);

    let sync_message = SyncCommitteeMessage {
        slot,
        beacon_block_root: block_root,
        validator_index: 0,
        signature: bls::Signature::empty().into(),
    };

    let subnet_id = SyncSubnetId::new(0);
    let position = 0_usize;
    let mut subnet_positions = HashMap::new();
    subnet_positions.insert(subnet_id, vec![position]);

    let verified = VerifiedSyncCommitteeMessage::new_for_test(sync_message, subnet_positions);

    let result = manager.add_to_naive_sync_aggregation_pool(verified);
    assert!(result.is_ok(), "inserting into empty pool should succeed");

    // The contribution should now be retrievable.
    let data = SyncContributionData {
        slot,
        beacon_block_root: block_root,
        subcommittee_index: u64::from(subnet_id),
    };
    let contribution = manager.get_aggregated_sync_committee_contribution(&data);
    assert!(
        contribution.is_some(),
        "pool should contain the contribution we just inserted"
    );
    let contribution = contribution.unwrap();
    assert_eq!(contribution.slot, slot);
    assert_eq!(contribution.beacon_block_root, block_root);
    assert!(
        contribution.aggregation_bits.get(position).unwrap_or(false),
        "aggregation bit at the inserted position should be set"
    );
}

#[test]
fn add_to_naive_sync_aggregation_pool_aggregates_multiple_positions() {
    let manager = new_manager();
    let slot = Slot::new(1);
    let block_root = Hash256::from_low_u64_be(99);
    let subnet_id = SyncSubnetId::new(0);

    // Insert first message at position 0.
    let msg1 = SyncCommitteeMessage {
        slot,
        beacon_block_root: block_root,
        validator_index: 0,
        signature: bls::Signature::empty().into(),
    };
    let mut positions1 = HashMap::new();
    positions1.insert(subnet_id, vec![0]);
    let verified1 = VerifiedSyncCommitteeMessage::new_for_test(msg1, positions1);
    manager
        .add_to_naive_sync_aggregation_pool(verified1)
        .expect("first insert should succeed");

    // Insert second message at position 1.
    let msg2 = SyncCommitteeMessage {
        slot,
        beacon_block_root: block_root,
        validator_index: 1,
        signature: bls::Signature::empty().into(),
    };
    let mut positions2 = HashMap::new();
    positions2.insert(subnet_id, vec![1]);
    let verified2 = VerifiedSyncCommitteeMessage::new_for_test(msg2, positions2);
    manager
        .add_to_naive_sync_aggregation_pool(verified2)
        .expect("second insert should succeed");

    // Both bits should be set.
    let data = SyncContributionData {
        slot,
        beacon_block_root: block_root,
        subcommittee_index: u64::from(subnet_id),
    };
    let contribution = manager
        .get_aggregated_sync_committee_contribution(&data)
        .expect("pool should contain aggregated contribution");
    assert!(contribution.aggregation_bits.get(0).unwrap_or(false));
    assert!(contribution.aggregation_bits.get(1).unwrap_or(false));
}

// -----------------------------------------------------------------------
// Sync committee duties tests
// -----------------------------------------------------------------------

#[test]
fn sync_committee_duties_returns_results_for_valid_validators() {
    let spec = altair_spec();
    let keypairs = types::test_utils::generate_deterministic_keypairs(VALIDATOR_COUNT);
    let state = interop_genesis_state::<E>(
        &keypairs,
        0,
        Hash256::from_slice(DEFAULT_ETH1_BLOCK_HASH),
        None,
        &spec,
    )
    .expect("should build genesis state");

    let manager = SyncCommitteeManager::new(Arc::new(spec), Arc::new(OperationPool::default()));

    let duties = manager
        .sync_committee_duties(Epoch::new(0), &[0, 1], &state)
        .expect("sync committee duties should succeed with Altair enabled");

    assert_eq!(
        duties.len(),
        2,
        "should return one result per validator index"
    );
    // Each entry should be Ok (the validators exist in the state).
    for duty_result in &duties {
        assert!(
            duty_result.is_ok(),
            "each duty result for a known validator should be Ok, got: {:?}",
            duty_result
        );
    }
}

#[test]
fn sync_committee_duties_unknown_validator_returns_none() {
    let spec = altair_spec();
    let keypairs = types::test_utils::generate_deterministic_keypairs(VALIDATOR_COUNT);
    let state = interop_genesis_state::<E>(
        &keypairs,
        0,
        Hash256::from_slice(DEFAULT_ETH1_BLOCK_HASH),
        None,
        &spec,
    )
    .expect("should build genesis state");

    let manager = SyncCommitteeManager::new(Arc::new(spec), Arc::new(OperationPool::default()));

    let duties = manager
        .sync_committee_duties(Epoch::new(0), &[9999], &state)
        .expect("sync committee duties should succeed with Altair enabled");

    assert_eq!(duties.len(), 1);
    match &duties[0] {
        Ok(None) => {}
        Ok(Some(_)) => {
            panic!("unknown validator should not have duties");
        }
        Err(_) => {
            // Also acceptable -- validator index out of range.
        }
    }
}
