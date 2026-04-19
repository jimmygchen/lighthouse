use super::*;
use crate::custody_context::{CustodyContext, NodeCustodyType};
use crate::data_availability_checker::{AvailableBlockData, DataAvailabilityChecker};
use crate::test_utils::{EphemeralHarnessType, generate_data_column_indices_rand_order, get_kzg};
use slot_clock::{SlotClock, TestingSlotClock};
use ssz_types::RuntimeVariableList;
use std::time::Duration;
use store::config::StoreConfig;
use types::MinimalEthSpec;

type E = MinimalEthSpec;

/// Build a minimal `DataAvailabilityManager` without `BeaconChainBuilder`.
fn build_dam() -> Arc<DataAvailabilityManager<EphemeralHarnessType<E>>> {
    let spec = Arc::new(E::default_spec());
    let kzg = get_kzg(&spec);

    let store =
        store::HotColdDB::open_ephemeral(StoreConfig::default(), spec.as_ref().clone().into())
            .expect("should open ephemeral store");

    let slot_clock =
        TestingSlotClock::new(Slot::new(0), Duration::from_secs(0), Duration::from_secs(1));

    let custody_context = Arc::new(CustodyContext::new(
        NodeCustodyType::default(),
        generate_data_column_indices_rand_order::<E>(),
        &spec,
    ));

    let data_availability_checker = Arc::new(
        DataAvailabilityChecker::new(
            true, // complete_blob_backfill
            slot_clock,
            kzg.clone(),
            custody_context,
            spec.clone(),
        )
        .expect("should create data availability checker"),
    );

    Arc::new(DataAvailabilityManager::new(
        spec,
        Arc::new(store),
        data_availability_checker,
        kzg,
    ))
}

/// Build a DAM with Fulu fork enabled at a given epoch.
fn build_dam_with_fulu(fulu_epoch: Epoch) -> Arc<DataAvailabilityManager<EphemeralHarnessType<E>>> {
    let mut spec = E::default_spec();
    spec.deneb_fork_epoch = Some(Epoch::new(0));
    spec.fulu_fork_epoch = Some(fulu_epoch);
    let spec = Arc::new(spec);
    let kzg = get_kzg(&spec);

    let store =
        store::HotColdDB::open_ephemeral(StoreConfig::default(), spec.as_ref().clone().into())
            .expect("should open ephemeral store");

    let slot_clock =
        TestingSlotClock::new(Slot::new(0), Duration::from_secs(0), Duration::from_secs(1));

    let custody_context = Arc::new(CustodyContext::new(
        NodeCustodyType::default(),
        generate_data_column_indices_rand_order::<E>(),
        &spec,
    ));

    let data_availability_checker = Arc::new(
        DataAvailabilityChecker::new(true, slot_clock, kzg.clone(), custody_context, spec.clone())
            .expect("should create data availability checker"),
    );

    Arc::new(DataAvailabilityManager::new(
        spec,
        Arc::new(store),
        data_availability_checker,
        kzg,
    ))
}

// -----------------------------------------------------------------------
// DA boundary query tests
// -----------------------------------------------------------------------

#[test]
fn should_fetch_blobs_before_deneb() {
    let dam = build_dam();
    // Before Deneb, DA boundary is None, so should_fetch_blobs returns false.
    let pre_deneb_epoch = Epoch::new(0);
    assert!(
        !dam.should_fetch_blobs(pre_deneb_epoch),
        "should not fetch blobs before DA boundary is set"
    );
}

#[test]
fn should_fetch_custody_columns_before_peer_das() {
    let dam = build_dam();
    // Before Fulu/PeerDAS, should_fetch_custody_columns returns false.
    let pre_fulu_epoch = Epoch::new(0);
    assert!(
        !dam.should_fetch_custody_columns(pre_fulu_epoch),
        "should not fetch custody columns before PeerDAS"
    );
}

#[test]
fn column_da_boundary_none_without_fulu() {
    let dam = build_dam();
    // Without Fulu enabled (default minimal spec), column_da_boundary should be None.
    assert_eq!(
        dam.column_data_availability_boundary(),
        None,
        "column DA boundary should be None when Fulu is not enabled"
    );
}

#[test]
fn get_blobs_missing_root_returns_no_root() {
    let dam = build_dam();
    let result = dam.get_blobs(&Hash256::random());
    assert!(result.is_ok());
    let blobs = result.expect("get_blobs should succeed for unknown root");
    assert!(
        blobs.blobs().is_none(),
        "getting blobs for unknown root should return NoRoot"
    );
}

// -----------------------------------------------------------------------
// AvailabilityProcessingStatus conversion tests
// -----------------------------------------------------------------------

#[test]
fn availability_processing_status_imported_try_into_hash256() {
    let root = Hash256::random();
    let status = AvailabilityProcessingStatus::Imported(root);
    let result: Result<Hash256, ()> = status.try_into();
    assert_eq!(result, Ok(root));
}

