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
    import_block_observe_attestations(
        &harness.chain.block_importer,
        block,
        state,
        &mut ctxt,
        far_future_epoch,
    );
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
        &harness.chain.block_importer,
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

// -----------------------------------------------------------------------
// Helper: wrap a SignedBeaconBlock into a RangeSyncBlock (no-data variant)
// -----------------------------------------------------------------------

fn wrap_in_range_sync_block(
    harness: &BeaconChainHarness<crate::test_utils::EphemeralHarnessType<E>>,
    block: Arc<SignedBeaconBlock<E>>,
) -> RangeSyncBlock<E> {
    use crate::block_verification_types::AvailableBlockData;
    RangeSyncBlock::new(
        block,
        AvailableBlockData::NoData,
        harness
            .chain
            .data_availability_manager
            .data_availability_checker(),
        harness.chain.spec.clone(),
    )
    .expect("should create RangeSyncBlock with NoData")
}

// -----------------------------------------------------------------------
// check_block_relevancy via filter_chain_segment: genesis block (slot 0)
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_skips_genesis_block() {
    let harness = build_harness();
    harness.advance_slot();

    // Construct a block at slot 0 (genesis). Use BeaconBlock::empty which produces slot 0.
    let spec = test_spec::<E>();
    let genesis_block = BeaconBlock::empty(&spec);
    let signed_genesis = SignedBeaconBlock::from_block(genesis_block, Signature::empty());
    let range_block = wrap_in_range_sync_block(&harness, Arc::new(signed_genesis));

    // filter_chain_segment should skip the genesis block (GenesisBlock error => continue).
    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_block]);
    match result {
        Ok(filtered) => assert!(
            filtered.is_empty(),
            "genesis block should be filtered out, but got {} blocks",
            filtered.len()
        ),
        Err(_) => panic!("filter_chain_segment should not return an error for genesis block"),
    }
}

// -----------------------------------------------------------------------
// check_block_relevancy via filter_chain_segment: future block
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_rejects_future_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = head.snapshot.beacon_state.clone();

    // Advance state well past the current clock slot.
    let far_future_slot = Slot::new(1000);
    let ((block, _blobs), _state) = harness.make_block(state, far_future_slot).await;
    let range_block = wrap_in_range_sync_block(&harness, block);

    // The slot clock is only at slot ~2, but the block is at slot 1000 => FutureSlot.
    // filter_chain_segment hits the `_ => break` arm, returning Ok with empty filtered list.
    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_block]);
    match result {
        Ok(filtered) => assert!(
            filtered.is_empty(),
            "future block should be filtered out (break), but got {} blocks",
            filtered.len()
        ),
        Err(_) => panic!("filter_chain_segment should not return an error for a future block"),
    }
}

// -----------------------------------------------------------------------
// check_block_relevancy via filter_chain_segment: finalized slot
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_skips_finalized_slot_block() {
    let harness = build_harness();
    harness.advance_slot();

    // Advance the chain far enough to finalize some epochs.
    // With MinimalEthSpec (8 slots/epoch), we need at least 4 epochs (32 slots) with full
    // attestations to finalize past genesis (finality requires 2 justified epochs).
    harness.extend_slots(32).await;

    let finalized_epoch = harness
        .chain
        .canonical_head
        .cached_head()
        .finalized_checkpoint()
        .epoch;
    assert!(
        finalized_epoch > Epoch::new(0),
        "chain should have finalized past genesis, finalized_epoch={}",
        finalized_epoch
    );

    // Build a block at a slot that is finalized.
    // We use slot 1 which should be within the finalized range.
    let spec = test_spec::<E>();
    let mut block = BeaconBlock::empty(&spec);
    *block.slot_mut() = Slot::new(1);
    let signed_block = SignedBeaconBlock::from_block(block, Signature::empty());
    let range_block = wrap_in_range_sync_block(&harness, Arc::new(signed_block));

    // filter_chain_segment should skip the block (WouldRevertFinalizedSlot => continue).
    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_block]);
    match result {
        Ok(filtered) => assert!(
            filtered.is_empty(),
            "block at finalized slot should be filtered out, but got {} blocks",
            filtered.len()
        ),
        Err(_) => {
            panic!("filter_chain_segment should not return an error for finalized slot block")
        }
    }
}

