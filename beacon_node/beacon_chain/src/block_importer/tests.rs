use super::*;
use crate::test_utils::{BeaconChainHarness, test_spec};
use bls::{FixedBytesExtended, Keypair, Signature};
use std::collections::HashSet;
use std::sync::LazyLock;
use types::MinimalEthSpec;

type E = MinimalEthSpec;

const VALIDATOR_COUNT: usize = 48;

static KEYPAIRS: LazyLock<Vec<Keypair>> =
    LazyLock::new(|| types::test_utils::generate_deterministic_keypairs(VALIDATOR_COUNT));

fn build_harness() -> BeaconChainHarness<crate::test_utils::EphemeralHarnessType<E>> {
    let spec = Arc::new(test_spec::<E>());
    BeaconChainHarness::builder(MinimalEthSpec)
        .spec(spec)
        .keypairs(KEYPAIRS[..VALIDATOR_COUNT].to_vec())
        .fresh_ephemeral_store()
        .mock_execution_layer()
        .build()
}

// -----------------------------------------------------------------------
// check_invalid_block_roots tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn check_invalid_block_roots_rejects_configured_invalid_root() {
    let invalid_root = Hash256::random();
    let spec = Arc::new(test_spec::<E>());
    let harness = BeaconChainHarness::builder(MinimalEthSpec)
        .spec(spec)
        .keypairs(KEYPAIRS[..VALIDATOR_COUNT].to_vec())
        .fresh_ephemeral_store()
        .mock_execution_layer()
        .chain_config(ChainConfig {
            invalid_block_roots: HashSet::from([invalid_root]),
            ..ChainConfig::default()
        })
        .build();

    let result = harness
        .chain
        .block_importer
        .check_invalid_block_roots(invalid_root);
    assert!(matches!(
        result,
        Err(BlockError::KnownInvalidExecutionPayload(root)) if root == invalid_root
    ));

    // A different root should still be accepted.
    let other_root = Hash256::random();
    assert!(
        harness
            .chain
            .block_importer
            .check_invalid_block_roots(other_root)
            .is_ok()
    );
}

// -----------------------------------------------------------------------
// check_block_against_weak_subjectivity_checkpoint tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn weak_subjectivity_check_passes_without_config() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = &head.snapshot.beacon_state;
    let block_root = head.head_block_root();
    let block = head.snapshot.beacon_block.message();

    // No weak subjectivity checkpoint configured, should always pass.
    let result = harness
        .chain
        .block_importer
        .check_block_against_weak_subjectivity_checkpoint(block, block_root, state);
    assert!(result.is_ok());
}

// -----------------------------------------------------------------------
// import_block_update_metrics_and_events: old-block path
// -----------------------------------------------------------------------

#[tokio::test]
async fn metrics_skips_old_blocks() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block_root = head.head_block_root();
    let block = head.snapshot.beacon_block.message();

    // Use a current_slot far in the future so the block is "old".
    let far_future_slot = Slot::new(10_000);

    // Should not panic, but should skip most metrics/events.
    import_block_update_metrics_and_events(
        &harness.chain.block_importer,
        block,
        block_root,
        Duration::from_secs(100_000),
        PayloadVerificationStatus::Verified,
        far_future_slot,
    );
}

// -----------------------------------------------------------------------
// import_block_observe_attestations tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn observe_attestations_skips_old_epoch() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block = head.snapshot.beacon_block.message();
    let state = &head.snapshot.beacon_state;

    // Use an epoch far in the future so that the state's current_epoch + 1 < current_epoch.
    let far_future_epoch = Epoch::new(1000);
    let mut ctxt = ConsensusContext::new(state.slot());

    // Should skip observation because block is too old relative to current_epoch.
    import_block_observe_attestations(&harness.chain, block, state, &mut ctxt, far_future_epoch);
    // No panic, and no attestation should be observed (the function returns early).
}

