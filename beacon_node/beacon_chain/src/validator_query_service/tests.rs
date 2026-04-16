use super::*;
use crate::test_utils::EphemeralHarnessType;
use crate::validator_pubkey_cache::ValidatorPubkeyCache;
use bls::Keypair;
use genesis::{DEFAULT_ETH1_BLOCK_HASH, interop_genesis_state};
use logging::create_test_tracing_subscriber;
use std::sync::Arc;
use store::HotColdDB;
use types::{EthSpec, Hash256, MainnetEthSpec};

type E = MainnetEthSpec;
type T = EphemeralHarnessType<E>;

const VALIDATOR_COUNT: usize = 32;

fn test_spec() -> types::ChainSpec {
    E::default_spec()
}

/// Build a genesis state and return it alongside the keypairs.
fn genesis_state_and_keypairs() -> (types::BeaconState<E>, Vec<Keypair>) {
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

fn get_store() -> crate::BeaconStore<T> {
    create_test_tracing_subscriber();
    Arc::new(HotColdDB::open_ephemeral(<_>::default(), Arc::new(test_spec())).unwrap())
}

/// Create a `ValidatorQueryService` populated with deterministic validators.
fn new_service() -> (ValidatorQueryService<T>, Vec<Keypair>) {
    let (state, keypairs) = genesis_state_and_keypairs();
    let store = get_store();
    let cache = ValidatorPubkeyCache::new(&state, store).expect("should create cache");
    (ValidatorQueryService::new(cache), keypairs)
}

// -----------------------------------------------------------------------
// validator_index tests
// -----------------------------------------------------------------------

#[test]
fn validator_query_index_found() {
    let (service, keypairs) = new_service();
    let pubkey_bytes: PublicKeyBytes = keypairs[0].pk.clone().into();

    let result = service
        .validator_index(&pubkey_bytes)
        .expect("should not error");
    assert_eq!(result, Some(0), "first validator should have index 0");
}

#[test]
fn validator_query_index_not_found() {
    let (service, _keypairs) = new_service();
    let unknown = PublicKeyBytes::empty();

    let result = service.validator_index(&unknown).expect("should not error");
    assert_eq!(result, None, "unknown pubkey should return None");
}

// -----------------------------------------------------------------------
// validator_indices tests
// -----------------------------------------------------------------------

#[test]
fn validator_query_indices_all_known() {
    let (service, keypairs) = new_service();
    let pubkey_bytes: Vec<PublicKeyBytes> = keypairs
        .iter()
        .take(3)
        .map(|kp| kp.pk.clone().into())
        .collect();

    let result = service
        .validator_indices(pubkey_bytes.iter())
        .expect("should resolve all indices");
    assert_eq!(result, vec![0, 1, 2]);
}

#[test]
fn validator_query_indices_unknown_pubkey_errors() {
    let (service, keypairs) = new_service();
    let mut pubkey_bytes: Vec<PublicKeyBytes> = keypairs
        .iter()
        .take(1)
        .map(|kp| kp.pk.clone().into())
        .collect();
    pubkey_bytes.push(PublicKeyBytes::empty());

    let result = service.validator_indices(pubkey_bytes.iter());
    assert!(
        result.is_err(),
        "should error when an unknown pubkey is included"
    );
}

// -----------------------------------------------------------------------
// validator_pubkey tests
// -----------------------------------------------------------------------

#[test]
fn validator_query_pubkey_found() {
    let (service, keypairs) = new_service();

    let result = service.validator_pubkey(0).expect("should not error");
    assert_eq!(
        result.as_ref(),
        Some(&keypairs[0].pk),
        "should return the correct pubkey"
    );
}

#[test]
fn validator_query_pubkey_out_of_bounds() {
    let (service, _keypairs) = new_service();

    let result = service
        .validator_pubkey(VALIDATOR_COUNT + 1)
        .expect("should not error");
    assert_eq!(result, None, "out of bounds index should return None");
}

// -----------------------------------------------------------------------
// validator_pubkey_bytes tests
// -----------------------------------------------------------------------

#[test]
fn validator_query_pubkey_bytes_found() {
    let (service, keypairs) = new_service();
    let expected: PublicKeyBytes = keypairs[2].pk.clone().into();

    let result = service.validator_pubkey_bytes(2).expect("should not error");
    assert_eq!(result, Some(expected));
}

#[test]
fn validator_query_pubkey_bytes_out_of_bounds() {
    let (service, _keypairs) = new_service();

    let result = service
        .validator_pubkey_bytes(VALIDATOR_COUNT + 1)
        .expect("should not error");
    assert_eq!(result, None);
}

// -----------------------------------------------------------------------
// validator_pubkey_bytes_many tests
// -----------------------------------------------------------------------

#[test]
fn validator_query_pubkey_bytes_many_all_found() {
    let (service, keypairs) = new_service();
    let indices = vec![0, 1, 2];

    let result = service
        .validator_pubkey_bytes_many(&indices)
        .expect("should not error");

    assert_eq!(result.len(), 3);
    for &i in &indices {
        let expected: PublicKeyBytes = keypairs[i].pk.clone().into();
        assert_eq!(result.get(&i), Some(&expected));
    }
}

#[test]
fn validator_query_pubkey_bytes_many_partial() {
    let (service, keypairs) = new_service();
    let indices = vec![0, VALIDATOR_COUNT + 1];

    let result = service
        .validator_pubkey_bytes_many(&indices)
        .expect("should not error");

    assert_eq!(result.len(), 1, "only one index should resolve");
    let expected: PublicKeyBytes = keypairs[0].pk.clone().into();
    assert_eq!(result.get(&0), Some(&expected));
}

#[test]
fn validator_query_pubkey_bytes_many_empty() {
    let (service, _keypairs) = new_service();

    let result = service
        .validator_pubkey_bytes_many(&[])
        .expect("should not error");
    assert!(result.is_empty());
}