// -----------------------------------------------------------------------
// check_block_relevancy via filter_chain_segment: duplicate block
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_skips_duplicate_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(2).await;

    // Get the head block which is already imported into fork choice.
    let head_block = harness.get_head_block();

    // filter_chain_segment should skip it (DuplicateFullyImported => continue).
    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![head_block]);
    match result {
        Ok(filtered) => assert!(
            filtered.is_empty(),
            "duplicate block should be filtered out, but got {} blocks",
            filtered.len()
        ),
        Err(_) => panic!("filter_chain_segment should not return an error for duplicate block"),
    }
}

// -----------------------------------------------------------------------
// filter_chain_segment: non-linear parent roots
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_rejects_non_linear_parent_roots() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(2).await;

    let head = harness.chain.canonical_head.cached_head();
    let head_slot = head.head_slot();

    // Build two synthetic blocks at consecutive slots where the second block's parent_root
    // does NOT match the first block's block_root (i.e., non-linear parent chain).
    let spec = test_spec::<E>();

    let mut block_a = BeaconBlock::empty(&spec);
    *block_a.slot_mut() = head_slot + 1;
    *block_a.parent_root_mut() = head.head_block_root();
    let signed_a = Arc::new(SignedBeaconBlock::from_block(block_a, Signature::empty()));

    let mut block_b = BeaconBlock::empty(&spec);
    *block_b.slot_mut() = head_slot + 2;
    // Set parent_root to a random value that won't match block_a's root.
    *block_b.parent_root_mut() = Hash256::random();
    let signed_b = Arc::new(SignedBeaconBlock::from_block(block_b, Signature::empty()));

    let range_a = wrap_in_range_sync_block(&harness, signed_a);
    let range_b = wrap_in_range_sync_block(&harness, signed_b);

    // Advance slot clock so these blocks are not rejected as FutureSlot.
    harness.advance_slot();
    harness.advance_slot();

    // block_a is at index 0, block_b is at index 1.
    // children[0] = (block_b.parent_root, block_b.slot) — block_b.parent_root != block_a.block_root
    // => NonLinearParentRoots
    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_a, range_b]);
    assert!(
        matches!(
            result,
            Err(ref seg) if matches!(
                seg.as_ref(),
                ChainSegmentResult::Failed { error: BlockError::NonLinearParentRoots, .. }
            )
        ),
        "expected NonLinearParentRoots error from filter_chain_segment"
    );
}

// -----------------------------------------------------------------------
// filter_chain_segment: non-linear slots
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_rejects_non_linear_slots() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(4).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = head.snapshot.beacon_state.clone();

    // Make two blocks at the same slot. The second block has the first as parent.
    let next_slot = head.head_slot() + 1;
    let ((block_a, _), _) = harness.make_block(state.clone(), next_slot).await;
    let block_a_root = block_a.canonical_root();

    // Make another block also at next_slot but with block_a's root as parent root.
    let ((block_b, _), _) = harness
        .make_block_with_modifier(state, next_slot, |b| {
            *b.parent_root_mut() = block_a_root;
        })
        .await;

    let range_a = wrap_in_range_sync_block(&harness, block_a);
    let range_b = wrap_in_range_sync_block(&harness, block_b);

    // Both at the same slot: child_slot <= block.slot() => NonLinearSlots
    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_a, range_b]);
    assert!(
        matches!(
            result,
            Err(ref seg) if matches!(
                seg.as_ref(),
                ChainSegmentResult::Failed { error: BlockError::NonLinearSlots, .. }
            )
        ),
        "expected NonLinearSlots error from filter_chain_segment"
    );
}