// -----------------------------------------------------------------------
// import_block_update_validator_monitor tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn validator_monitor_skips_old_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block = head.snapshot.beacon_block.message();
    let state = &head.snapshot.beacon_state;

    let mut ctxt = ConsensusContext::new(state.slot());

    // Use a current_slot far ahead so the block is considered historic.
    let far_future_slot = Slot::new(100_000);
    let parent_block_slot = Slot::new(0);

    import_block_update_validator_monitor(
        &harness.chain,
        block,
        state,
        &mut ctxt,
        far_future_slot,
        parent_block_slot,
    );
    // Should return early without processing.
}

// -----------------------------------------------------------------------
// filter_chain_segment edge cases
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_empty_input() {
    let harness = build_harness();
    harness.advance_slot();

    let result = harness.chain.block_importer.filter_chain_segment(vec![]);
    match result {
        Ok(filtered) => assert!(filtered.is_empty()),
        Err(_) => panic!("filter_chain_segment should succeed with empty input"),
    }
}

// -----------------------------------------------------------------------
// process_rpc_blobs tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn process_rpc_blobs_rejects_already_imported_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block_root = head.head_block_root();
    let slot = head.head_slot();

    // Empty blob list — the DuplicateFullyImported check fires before blobs are inspected.
    let blobs = FixedBlobSidecarList::new(vec![]);

    let result = harness
        .chain
        .block_importer
        .process_rpc_blobs(slot, block_root, blobs)
        .await;

    assert!(
        matches!(result, Err(BlockError::DuplicateFullyImported(root)) if root == block_root),
        "expected DuplicateFullyImported, got {:?}",
        result
    );
}

#[tokio::test]
async fn process_rpc_blobs_rejects_unknown_parent() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let head_block = head.snapshot.beacon_block.message();

    // Use an unknown block root (not in fork choice) so we pass the duplicate check.
    let unknown_block_root = Hash256::random();
    let slot = head.head_slot();

    // Build a blob sidecar whose parent_root is also unknown.
    let unknown_parent = Hash256::random();
    let mut header = head_block.block_header();
    header.parent_root = unknown_parent;
    let signed_header = SignedBeaconBlockHeader {
        message: header,
        signature: Signature::empty(),
    };

    let blob = Arc::new(BlobSidecar {
        index: 0,
        blob: Blob::<E>::default(),
        kzg_commitment: KzgCommitment::empty_for_testing(),
        kzg_proof: KzgProof::empty(),
        signed_block_header: signed_header,
        kzg_commitment_inclusion_proof: Default::default(),
    });

    let blobs = FixedBlobSidecarList::new(vec![Some(blob)]);

    let result = harness
        .chain
        .block_importer
        .process_rpc_blobs(slot, unknown_block_root, blobs)
        .await;

    assert!(
        matches!(result, Err(BlockError::ParentUnknown { parent_root }) if parent_root == unknown_parent),
        "expected ParentUnknown, got {:?}",
        result
    );
}

// -----------------------------------------------------------------------
// process_engine_blobs tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn process_engine_blobs_rejects_already_imported_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block_root = head.head_block_root();
    let slot = head.head_slot();

    // Use an empty blobs variant — the duplicate check happens first.
    let engine_output = crate::fetch_blobs::EngineGetBlobsOutput::Blobs(vec![]);

    let result = harness
        .chain
        .block_importer
        .process_engine_blobs(slot, block_root, engine_output)
        .await;

    assert!(
        matches!(result, Err(BlockError::DuplicateFullyImported(root)) if root == block_root),
        "expected DuplicateFullyImported, got {:?}",
        result
    );
}

// -----------------------------------------------------------------------
// process_rpc_custody_columns tests
// -----------------------------------------------------------------------