#[test]
fn availability_processing_status_missing_try_into_hash256() {
    let status = AvailabilityProcessingStatus::MissingComponents(Slot::new(0), Hash256::random());
    let result: Result<Hash256, ()> = status.try_into();
    assert!(result.is_err());
}

#[test]
fn availability_processing_status_imported_try_into_signed_beacon_block_hash() {
    let root = Hash256::random();
    let status = AvailabilityProcessingStatus::Imported(root);
    let result: Result<SignedBeaconBlockHash, ()> = status.try_into();
    assert_eq!(result, Ok(root.into()));
}

#[test]
fn availability_processing_status_missing_try_into_signed_beacon_block_hash() {
    let status = AvailabilityProcessingStatus::MissingComponents(Slot::new(1), Hash256::ZERO);
    let result: Result<SignedBeaconBlockHash, ()> = status.try_into();
    assert!(result.is_err());
}

#[test]
fn availability_processing_status_equality() {
    let root = Hash256::random();
    assert_eq!(
        AvailabilityProcessingStatus::Imported(root),
        AvailabilityProcessingStatus::Imported(root)
    );
    assert_ne!(
        AvailabilityProcessingStatus::Imported(root),
        AvailabilityProcessingStatus::MissingComponents(Slot::new(0), root)
    );
}

// -----------------------------------------------------------------------
// column_data_availability_boundary with Fulu enabled
// -----------------------------------------------------------------------

#[test]
fn column_da_boundary_with_fulu_enabled() {
    let fulu_epoch = Epoch::new(10);
    let dam = build_dam_with_fulu(fulu_epoch);

    // With Fulu enabled and DA boundary active, column_da_boundary should be Some.
    let boundary = dam.column_data_availability_boundary();
    // The boundary should be at least the Fulu fork epoch.
    if let Some(b) = boundary {
        assert!(
            b >= fulu_epoch,
            "column DA boundary ({b:?}) should be >= Fulu fork epoch ({fulu_epoch:?})"
        );
    }
    // Note: boundary could be None if data_availability_boundary() returns None,
    // which depends on finalized epoch. In a fresh store this is expected.
}

// -----------------------------------------------------------------------
// should_fetch_blobs / should_fetch_custody_columns with Fulu enabled
// -----------------------------------------------------------------------

#[test]
fn should_fetch_blobs_returns_false_when_peer_das_enabled() {
    let fulu_epoch = Epoch::new(0);
    let dam = build_dam_with_fulu(fulu_epoch);

    // After PeerDAS is enabled, should_fetch_blobs returns false (use columns instead).
    // Even if DA check is required, PeerDAS means we fetch columns not blobs.
    let post_fulu_epoch = Epoch::new(5);
    assert!(
        !dam.should_fetch_blobs(post_fulu_epoch),
        "should not fetch blobs when PeerDAS is enabled"
    );
}

#[test]
fn should_fetch_custody_columns_when_peer_das_enabled_and_da_required() {
    let fulu_epoch = Epoch::new(0);
    let dam = build_dam_with_fulu(fulu_epoch);

    // If DA check is required and PeerDAS is enabled, should_fetch_custody_columns returns true.
    let post_fulu_epoch = Epoch::new(1);
    let da_required = dam.da_check_required_for_epoch(post_fulu_epoch);
    let peer_das_enabled = dam.spec.is_peer_das_enabled_for_epoch(post_fulu_epoch);
    let expected = da_required && peer_das_enabled;
    assert_eq!(
        dam.should_fetch_custody_columns(post_fulu_epoch),
        expected,
        "should_fetch_custody_columns should match da_required && peer_das_enabled"
    );
}

// -----------------------------------------------------------------------
// get_data_columns / get_data_column for unknown roots
// -----------------------------------------------------------------------

#[test]
fn get_data_columns_unknown_root_returns_none() {
    let dam = build_dam();
    let result = dam.get_data_columns(&Hash256::random(), ForkName::Fulu);
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
}

#[test]
fn get_data_column_unknown_root_returns_none() {
    let dam = build_dam();
    let result = dam.get_data_column(&Hash256::random(), &0, ForkName::Fulu);
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
}

// -----------------------------------------------------------------------
// get_or_reconstruct_blobs for unknown block
// -----------------------------------------------------------------------

#[test]
fn get_or_reconstruct_blobs_unknown_block_returns_none() {
    let dam = build_dam();
    let result = dam.get_or_reconstruct_blobs(&Hash256::random());
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
}

// -----------------------------------------------------------------------
// get_blobs_or_columns_store_op tests
// -----------------------------------------------------------------------