// -----------------------------------------------------------------------
// process_chain_segment: empty segment returns Successful with no blocks
// -----------------------------------------------------------------------

#[tokio::test]
async fn process_chain_segment_empty_returns_successful() {
    let harness = build_harness();
    harness.advance_slot();

    let result = harness
        .chain
        .block_importer
        .process_chain_segment(vec![], NotifyExecutionLayer::Yes, &harness.chain)
        .await;

    match result {
        ChainSegmentResult::Successful { imported_blocks } => {
            assert!(
                imported_blocks.is_empty(),
                "empty segment should import zero blocks"
            );
        }
        ChainSegmentResult::Failed { error, .. } => {
            panic!(
                "expected Successful for empty segment, got Failed: {:?}",
                error
            );
        }
    }
}

// -----------------------------------------------------------------------
// verify_header_signature_inline: unknown validator index
// -----------------------------------------------------------------------

#[tokio::test]
async fn verify_header_signature_rejects_unknown_validator() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let mut header = head.snapshot.beacon_block.message().block_header();

    // Set proposer_index to a value beyond the known validator set.
    header.proposer_index = 999_999;

    let signed_header = SignedBeaconBlockHeader {
        message: header,
        signature: Signature::empty(),
    };

    let result = verify_header_signature_inline::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.block_importer,
        &signed_header,
    );

    assert!(
        matches!(result, Err(BlockError::UnknownValidator(999_999))),
        "expected UnknownValidator(999999), got {:?}",
        result
    );
}

// -----------------------------------------------------------------------
// verify_header_signature_inline: invalid signature
// -----------------------------------------------------------------------

#[tokio::test]
async fn verify_header_signature_rejects_invalid_signature() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let header = head.snapshot.beacon_block.message().block_header();

    // Use a valid proposer_index but an empty (invalid) signature.
    let signed_header = SignedBeaconBlockHeader {
        message: header,
        signature: Signature::empty(),
    };

    let result = verify_header_signature_inline::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.block_importer,
        &signed_header,
    );

    assert!(
        matches!(result, Err(BlockError::InvalidSignature(_))),
        "expected InvalidSignature, got {:?}",
        result
    );
}

// -----------------------------------------------------------------------
// verify_header_signature_inline: valid signature
// -----------------------------------------------------------------------

#[tokio::test]
async fn verify_header_signature_accepts_valid_signature() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let signed_block = &head.snapshot.beacon_block;

    // Extract the actual signed block header (with valid signature from the real proposer).
    let signed_header = signed_block.signed_block_header();

    let result = verify_header_signature_inline::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.block_importer,
        &signed_header,
    );

    assert!(
        result.is_ok(),
        "expected valid header signature to pass, got {:?}",
        result
    );
}

// -----------------------------------------------------------------------
// check_block_relevancy via filter_chain_segment: block at max slot
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_rejects_max_slot_block() {
    let harness = build_harness();
    harness.advance_slot();

    // Construct a block at MAXIMUM_BLOCK_SLOT_NUMBER (2^32). This should be rejected
    // by check_block_relevancy and hit the `_ => break` arm in filter_chain_segment.
    let spec = test_spec::<E>();
    let mut block = BeaconBlock::empty(&spec);
    *block.slot_mut() = Slot::new(MAXIMUM_BLOCK_SLOT_NUMBER);
    let signed_block = SignedBeaconBlock::from_block(block, Signature::empty());
    let range_block = wrap_in_range_sync_block(&harness, Arc::new(signed_block));

    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_block]);
    match result {
        Ok(filtered) => assert!(
            filtered.is_empty(),
            "block at max slot should be filtered out (break), but got {} blocks",
            filtered.len()
        ),
        Err(_) => panic!("filter_chain_segment should not error for max slot block"),
    }
}

