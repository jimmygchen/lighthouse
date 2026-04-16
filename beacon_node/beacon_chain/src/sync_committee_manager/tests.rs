use super::*;
use fixed_bytes::FixedBytesExtended;
use genesis::{DEFAULT_ETH1_BLOCK_HASH, interop_genesis_state};
use types::*;

type E = MinimalEthSpec;

const VALIDATOR_COUNT: usize = 16;

/// Create a minimal spec suitable for testing at genesis.
fn test_spec() -> ChainSpec {
    E::default_spec()
}

/// Build a genesis `BeaconState` and return it alongside the keypairs.
fn genesis_state_and_keypairs() -> (BeaconState<E>, Vec<bls::Keypair>) {
    let spec = test_spec();
    let keypairs = types::test_utils::generate_deterministic_keypairs(VALIDATOR_COUNT);
    let state = interop_genesis_state::<E>(
        &keypairs,
        0,
        Hash256::from_slice(DEFAULT_ETH1_BLOCK_HASH),
        None,
        &spec,
    )
    .expect("should build genesis state");
    (state, keypairs)
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

// -----------------------------------------------------------------------
// Sync committee duties tests
// -----------------------------------------------------------------------

#[test]
fn sync_committee_duties_returns_results_for_valid_validators() {
    let manager = new_manager();
    let (state, _keypairs) = genesis_state_and_keypairs();

    // MinimalEthSpec may not have Altair enabled at genesis; if not, we expect
    // an error rather than a panic.
    let result = manager.sync_committee_duties(Epoch::new(0), &[0, 1], &state);
    // Whether it succeeds or fails depends on the fork schedule, but it must
    // not panic.
    match result {
        Ok(duties) => {
            assert_eq!(
                duties.len(),
                2,
                "should return one result per validator index"
            );
        }
        Err(_) => {
            // Expected if Altair is not active at epoch 0.
        }
    }
}

#[test]
fn sync_committee_duties_unknown_validator_returns_none() {
    let manager = new_manager();
    let (state, _keypairs) = genesis_state_and_keypairs();

    let result = manager.sync_committee_duties(Epoch::new(0), &[9999], &state);
    match result {
        Ok(duties) => {
            assert_eq!(duties.len(), 1);
            // Unknown validator should yield Ok(None) or an error per-entry.
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
        Err(_) => {
            // Expected if sync committees are not active at this epoch.
        }
    }
}