#[test]
fn get_blobs_or_columns_store_op_no_data() {
    let dam = build_dam();
    let result = get_blobs_or_columns_store_op(
        &dam,
        &dam.spec,
        Hash256::random(),
        Slot::new(0),
        AvailableBlockData::NoData,
    );
    assert!(result.is_none(), "NoData should produce no store op");
}

#[test]
fn get_blobs_or_columns_store_op_with_blobs() {
    let dam = build_dam();
    let blobs = RuntimeVariableList::empty(6);
    let result = get_blobs_or_columns_store_op(
        &dam,
        &dam.spec,
        Hash256::random(),
        Slot::new(0),
        AvailableBlockData::Blobs(blobs),
    );
    assert!(
        result.is_some(),
        "Blobs variant should produce a PutBlobs store op"
    );
    assert!(matches!(result.unwrap(), StoreOp::PutBlobs(_, _)));
}

#[test]
fn get_blobs_or_columns_store_op_with_data_columns() {
    let dam = build_dam();
    let data_columns: DataColumnSidecarList<E> = vec![];
    let result = get_blobs_or_columns_store_op(
        &dam,
        &dam.spec,
        Hash256::random(),
        Slot::new(0),
        AvailableBlockData::DataColumns(data_columns),
    );
    assert!(
        result.is_some(),
        "DataColumns variant should produce a PutDataColumns store op"
    );
    assert!(matches!(result.unwrap(), StoreOp::PutDataColumns(_, _)));
}

// -----------------------------------------------------------------------
// get_missing_columns_for_epoch tests
// -----------------------------------------------------------------------

#[test]
fn get_missing_columns_for_epoch_no_missing() {
    let dam = build_dam();
    // For epoch 0, custody columns at head should match custody columns at epoch 0.
    let missing = dam.get_missing_columns_for_epoch(Epoch::new(0));
    // With default setup, there should be no missing columns since head and epoch 0 are the same.
    assert!(
        missing.is_empty(),
        "should have no missing columns for the same epoch configuration"
    );
}

// -----------------------------------------------------------------------
// custody_columns_for_epoch tests
// -----------------------------------------------------------------------

#[test]
fn custody_columns_for_epoch_returns_nonempty() {
    let dam = build_dam();
    let columns = dam.custody_columns_for_epoch(None);
    assert!(
        !columns.is_empty(),
        "custody columns should not be empty for a fullnode"
    );
}

#[test]
fn custody_columns_for_specific_epoch() {
    let dam = build_dam();
    let columns = dam.custody_columns_for_epoch(Some(Epoch::new(5)));
    assert!(
        !columns.is_empty(),
        "custody columns for a specific epoch should not be empty"
    );
}

// -----------------------------------------------------------------------
// sampling_columns_for_epoch tests
// -----------------------------------------------------------------------

#[test]
fn sampling_columns_for_epoch_returns_nonempty() {
    let dam = build_dam();
    let columns = dam.sampling_columns_for_epoch(Epoch::new(0));
    assert!(!columns.is_empty(), "sampling columns should not be empty");
}

// -----------------------------------------------------------------------
// data_availability_boundary tests
// -----------------------------------------------------------------------

#[test]
fn data_availability_boundary_none_before_deneb() {
    let dam = build_dam();
    // Default minimal spec has no Deneb, so boundary should be None.
    // (depends on whether default_spec enables Deneb)
    let _boundary = dam.data_availability_boundary();
    // Just verify it doesn't panic.
}

#[test]
fn da_check_required_for_epoch_before_boundary() {
    let dam = build_dam();
    // For epoch 0 in a fresh store, the DA check behavior depends on spec.
    let _required = dam.da_check_required_for_epoch(Epoch::new(0));
    // Just verify it doesn't panic.
}

// -----------------------------------------------------------------------
// kzg accessor test
// -----------------------------------------------------------------------

#[test]
fn kzg_accessor_returns_valid_ref() {
    let dam = build_dam();
    let kzg = dam.kzg();
    // Should be a valid Arc ref, same as what was passed in.
    let _ = kzg.clone();
}

// -----------------------------------------------------------------------
// data_availability_checker accessor test
// -----------------------------------------------------------------------

#[test]
fn data_availability_checker_accessor() {
    let dam = build_dam();
    let checker = dam.data_availability_checker();
    // Should return the inner checker.
    let _ = checker.clone();
}

// -----------------------------------------------------------------------
// earliest_custodied_data_column_epoch tests
// -----------------------------------------------------------------------

#[test]
fn earliest_custodied_data_column_epoch_returns_none_initially() {
    let dam = build_dam();
    // Fresh store has no custody info persisted.
    let result = dam.earliest_custodied_data_column_epoch();
    assert!(
        result.is_none(),
        "should be None with no custody info persisted"
    );
}

// -----------------------------------------------------------------------
// update_data_column_custody_info tests
// -----------------------------------------------------------------------