// -----------------------------------------------------------------------
// filter_chain_segment: valid block passes through
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_passes_valid_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(2).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = head.snapshot.beacon_state.clone();

    // Make a new block that is valid and not yet imported.
    let next_slot = head.head_slot() + 1;
    harness.advance_slot();
    let ((block, _blobs), _state) = harness.make_block(state, next_slot).await;
    let range_block = wrap_in_range_sync_block(&harness, block);

    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_block]);
    match result {
        Ok(filtered) => assert_eq!(
            filtered.len(),
            1,
            "valid unimported block should pass through filter"
        ),
        Err(_seg) => panic!("filter_chain_segment should not error for valid block"),
    }
}

// =======================================================================
// New tests — targeting uncovered code paths
// =======================================================================

// -----------------------------------------------------------------------
// ChainSegmentResult::into_block_error
// -----------------------------------------------------------------------

#[test]
fn into_block_error_returns_ok_for_successful() {
    let result = ChainSegmentResult::Successful {
        imported_blocks: vec![(Hash256::random(), Slot::new(1))],
    };
    assert!(
        result.into_block_error().is_ok(),
        "Successful variant should return Ok(())"
    );
}

#[test]
fn into_block_error_returns_err_for_failed() {
    let result = ChainSegmentResult::Failed {
        imported_blocks: vec![(Hash256::random(), Slot::new(1))],
        error: BlockError::GenesisBlock,
    };
    let err = result
        .into_block_error()
        .expect_err("Failed variant should return Err");
    assert!(
        matches!(err, BlockError::GenesisBlock),
        "expected GenesisBlock error"
    );
}

// -----------------------------------------------------------------------
// verify_weak_subjectivity_checkpoint: all branches
// -----------------------------------------------------------------------

#[tokio::test]
async fn wss_same_epoch_matching_root_passes() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = &head.snapshot.beacon_state;
    let block_root = head.head_block_root();
    let finalized = state.finalized_checkpoint();

    let wss = Checkpoint {
        epoch: finalized.epoch,
        root: finalized.root,
    };

    let result = verify_weak_subjectivity_checkpoint::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.store,
        wss,
        block_root,
        state,
    );
    assert!(result.is_ok(), "matching WSS checkpoint should pass");
}

#[tokio::test]
async fn wss_same_epoch_different_root_fails() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = &head.snapshot.beacon_state;
    let block_root = head.head_block_root();
    let finalized = state.finalized_checkpoint();

    let wss = Checkpoint {
        epoch: finalized.epoch,
        root: Hash256::random(),
    };

    let result = verify_weak_subjectivity_checkpoint::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.store,
        wss,
        block_root,
        state,
    );
    assert!(
        matches!(
            result,
            Err(BeaconChainError::WeakSubjectivtyVerificationFailure)
        ),
        "WSS with same epoch but different root should fail"
    );
}

#[tokio::test]
async fn wss_future_epoch_passes() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = &head.snapshot.beacon_state;
    let block_root = head.head_block_root();
    let finalized = state.finalized_checkpoint();

    let wss = Checkpoint {
        epoch: finalized.epoch + 100,
        root: Hash256::random(),
    };

    let result = verify_weak_subjectivity_checkpoint::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.store,
        wss,
        block_root,
        state,
    );
    assert!(
        result.is_ok(),
        "WSS with future epoch should pass (no check performed)"
    );
}

#[tokio::test]
async fn wss_past_epoch_root_not_found_fails() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(32).await;

    let head = harness.chain.canonical_head.cached_head();
    let mut state = head.snapshot.beacon_state.clone();
    let block_root = head.head_block_root();

    assert!(
        state.finalized_checkpoint().epoch > Epoch::new(0),
        "need finalized epoch > 0"
    );

    // Set finalized checkpoint to a high epoch so WSS epoch is less.
    *state.finalized_checkpoint_mut() = Checkpoint {
        epoch: Epoch::new(10_000),
        root: Hash256::random(),
    };

    let wss = Checkpoint {
        epoch: Epoch::new(9_999),
        root: Hash256::random(),
    };

    let result = verify_weak_subjectivity_checkpoint::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.store,
        wss,
        block_root,
        &state,
    );
    assert!(
        matches!(
            result,
            Err(BeaconChainError::WeakSubjectivtyVerificationFailure)
        ),
        "WSS past epoch with root not found should fail"
    );
}

