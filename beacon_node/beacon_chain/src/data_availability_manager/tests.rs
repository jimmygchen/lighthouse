use super::*;
use crate::custody_context::{CustodyContext, NodeCustodyType};
use crate::data_availability_checker::DataAvailabilityChecker;
use crate::test_utils::{EphemeralHarnessType, generate_data_column_indices_rand_order, get_kzg};
use slot_clock::{SlotClock, TestingSlotClock};
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