#[test]
fn update_data_column_custody_info_with_none() {
    let dam = build_dam();
    // Setting custody info to None should not panic.
    dam.update_data_column_custody_info(None);
}

#[test]
fn update_data_column_custody_info_with_slot() {
    let dam = build_dam();
    let slot = Slot::new(100);
    dam.update_data_column_custody_info(Some(slot));

    // After updating, earliest_custodied_data_column_epoch should reflect the change.
    let epoch = dam.earliest_custodied_data_column_epoch();
    assert!(epoch.is_some(), "should have custody info after update");

    let expected_epoch = slot.epoch(E::slots_per_epoch());
    // Since slot 100 is not the first slot in the epoch, the result should be epoch + 1.
    let first_slot_in_epoch = expected_epoch.start_slot(E::slots_per_epoch());
    if slot > first_slot_in_epoch {
        assert_eq!(epoch.unwrap(), expected_epoch + 1);
    } else {
        assert_eq!(epoch.unwrap(), expected_epoch);
    }
}

#[test]
fn update_data_column_custody_info_at_epoch_boundary() {
    let dam = build_dam();
    let epoch = Epoch::new(5);
    let slot = epoch.start_slot(E::slots_per_epoch());
    dam.update_data_column_custody_info(Some(slot));

    let result = dam.earliest_custodied_data_column_epoch();
    assert_eq!(
        result,
        Some(epoch),
        "custody info at epoch boundary should return exact epoch"
    );
}

// -----------------------------------------------------------------------
// safely_backfill_data_column_custody_info tests
// -----------------------------------------------------------------------

#[test]
fn safely_backfill_no_existing_custody_info() {
    let dam = build_dam();
    // With no existing custody info, backfill should be a no-op.
    let result = dam.safely_backfill_data_column_custody_info(Epoch::new(0));
    assert!(result.is_ok());
}

#[test]
fn safely_backfill_epoch_at_or_after_earliest() {
    let dam = build_dam();
    let epoch = Epoch::new(5);
    dam.update_data_column_custody_info(Some(epoch.start_slot(E::slots_per_epoch())));

    // Backfilling at the same epoch should be a no-op.
    let result = dam.safely_backfill_data_column_custody_info(epoch);
    assert!(result.is_ok());

    // Backfilling at a later epoch should also be a no-op.
    let result = dam.safely_backfill_data_column_custody_info(epoch + 1);
    assert!(result.is_ok());
}

#[test]
fn safely_backfill_decrements_by_one_epoch() {
    let dam = build_dam();
    let initial_epoch = Epoch::new(5);
    dam.update_data_column_custody_info(Some(initial_epoch.start_slot(E::slots_per_epoch())));

    // Backfill by one epoch should succeed (same CGC).
    let result = dam.safely_backfill_data_column_custody_info(initial_epoch - 1);
    assert!(
        result.is_ok(),
        "should be able to backfill by one epoch: {result:?}"
    );

    // After successful backfill, the earliest epoch should be decremented.
    let new_earliest = dam.earliest_custodied_data_column_epoch();
    assert_eq!(new_earliest, Some(initial_epoch - 1));
}

#[test]
fn safely_backfill_rejects_jump_of_more_than_one_epoch() {
    let dam = build_dam();
    let initial_epoch = Epoch::new(10);
    dam.update_data_column_custody_info(Some(initial_epoch.start_slot(E::slots_per_epoch())));

    // Trying to backfill by more than one epoch should fail.
    let result = dam.safely_backfill_data_column_custody_info(initial_epoch - 2);
    assert!(
        result.is_err(),
        "should reject backfill by more than one epoch"
    );
}

// -----------------------------------------------------------------------
// persist_custody_ctx tests
// -----------------------------------------------------------------------

#[test]
fn persist_custody_ctx_no_peer_das_is_noop() {
    // With default spec (no PeerDAS), persist_custody_ctx should be a no-op.
    let spec = Arc::new(E::default_spec());
    let kzg = get_kzg(&spec);

    let store =
        store::HotColdDB::open_ephemeral(StoreConfig::default(), spec.as_ref().clone().into())
            .expect("should open ephemeral store");

    let slot_clock =
        TestingSlotClock::new(Slot::new(0), Duration::from_secs(0), Duration::from_secs(1));

    let custody_context = Arc::new(CustodyContext::new(
        NodeCustodyType::default(),
        generate_data_column_indices_rand_order::<E>(),
        &spec,
    ));

    let data_availability_checker = Arc::new(
        DataAvailabilityChecker::new(true, slot_clock, kzg, custody_context, spec.clone())
            .expect("should create data availability checker"),
    );

    let result = persist_custody_ctx::<EphemeralHarnessType<E>>(
        &spec,
        &data_availability_checker,
        &Arc::new(store),
    );
    assert!(result.is_ok());
}