#[tokio::test]
async fn wss_past_epoch_matching_root_passes() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(32).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = head.snapshot.beacon_state.clone();
    let block_root = head.head_block_root();

    assert!(
        state.finalized_checkpoint().epoch > Epoch::new(0),
        "need finalized epoch > 0"
    );

    let epoch_0_slot = Slot::new(0);
    let root_at_epoch_0 = crate::state_query::root_at_slot_from_state::<
        crate::test_utils::EphemeralHarnessType<E>,
    >(&harness.chain.store, epoch_0_slot, block_root, &state)
    .expect("should read block root iterator")
    .expect("should find root at slot 0");

    let wss = Checkpoint {
        epoch: Epoch::new(0),
        root: root_at_epoch_0,
    };

    let result = verify_weak_subjectivity_checkpoint::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.store,
        wss,
        block_root,
        &state,
    );
    assert!(
        result.is_ok(),
        "WSS past epoch with matching root should pass"
    );
}

#[tokio::test]
async fn wss_past_epoch_different_root_fails() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(32).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = head.snapshot.beacon_state.clone();
    let block_root = head.head_block_root();

    assert!(
        state.finalized_checkpoint().epoch > Epoch::new(0),
        "need finalized epoch > 0"
    );

    let wss = Checkpoint {
        epoch: Epoch::new(0),
        root: Hash256::random(),
    };

    let result = verify_weak_subjectivity_checkpoint::<crate::test_utils::EphemeralHarnessType<E>>(
        &harness.chain.store,
        wss,
        block_root,
        &state,
    );
    assert!(
        matches!(
            result,
            Err(BeaconChainError::WeakSubjectivtyVerificationFailure)
        ),
        "WSS past epoch with wrong root should fail"
    );
}

// -----------------------------------------------------------------------
// filter_chain_segment: inconsistent fork
// -----------------------------------------------------------------------

#[tokio::test]
async fn filter_chain_segment_rejects_inconsistent_fork() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(2).await;

    // Create a Phase0/Base block using the default spec (Phase0 at genesis).
    // The harness uses test_spec (Bellatrix at genesis), so any Base block triggers a fork
    // mismatch.
    let default_spec = E::default_spec();
    let mut base_block = BeaconBlock::empty(&default_spec);
    *base_block.slot_mut() = Slot::new(5);
    let signed_base = SignedBeaconBlock::from_block(base_block, Signature::empty());

    for _ in 0..6 {
        harness.advance_slot();
    }

    let range_block = wrap_in_range_sync_block(&harness, Arc::new(signed_base));

    let result = harness
        .chain
        .block_importer
        .filter_chain_segment(vec![range_block]);
    assert!(
        matches!(
            result,
            Err(ref seg) if matches!(
                seg.as_ref(),
                ChainSegmentResult::Failed { error: BlockError::InconsistentFork(_), .. }
            )
        ),
        "expected InconsistentFork error"
    );
}

// -----------------------------------------------------------------------
// check_blob_header_signature_and_slashability
// -----------------------------------------------------------------------

#[tokio::test]
async fn blob_header_check_passes_with_empty_blobs() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let block_root = Hash256::random();
    let empty: Vec<&BlobSidecar<E>> = vec![];

    let result = check_blob_header_signature_and_slashability::<
        crate::test_utils::EphemeralHarnessType<E>,
    >(&harness.chain.block_importer, block_root, empty);
    assert!(result.is_ok(), "empty blobs should pass");
}

