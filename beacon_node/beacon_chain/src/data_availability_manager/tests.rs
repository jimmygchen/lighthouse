use super::*;
use crate::builder::BeaconChainBuilder;
use crate::test_utils::{EphemeralHarnessType, generate_data_column_indices_rand_order, get_kzg};
use genesis::{DEFAULT_ETH1_BLOCK_HASH, interop_genesis_state};
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::time::Duration;
use store::config::StoreConfig;
use task_executor::test_utils::TestRuntime;
use types::MinimalEthSpec;

type E = MinimalEthSpec;

/// Build a minimal chain and return its `DataAvailabilityManager`.
fn build_dam() -> Arc<DataAvailabilityManager<EphemeralHarnessType<E>>> {
    let spec = Arc::new(E::default_spec());
    let kzg = get_kzg(&spec);
    let runtime = TestRuntime::default();

    let genesis_state = interop_genesis_state::<E>(
        &types::test_utils::generate_deterministic_keypairs(4),
        0,
        Hash256::from_slice(DEFAULT_ETH1_BLOCK_HASH),
        None,
        &spec,
    )
    .expect("should build genesis state");

    let store =
        store::HotColdDB::open_ephemeral(StoreConfig::default(), spec.as_ref().clone().into())
            .expect("should open ephemeral store");

    let (shutdown_tx, _) = futures::channel::mpsc::channel(1);

    let chain = BeaconChainBuilder::new(MinimalEthSpec, kzg)
        .custom_spec(spec)
        .store(Arc::new(store))
        .task_executor(runtime.task_executor.clone())
        .genesis_state(genesis_state)
        .expect("should set genesis state")
        .testing_slot_clock(Duration::from_secs(1))
        .expect("should configure testing slot clock")
        .shutdown_sender(shutdown_tx)
        .ordered_custody_column_indices(generate_data_column_indices_rand_order::<E>())
        .rng(Box::new(StdRng::seed_from_u64(42)))
        .build()
        .expect("should build chain");

    chain.data_availability_manager.clone()
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
fn get_column_da_boundary_none_without_fulu() {
    let dam = build_dam();
    assert_eq!(
        dam.get_column_da_boundary(),
        None,
        "get_column_da_boundary should be None when Fulu is not enabled"
    );
}

#[test]
fn get_blobs_missing_root_returns_no_root() {
    let dam = build_dam();
    let result = dam.get_blobs(&Hash256::random());
    assert!(result.is_ok());
    let blobs = result.unwrap();
    assert!(
        blobs.blobs().is_none(),
        "getting blobs for unknown root should return NoRoot"
    );
}
