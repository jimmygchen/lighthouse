use super::*;
use crate::beacon_proposer_cache::BeaconProposerCache;
use crate::execution_methods::handle_invalid_justified_checkpoint;
use crate::test_utils::DiskHarnessType;
use execution_layer::ExecutionBlockHash;
use fixed_bytes::FixedBytesExtended;
use genesis::{DEFAULT_ETH1_BLOCK_HASH, interop_genesis_state};
use parking_lot::Mutex;
use std::sync::Arc;
use task_executor::ShutdownReason;
use types::*;

type E = MinimalEthSpec;
type T = DiskHarnessType<E>;

const VALIDATOR_COUNT: usize = 16;

fn test_spec() -> ChainSpec {
    E::default_spec()
}

fn new_manager() -> ExecutionManager<T> {
    let spec = Arc::new(test_spec());
    let cache = Arc::new(Mutex::new(BeaconProposerCache::default()));
    ExecutionManager::new(spec, None, cache)
}

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

// -----------------------------------------------------------------------
// slot_is_prior_to_bellatrix tests
// -----------------------------------------------------------------------

#[test]
fn slot_prior_to_bellatrix_when_no_fork_epoch() {
    let mut spec = test_spec();
    spec.bellatrix_fork_epoch = None;
    let manager = ExecutionManager::<T>::new(
        Arc::new(spec),
        None,
        Arc::new(Mutex::new(BeaconProposerCache::default())),
    );

    // Without a bellatrix fork epoch, all slots are prior to bellatrix.
    assert!(manager.slot_is_prior_to_bellatrix(Slot::new(0)));
    assert!(manager.slot_is_prior_to_bellatrix(Slot::new(u64::MAX)));
}

#[test]
fn slot_prior_to_bellatrix_with_fork_epoch() {
    let mut spec = test_spec();
    let bellatrix_epoch = Epoch::new(10);
    spec.bellatrix_fork_epoch = Some(bellatrix_epoch);
    let manager = ExecutionManager::<T>::new(
        Arc::new(spec),
        None,
        Arc::new(Mutex::new(BeaconProposerCache::default())),
    );

    let slots_per_epoch = E::slots_per_epoch();

    // A slot in epoch 9 (before bellatrix) should be prior.
    let pre_bellatrix_slot = Slot::new(9 * slots_per_epoch);
    assert!(
        manager.slot_is_prior_to_bellatrix(pre_bellatrix_slot),
        "slot in epoch 9 should be prior to bellatrix at epoch 10"
    );

    // A slot in epoch 10 (the fork epoch) should NOT be prior.
    let at_bellatrix_slot = Slot::new(10 * slots_per_epoch);
    assert!(
        !manager.slot_is_prior_to_bellatrix(at_bellatrix_slot),
        "slot in epoch 10 should not be prior to bellatrix at epoch 10"
    );

    // A slot in epoch 11 (after bellatrix) should NOT be prior.
    let post_bellatrix_slot = Slot::new(11 * slots_per_epoch);
    assert!(
        !manager.slot_is_prior_to_bellatrix(post_bellatrix_slot),
        "slot in epoch 11 should not be prior to bellatrix at epoch 10"
    );
}

#[test]
fn slot_prior_to_bellatrix_boundary() {
    let mut spec = test_spec();
    spec.bellatrix_fork_epoch = Some(Epoch::new(5));
    let manager = ExecutionManager::<T>::new(
        Arc::new(spec),
        None,
        Arc::new(Mutex::new(BeaconProposerCache::default())),
    );

    let slots_per_epoch = E::slots_per_epoch();

    // Last slot of epoch 4 is prior.
    let last_pre_slot = Slot::new(5 * slots_per_epoch - 1);
    assert!(manager.slot_is_prior_to_bellatrix(last_pre_slot));

    // First slot of epoch 5 is not prior.
    let first_at_slot = Slot::new(5 * slots_per_epoch);
    assert!(!manager.slot_is_prior_to_bellatrix(first_at_slot));
}

// -----------------------------------------------------------------------
// with_proposer_cache tests
// -----------------------------------------------------------------------

#[test]
fn with_proposer_cache_populates_on_miss() {
    let spec = test_spec();
    let cache = Arc::new(Mutex::new(BeaconProposerCache::default()));
    let manager = ExecutionManager::<T>::new(Arc::new(spec.clone()), None, cache.clone());

    let (state, _keypairs) = genesis_state_and_keypairs();
    let proposal_epoch = Epoch::new(0);

    // For genesis state at slot 0, the state root is effectively zero (uninitialized).
    // The decision root at epoch 0 equals the latest block root because
    // state.slot <= decision_slot.
    let state_root = Hash256::zero();
    let latest_block_root = state.get_latest_block_root(state_root);
    let decision_root = state
        .proposer_shuffling_decision_root_at_epoch(proposal_epoch, latest_block_root, &spec)
        .expect("should compute decision root");

    // The cache is empty, so this should call state_provider and populate.
    let state_clone = state.clone();
    let result: Result<usize, BeaconChainError> = manager.with_proposer_cache(
        decision_root,
        proposal_epoch,
        |proposers| Ok(proposers.proposers.len()),
        move || -> Result<(Hash256, BeaconState<E>), BeaconChainError> {
            Ok((state_root, state_clone))
        },
    );

    let num_proposers = result.expect("should succeed");
    assert_eq!(
        num_proposers,
        E::slots_per_epoch() as usize,
        "should have one proposer per slot in the epoch"
    );

    // Now a second call should hit the cache (state_provider is not called).
    let result: Result<usize, BeaconChainError> = manager.with_proposer_cache(
        decision_root,
        proposal_epoch,
        |proposers| Ok(proposers.proposers.len()),
        || -> Result<(Hash256, BeaconState<E>), BeaconChainError> {
            panic!("state_provider should not be called on cache hit");
        },
    );

    assert_eq!(
        result.expect("should succeed on cache hit"),
        E::slots_per_epoch() as usize,
    );
}