#[tokio::test]
async fn blob_header_check_rejects_invalid_signature() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let header = head.snapshot.beacon_block.message().block_header();
    let block_root = Hash256::random();

    let signed_header = SignedBeaconBlockHeader {
        message: header,
        signature: Signature::empty(),
    };

    let blob = BlobSidecar {
        index: 0,
        blob: Blob::<E>::default(),
        kzg_commitment: KzgCommitment::empty_for_testing(),
        kzg_proof: KzgProof::empty(),
        signed_block_header: signed_header,
        kzg_commitment_inclusion_proof: Default::default(),
    };

    let result = check_blob_header_signature_and_slashability::<
        crate::test_utils::EphemeralHarnessType<E>,
    >(&harness.chain.block_importer, block_root, [&blob]);
    assert!(
        matches!(result, Err(BlockError::InvalidSignature(_))),
        "blob with invalid header signature should fail"
    );
}

#[tokio::test]
async fn blob_header_check_accepts_valid_signature() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let signed_block = &head.snapshot.beacon_block;
    let block_root = head.head_block_root();

    let signed_header = signed_block.signed_block_header();

    let blob = BlobSidecar {
        index: 0,
        blob: Blob::<E>::default(),
        kzg_commitment: KzgCommitment::empty_for_testing(),
        kzg_proof: KzgProof::empty(),
        signed_block_header: signed_header,
        kzg_commitment_inclusion_proof: Default::default(),
    };

    let result = check_blob_header_signature_and_slashability::<
        crate::test_utils::EphemeralHarnessType<E>,
    >(&harness.chain.block_importer, block_root, [&blob]);
    assert!(
        result.is_ok(),
        "blob with valid header signature should pass"
    );
}

// -----------------------------------------------------------------------
// check_data_column_sidecar_header_signature_and_slashability
// -----------------------------------------------------------------------

fn make_test_fulu_sidecar(header: SignedBeaconBlockHeader, index: u64) -> DataColumnSidecarFulu<E> {
    DataColumnSidecarFulu {
        index,
        column: vec![].try_into().unwrap(),
        kzg_commitments: vec![].try_into().unwrap(),
        kzg_proofs: vec![].try_into().unwrap(),
        signed_block_header: header,
        kzg_commitments_inclusion_proof: vec![
            Hash256::ZERO;
            E::kzg_commitments_inclusion_proof_depth()
        ]
        .try_into()
        .unwrap(),
    }
}

#[tokio::test]
async fn column_header_check_passes_with_empty_columns() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let block_root = Hash256::random();
    let empty: Vec<&DataColumnSidecarFulu<E>> = vec![];

    let result = check_data_column_sidecar_header_signature_and_slashability::<
        crate::test_utils::EphemeralHarnessType<E>,
    >(&harness.chain.block_importer, block_root, empty);
    assert!(result.is_ok(), "empty columns should pass");
}

#[tokio::test]
async fn column_header_check_rejects_invalid_signature() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let header = head.snapshot.beacon_block.message().block_header();
    let block_root = Hash256::random();

    let signed_header = SignedBeaconBlockHeader {
        message: header,
        signature: Signature::empty(),
    };

    let sidecar = make_test_fulu_sidecar(signed_header, 0);

    let result = check_data_column_sidecar_header_signature_and_slashability::<
        crate::test_utils::EphemeralHarnessType<E>,
    >(&harness.chain.block_importer, block_root, [&sidecar]);
    assert!(
        matches!(result, Err(BlockError::InvalidSignature(_))),
        "column with invalid header signature should fail"
    );
}

#[tokio::test]
async fn column_header_check_accepts_valid_signature() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let signed_block = &head.snapshot.beacon_block;
    let block_root = head.head_block_root();

    let signed_header = signed_block.signed_block_header();
    let sidecar = make_test_fulu_sidecar(signed_header, 0);

    let result = check_data_column_sidecar_header_signature_and_slashability::<
        crate::test_utils::EphemeralHarnessType<E>,
    >(&harness.chain.block_importer, block_root, [&sidecar]);
    assert!(
        result.is_ok(),
        "column with valid header signature should pass"
    );
}