/// Helper to build a minimal `DataColumnSidecar` (Fulu variant) for testing.
fn make_test_data_column_sidecar(
    slot: Slot,
    block_root_seed: u64,
    parent_root: Hash256,
    index: u64,
) -> Arc<DataColumnSidecar<E>> {
    let header = BeaconBlockHeader {
        slot,
        proposer_index: 0,
        parent_root,
        state_root: Hash256::ZERO,
        body_root: Hash256::from_low_u64_be(block_root_seed),
    };
    let signed_header = SignedBeaconBlockHeader {
        message: header,
        signature: Signature::empty(),
    };
    Arc::new(DataColumnSidecar::Fulu(DataColumnSidecarFulu {
        index,
        column: vec![].try_into().unwrap(),
        kzg_commitments: vec![].try_into().unwrap(),
        kzg_proofs: vec![].try_into().unwrap(),
        signed_block_header: signed_header,
        kzg_commitments_inclusion_proof: vec![
            Hash256::ZERO;
            E::kzg_commitments_inclusion_proof_depth()
        ]
        .try_into()
        .unwrap(),
    }))
}

#[tokio::test]
async fn process_rpc_custody_columns_rejects_columns_from_different_blocks() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let parent = Hash256::random();
    // Two columns with different body_root seeds -> different block_root values.
    let col_a = make_test_data_column_sidecar(Slot::new(99), 1, parent, 0);
    let col_b = make_test_data_column_sidecar(Slot::new(99), 2, parent, 1);

    assert_ne!(
        col_a.block_root(),
        col_b.block_root(),
        "columns must have different block roots for this test"
    );

    let result = harness
        .chain
        .block_importer
        .process_rpc_custody_columns(vec![col_a, col_b])
        .await;

    assert!(
        matches!(result, Err(BlockError::InternalError(ref msg)) if msg.contains("same block")),
        "expected InternalError about columns from same block, got {:?}",
        result
    );
}

#[tokio::test]
async fn process_rpc_custody_columns_rejects_already_imported_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let head_block = head.snapshot.beacon_block.message();
    let block_root = head.head_block_root();

    // Build a column whose signed_block_header tree-hash-root matches the imported block root.
    // The easiest way: use the actual block header from the imported block.
    let signed_header = SignedBeaconBlockHeader {
        message: head_block.block_header(),
        signature: Signature::empty(),
    };
    let column = Arc::new(DataColumnSidecar::Fulu(DataColumnSidecarFulu {
        index: 0,
        column: vec![].try_into().unwrap(),
        kzg_commitments: vec![].try_into().unwrap(),
        kzg_proofs: vec![].try_into().unwrap(),
        signed_block_header: signed_header,
        kzg_commitments_inclusion_proof: vec![
            Hash256::ZERO;
            E::kzg_commitments_inclusion_proof_depth()
        ]
        .try_into()
        .unwrap(),
    }));

    assert_eq!(
        column.block_root(),
        block_root,
        "column block_root should match the imported block"
    );

    let result = harness
        .chain
        .block_importer
        .process_rpc_custody_columns(vec![column])
        .await;

    assert!(
        matches!(result, Err(BlockError::DuplicateFullyImported(root)) if root == block_root),
        "expected DuplicateFullyImported, got {:?}",
        result
    );
}

#[tokio::test]
async fn process_rpc_custody_columns_rejects_unknown_parent() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    // Column with unknown block_root (not in fork choice) and unknown parent_root.
    let unknown_parent = Hash256::random();
    let column = make_test_data_column_sidecar(Slot::new(99), 42, unknown_parent, 0);

    // Ensure the column's block_root is not in fork choice.
    assert!(
        !harness
            .chain
            .canonical_head
            .fork_choice_read_lock()
            .contains_block(&column.block_root()),
        "column block_root should not be in fork choice"
    );

    let result = harness
        .chain
        .block_importer
        .process_rpc_custody_columns(vec![column])
        .await;

    assert!(
        matches!(result, Err(BlockError::ParentUnknown { parent_root }) if parent_root == unknown_parent),
        "expected ParentUnknown, got {:?}",
        result
    );
}