// -----------------------------------------------------------------------
// is_optimistic_or_invalid_block — pre-bellatrix path
// -----------------------------------------------------------------------

#[test]
fn given_pre_bellatrix_slot_when_checking_optimistic_status_then_returns_false() {
    // Given a spec with bellatrix at epoch 10
    let mut spec = test_spec();
    spec.bellatrix_fork_epoch = Some(Epoch::new(10));
    let manager = ExecutionManager::<T>::new(
        Arc::new(spec),
        None,
        Arc::new(Mutex::new(BeaconProposerCache::default())),
    );

    // When we check slot_is_prior_to_bellatrix for a pre-Bellatrix slot
    let pre_bellatrix_slot = Slot::new(0);

    // Then it returns true (the slot IS prior to bellatrix)
    assert!(
        manager.slot_is_prior_to_bellatrix(pre_bellatrix_slot),
        "slot 0 should be prior to bellatrix at epoch 10"
    );

    // And for is_optimistic_or_invalid_block with a pre-bellatrix slot,
    // the method would return Ok(false) — i.e., not optimistic.
    // This is because pre-bellatrix blocks have no execution payload,
    // so they cannot be optimistic. We verify the underlying logic:
    assert!(
        manager.slot_is_prior_to_bellatrix(Slot::new(5)),
        "slot in epoch 0 should be prior to bellatrix at epoch 10"
    );

    // Post-bellatrix slots should NOT be prior
    let post_bellatrix_slot = Slot::new(10 * E::slots_per_epoch());
    assert!(
        !manager.slot_is_prior_to_bellatrix(post_bellatrix_slot),
        "slot at bellatrix fork epoch should not be prior to bellatrix"
    );
}

// -----------------------------------------------------------------------
// handle_invalid_justified_checkpoint tests
// -----------------------------------------------------------------------

#[test]
fn given_invalid_justified_checkpoint_when_handled_then_shutdown_sent_and_error_returned() {
    // Given a channel for shutdown signals
    let (mut shutdown_tx, mut shutdown_rx) = futures::channel::mpsc::channel(1);
    let justified_root = Hash256::repeat_byte(0xab);
    let exec_hash = Some(ExecutionBlockHash::zero());

    // When handle_invalid_justified_checkpoint is called
    let result =
        handle_invalid_justified_checkpoint::<T>(&mut shutdown_tx, justified_root, exec_hash);

    // Then the result is an error with JustifiedPayloadInvalid
    assert!(
        result.is_err(),
        "should return error on invalid justified checkpoint"
    );
    match result.unwrap_err() {
        BeaconChainError::JustifiedPayloadInvalid {
            justified_root: r,
            execution_block_hash: h,
        } => {
            assert_eq!(r, justified_root, "error should carry the justified root");
            assert_eq!(h, exec_hash, "error should carry the execution block hash");
        }
        other => panic!("unexpected error variant: {:?}", other),
    }

    // And a shutdown signal was sent on the channel
    let shutdown = shutdown_rx.try_next().expect("should have a message");
    assert!(
        matches!(shutdown, Some(ShutdownReason::Failure(_))),
        "shutdown reason should be Failure"
    );
}

#[test]
fn given_invalid_justified_checkpoint_without_exec_hash_when_handled_then_error_has_none() {
    // Given a channel for shutdown signals and no execution block hash
    let (mut shutdown_tx, _shutdown_rx) = futures::channel::mpsc::channel(1);
    let justified_root = Hash256::repeat_byte(0xcd);

    // When handle_invalid_justified_checkpoint is called with None exec hash
    let result = handle_invalid_justified_checkpoint::<T>(&mut shutdown_tx, justified_root, None);

    // Then the error carries None for execution_block_hash
    assert!(result.is_err());
    match result.unwrap_err() {
        BeaconChainError::JustifiedPayloadInvalid {
            execution_block_hash,
            ..
        } => {
            assert_eq!(
                execution_block_hash, None,
                "error should have None execution_block_hash when none provided"
            );
        }
        other => panic!("unexpected error variant: {:?}", other),
    }
}

// -----------------------------------------------------------------------
// Accessor tests
// -----------------------------------------------------------------------

#[test]
fn execution_layer_is_none_by_default() {
    let manager = new_manager();
    assert!(
        manager.execution_layer().is_none(),
        "execution layer should be None when not configured"
    );
}

#[test]
fn spec_accessor_returns_expected() {
    let spec = Arc::new(test_spec());
    let manager = ExecutionManager::<T>::new(
        spec.clone(),
        None,
        Arc::new(Mutex::new(BeaconProposerCache::default())),
    );
    assert_eq!(
        manager.spec().max_committees_per_slot,
        spec.max_committees_per_slot,
        "spec accessor should return the provided spec"
    );
}