// -----------------------------------------------------------------------
// process_engine_blobs: CustodyColumns variant duplicate check
// -----------------------------------------------------------------------

#[tokio::test]
async fn process_engine_blobs_custody_columns_rejects_already_imported_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block_root = head.head_block_root();
    let slot = head.head_slot();

    let engine_output = crate::fetch_blobs::EngineGetBlobsOutput::CustodyColumns(vec![]);

    let result = harness
        .chain
        .block_importer
        .process_engine_blobs(slot, block_root, engine_output)
        .await;

    assert!(
        matches!(result, Err(BlockError::DuplicateFullyImported(root)) if root == block_root),
        "expected DuplicateFullyImported for engine CustodyColumns"
    );
}

// -----------------------------------------------------------------------
// process_rpc_custody_columns: empty list
// -----------------------------------------------------------------------

#[tokio::test]
async fn process_rpc_custody_columns_rejects_empty_list() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let result = harness
        .chain
        .block_importer
        .process_rpc_custody_columns(vec![])
        .await;

    assert!(
        matches!(result, Err(BlockError::InternalError(ref msg)) if msg.contains("same block")),
        "expected InternalError for empty columns"
    );
}

// -----------------------------------------------------------------------
// import_block_update_metrics_and_events: recent block path
// -----------------------------------------------------------------------

#[tokio::test]
async fn metrics_processes_recent_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block_root = head.head_block_root();
    let block = head.snapshot.beacon_block.message();
    let current_slot = block.slot() + 1;

    import_block_update_metrics_and_events(
        &harness.chain.block_importer,
        block,
        block_root,
        Duration::from_secs(1),
        PayloadVerificationStatus::Verified,
        current_slot,
    );

    let cache = harness.chain.block_importer.block_times_cache.read();
    assert!(
        cache.cache.contains_key(&block_root),
        "block_times_cache should contain an entry for the block"
    );
}

#[tokio::test]
async fn metrics_handles_optimistic_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block_root = head.head_block_root();
    let block = head.snapshot.beacon_block.message();
    let current_slot = block.slot() + 1;

    import_block_update_metrics_and_events(
        &harness.chain.block_importer,
        block,
        block_root,
        Duration::from_secs(1),
        PayloadVerificationStatus::Optimistic,
        current_slot,
    );
}

// -----------------------------------------------------------------------
// import_block_observe_attestations: current epoch path
// -----------------------------------------------------------------------

#[tokio::test]
async fn observe_attestations_processes_current_epoch() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block = head.snapshot.beacon_block.message();
    let state = &head.snapshot.beacon_state;
    let current_epoch = state.current_epoch();
    let mut ctxt = ConsensusContext::new(state.slot());

    import_block_observe_attestations(
        &harness.chain.block_importer,
        block,
        state,
        &mut ctxt,
        current_epoch,
    );
}

// -----------------------------------------------------------------------
// import_block_update_validator_monitor: current epoch path
// -----------------------------------------------------------------------

#[tokio::test]
async fn validator_monitor_processes_current_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block = head.snapshot.beacon_block.message();
    let state = &head.snapshot.beacon_state;
    let mut ctxt = ConsensusContext::new(state.slot());

    import_block_update_validator_monitor(
        &harness.chain.block_importer,
        block,
        state,
        &mut ctxt,
        block.slot(),
        Slot::new(0),
    );
}

// -----------------------------------------------------------------------
// import_block_update_slasher: no slasher configured
// -----------------------------------------------------------------------

#[tokio::test]
async fn slasher_noop_without_slasher_configured() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let block = head.snapshot.beacon_block.message();
    let state = &head.snapshot.beacon_state;
    let mut ctxt = ConsensusContext::new(state.slot());

    assert!(harness.chain.block_importer.slasher.is_none());
    import_block_update_slasher(&harness.chain.block_importer, block, state, &mut ctxt);
}

