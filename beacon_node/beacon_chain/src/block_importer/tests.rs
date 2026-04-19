use super::*;
use crate::test_utils::{BeaconChainHarness, test_spec};
use bls::Keypair;
use std::sync::LazyLock;
use types::MinimalEthSpec;

type E = MinimalEthSpec;

const VALIDATOR_COUNT: usize = 48;

static KEYPAIRS: LazyLock<Vec<Keypair>> =
    LazyLock::new(|| types::test_utils::generate_deterministic_keypairs(VALIDATOR_COUNT));

/// Compile-time regression test: ensures the `BlockImporter` is wired up and reachable from the
/// `BeaconChain` held by `BeaconChainHarness`.
#[tokio::test]
async fn block_importer_is_accessible_on_beacon_components() {
    let spec = Arc::new(test_spec::<E>());
    let harness = BeaconChainHarness::builder(MinimalEthSpec)
        .spec(spec)
        .keypairs(KEYPAIRS[..VALIDATOR_COUNT].to_vec())
        .fresh_ephemeral_store()
        .mock_execution_layer()
        .build();

    harness.advance_slot();
    harness.extend_slots(1).await;

    // Confirm the `BlockImporter` is reachable and the call path compiles.
    let _importer: &Arc<BlockImporter<_>> = &harness.chain.block_importer;
}