// -----------------------------------------------------------------------
// emit_sse events: no subscribers
// -----------------------------------------------------------------------

#[tokio::test]
async fn emit_sse_blob_events_noop_without_subscribers() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let block_root = Hash256::random();
    let blobs: Vec<&BlobSidecar<E>> = vec![];
    emit_sse_blob_sidecar_events(&harness.chain.block_importer, &block_root, blobs.into_iter());
}

#[tokio::test]
async fn emit_sse_data_column_events_noop_without_subscribers() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let block_root = Hash256::random();
    let columns: Vec<&DataColumnSidecar<E>> = vec![];
    emit_sse_data_column_sidecar_events(
        &harness.chain.block_importer,
        &block_root,
        columns.into_iter(),
    );
}

// -----------------------------------------------------------------------
// check_block_relevancy: direct tests for each error path
// -----------------------------------------------------------------------

#[tokio::test]
async fn check_block_relevancy_rejects_future_slot() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(1).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = head.snapshot.beacon_state.clone();
    let far_future_slot = Slot::new(1000);
    let ((block, _blobs), _state) = harness.make_block(state, far_future_slot).await;
    let block_root = block.canonical_root();

    let result = harness
        .chain
        .block_importer
        .check_block_relevancy(&block, block_root);
    assert!(
        matches!(result, Err(BlockError::FutureSlot { .. })),
        "expected FutureSlot"
    );
}

#[tokio::test]
async fn check_block_relevancy_rejects_genesis_slot() {
    let harness = build_harness();
    harness.advance_slot();

    let spec = test_spec::<E>();
    let block = BeaconBlock::empty(&spec);
    let signed = SignedBeaconBlock::from_block(block, Signature::empty());
    let block_root = signed.canonical_root();

    let result = harness
        .chain
        .block_importer
        .check_block_relevancy(&signed, block_root);
    assert!(
        matches!(result, Err(BlockError::GenesisBlock)),
        "expected GenesisBlock"
    );
}

#[tokio::test]
async fn check_block_relevancy_rejects_finalized_slot() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(32).await;

    assert!(
        harness
            .chain
            .canonical_head
            .cached_head()
            .finalized_checkpoint()
            .epoch
            > Epoch::new(0)
    );

    let spec = test_spec::<E>();
    let mut block = BeaconBlock::empty(&spec);
    *block.slot_mut() = Slot::new(1);
    let signed = SignedBeaconBlock::from_block(block, Signature::empty());
    let block_root = signed.canonical_root();

    let result = harness
        .chain
        .block_importer
        .check_block_relevancy(&signed, block_root);
    assert!(
        matches!(result, Err(BlockError::WouldRevertFinalizedSlot { .. })),
        "expected WouldRevertFinalizedSlot"
    );
}

#[tokio::test]
async fn check_block_relevancy_rejects_duplicate() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(2).await;

    let head = harness.chain.canonical_head.cached_head();
    let head_block = &head.snapshot.beacon_block;
    let block_root = head.head_block_root();

    let result = harness
        .chain
        .block_importer
        .check_block_relevancy(head_block.as_ref(), block_root);
    assert!(
        matches!(result, Err(BlockError::DuplicateFullyImported(_))),
        "expected DuplicateFullyImported"
    );
}

#[tokio::test]
async fn check_block_relevancy_accepts_valid_block() {
    let harness = build_harness();
    harness.advance_slot();
    harness.extend_slots(2).await;

    let head = harness.chain.canonical_head.cached_head();
    let state = head.snapshot.beacon_state.clone();
    let next_slot = head.head_slot() + 1;
    harness.advance_slot();
    let ((block, _blobs), _state) = harness.make_block(state, next_slot).await;
    let block_root = block.canonical_root();

    let result = harness
        .chain
        .block_importer
        .check_block_relevancy(&block, block_root);
    assert!(result.is_ok(), "valid unimported block should be relevant");
}
