use super::*;
use bls::{AggregateSignature, Keypair, PublicKeyBytes, Signature};
use fixed_bytes::FixedBytesExtended;
use genesis::{DEFAULT_ETH1_BLOCK_HASH, interop_genesis_state};
use operation_pool::ReceivedPreCapella;
use ssz_types::VariableList;
use types::*;

type E = MinimalEthSpec;

const VALIDATOR_COUNT: usize = 16;

/// Create a minimal spec suitable for testing operations at genesis.
///
/// Sets `shard_committee_period` to 0 so voluntary exits are valid at epoch 0.
fn test_spec() -> ChainSpec {
    let mut spec = E::default_spec();
    spec.shard_committee_period = 0;
    spec
}

/// Build a genesis `BeaconState` and return it alongside the keypairs.
fn genesis_state_and_keypairs() -> (BeaconState<E>, Vec<Keypair>) {
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

/// Create an `OperationsManager` backed by the default spec.
fn new_manager() -> OperationsManager<E> {
    let spec = Arc::new(test_spec());
    let op_pool = Arc::new(OperationPool::default());
    OperationsManager::new(spec, op_pool)
}

/// Create a signed voluntary exit for `validator_index` at `epoch`.
fn make_exit(
    keypairs: &[Keypair],
    genesis_validators_root: Hash256,
    validator_index: u64,
    epoch: Epoch,
    spec: &ChainSpec,
) -> SignedVoluntaryExit {
    let sk = &keypairs[validator_index as usize].sk;
    VoluntaryExit {
        epoch,
        validator_index,
    }
    .sign(sk, genesis_validators_root, spec)
}

/// Create a proposer slashing for `validator_index`.
fn make_proposer_slashing(
    keypairs: &[Keypair],
    state: &BeaconState<E>,
    validator_index: u64,
    spec: &ChainSpec,
) -> ProposerSlashing {
    let header_1 = BeaconBlockHeader {
        slot: Slot::new(0),
        proposer_index: validator_index,
        parent_root: Hash256::zero(),
        state_root: Hash256::random(),
        body_root: Hash256::random(),
    };

    let mut header_2 = header_1.clone();
    header_2.state_root = Hash256::zero();

    let fork = state.fork();
    let genesis_validators_root = state.genesis_validators_root();
    let sk = &keypairs[validator_index as usize].sk;

    let signed_header_1 = header_1.sign::<E>(sk, &fork, genesis_validators_root, spec);
    let signed_header_2 = header_2.sign::<E>(sk, &fork, genesis_validators_root, spec);

    ProposerSlashing {
        signed_header_1,
        signed_header_2,
    }
}

/// Create an attester slashing for the given `validator_indices`.
fn make_attester_slashing(
    keypairs: &[Keypair],
    state: &BeaconState<E>,
    validator_indices: Vec<u64>,
    spec: &ChainSpec,
) -> AttesterSlashing<E> {
    let fork = state.fork();
    let genesis_validators_root = state.genesis_validators_root();

    let data_1 = AttestationData {
        slot: Slot::new(0),
        index: 0,
        beacon_block_root: Hash256::zero(),
        target: Checkpoint {
            root: Hash256::zero(),
            epoch: fork.epoch,
        },
        source: Checkpoint {
            root: Hash256::zero(),
            epoch: Epoch::new(0),
        },
    };

    let mut data_2 = data_1.clone();
    data_2.index = 1;

    let mut attestation_1 = IndexedAttestation::Base(IndexedAttestationBase {
        attesting_indices: VariableList::new(validator_indices).expect("should create var list"),
        data: data_1,
        signature: AggregateSignature::infinity(),
    });

    let mut attestation_2 = IndexedAttestation::Base(IndexedAttestationBase {
        attesting_indices: attestation_1
            .attesting_indices_to_vec()
            .into_iter()
            .collect::<Vec<_>>()
            .try_into()
            .expect("should create var list"),
        data: data_2,
        signature: AggregateSignature::infinity(),
    });

    for attestation in [&mut attestation_1, &mut attestation_2] {
        if let IndexedAttestation::Base(att) = attestation {
            for &i in att.attesting_indices.iter() {
                let sk = &keypairs[i as usize].sk;
                let domain = spec.get_domain(
                    att.data.target.epoch,
                    Domain::BeaconAttester,
                    &fork,
                    genesis_validators_root,
                );
                let message = att.data.signing_root(domain);
                att.signature.add_assign(&sk.sign(message));
            }
        }
    }

    AttesterSlashing::Base(AttesterSlashingBase {
        attestation_1: attestation_1.as_base().unwrap().clone(),
        attestation_2: attestation_2.as_base().unwrap().clone(),
    })
}

// -----------------------------------------------------------------------
// Voluntary exit tests
// -----------------------------------------------------------------------

#[test]
fn valid_exit_accepted() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();
    let exit = make_exit(
        &keypairs,
        state.genesis_validators_root(),
        0,
        Epoch::new(0),
        &spec,
    );

    let outcome = manager
        .verify_voluntary_exit(exit, &state, Epoch::new(0))
        .expect("should verify");
    assert!(matches!(outcome, ObservationOutcome::New(_)));
}

#[test]
fn duplicate_exit_returns_already_known() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();
    let exit = make_exit(
        &keypairs,
        state.genesis_validators_root(),
        0,
        Epoch::new(0),
        &spec,
    );

    let outcome = manager
        .verify_voluntary_exit(exit.clone(), &state, Epoch::new(0))
        .expect("first verify should succeed");
    assert!(matches!(outcome, ObservationOutcome::New(_)));

    let outcome = manager
        .verify_voluntary_exit(exit, &state, Epoch::new(0))
        .expect("second verify should succeed");
    assert!(
        matches!(outcome, ObservationOutcome::AlreadyKnown),
        "duplicate exit should be AlreadyKnown"
    );
}

#[test]
fn different_exit_same_validator_returns_already_known() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();

    let exit_1 = make_exit(
        &keypairs,
        state.genesis_validators_root(),
        0,
        Epoch::new(0),
        &spec,
    );
    let exit_2 = make_exit(
        &keypairs,
        state.genesis_validators_root(),
        0,
        Epoch::new(1),
        &spec,
    );

    let outcome = manager
        .verify_voluntary_exit(exit_1, &state, Epoch::new(0))
        .expect("first exit should verify");
    assert!(matches!(outcome, ObservationOutcome::New(_)));

    let outcome = manager
        .verify_voluntary_exit(exit_2, &state, Epoch::new(1))
        .expect("second exit for same validator should succeed");
    assert!(matches!(outcome, ObservationOutcome::AlreadyKnown));
}

#[test]
fn exit_different_validator_accepted() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();

    let exit_1 = make_exit(
        &keypairs,
        state.genesis_validators_root(),
        0,
        Epoch::new(0),
        &spec,
    );
    let exit_2 = make_exit(
        &keypairs,
        state.genesis_validators_root(),
        1,
        Epoch::new(0),
        &spec,
    );

    let outcome = manager
        .verify_voluntary_exit(exit_1, &state, Epoch::new(0))
        .expect("first exit should verify");
    assert!(matches!(outcome, ObservationOutcome::New(_)));

    let outcome = manager
        .verify_voluntary_exit(exit_2, &state, Epoch::new(0))
        .expect("second exit for different validator should verify");
    assert!(matches!(outcome, ObservationOutcome::New(_)));
}

#[test]
fn invalid_exit_bad_signature_rejected() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();

    // Sign exit with a different validator's key.
    let wrong_sk = &keypairs[1].sk;
    let exit = VoluntaryExit {
        epoch: Epoch::new(0),
        validator_index: 0,
    }
    .sign(wrong_sk, state.genesis_validators_root(), &spec);

    let result = manager.verify_voluntary_exit(exit, &state, Epoch::new(0));
    assert!(result.is_err(), "bad-signature exit should be rejected");
}

#[test]
fn import_exit_adds_to_op_pool() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();

    let exit = make_exit(
        &keypairs,
        state.genesis_validators_root(),
        0,
        Epoch::new(0),
        &spec,
    );

    let outcome = manager
        .verify_voluntary_exit(exit, &state, Epoch::new(0))
        .expect("should verify");

    if let ObservationOutcome::New(verified) = outcome {
        manager.import_voluntary_exit(verified);
    } else {
        panic!("exit should be new");
    }

    let exits = manager.op_pool.get_all_voluntary_exits();
    assert_eq!(exits.len(), 1, "op pool should contain the imported exit");
}

// -----------------------------------------------------------------------
// Proposer slashing tests
// -----------------------------------------------------------------------

#[test]
fn proposer_slashing_verify_and_import() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();

    let slashing = make_proposer_slashing(&keypairs, &state, 0, &spec);

    let outcome = manager
        .verify_proposer_slashing(slashing.clone(), &state)
        .expect("should verify");
    assert!(matches!(outcome, ObservationOutcome::New(_)));

    // Duplicate.
    let outcome = manager
        .verify_proposer_slashing(slashing.clone(), &state)
        .expect("duplicate should succeed");
    assert!(matches!(outcome, ObservationOutcome::AlreadyKnown));

    // Import the verified slashing.
    let verified = match manager
        .verify_proposer_slashing(make_proposer_slashing(&keypairs, &state, 1, &spec), &state)
        .expect("should verify different validator")
    {
        ObservationOutcome::New(v) => v,
        _ => panic!("should be new"),
    };
    let returned = manager.import_proposer_slashing(verified);
    assert_eq!(
        returned.signed_header_1.message.proposer_index, 1,
        "import should return the slashing"
    );

    let all = manager.op_pool.get_all_proposer_slashings();
    assert_eq!(all.len(), 1, "op pool should contain one proposer slashing");
}

// -----------------------------------------------------------------------
// Attester slashing tests
// -----------------------------------------------------------------------

#[test]
fn attester_slashing_verify_and_import() {
    let manager = new_manager();
    let (state, keypairs) = genesis_state_and_keypairs();
    let spec = test_spec();

    let slashing = make_attester_slashing(&keypairs, &state, vec![0, 1], &spec);

    let outcome = manager
        .verify_attester_slashing(slashing.clone(), &state)
        .expect("should verify");
    assert!(matches!(outcome, ObservationOutcome::New(_)));

    // Duplicate.
    let outcome = manager
        .verify_attester_slashing(slashing.clone(), &state)
        .expect("duplicate should succeed");
    assert!(matches!(outcome, ObservationOutcome::AlreadyKnown));

    // Import.
    let slashing_2 = make_attester_slashing(&keypairs, &state, vec![2, 3], &spec);
    let verified = match manager
        .verify_attester_slashing(slashing_2, &state)
        .expect("should verify different indices")
    {
        ObservationOutcome::New(v) => v,
        _ => panic!("should be new"),
    };
    manager.import_attester_slashing(verified);

    let all = manager.op_pool.get_all_attester_slashings();
    assert_eq!(all.len(), 1, "op pool should contain one attester slashing");
}

// -----------------------------------------------------------------------
// BLS-to-execution change tests
// -----------------------------------------------------------------------

#[test]
fn bls_to_execution_change_pre_capella_rejected() {
    let manager = new_manager();
    let (state, _keypairs) = genesis_state_and_keypairs();

    let change = SignedBlsToExecutionChange {
        message: BlsToExecutionChange {
            validator_index: 0,
            from_bls_pubkey: PublicKeyBytes::empty(),
            to_execution_address: Address::zero(),
        },
        signature: Signature::empty(),
    };

    let result = manager.verify_bls_to_execution_change_for_gossip(
        change, &state, false, // is_post_capella = false
    );
    assert!(
        matches!(result, Err(Error::BlsToExecutionPriorToCapella)),
        "pre-capella gossip should be rejected"
    );
}

#[test]
fn import_bls_to_execution_change_returns_inserted() {
    // This test verifies the import path through the op pool.
    // We skip full signature verification and construct a SigVerifiedOp directly
    // by verifying against a state first.
    let spec = Arc::new(test_spec());
    let op_pool = Arc::new(OperationPool::<E>::default());
    let manager = OperationsManager::new(spec, op_pool);

    // A manually crafted (but invalid) change should still pass through import
    // since import takes already-verified operations. We test that the pool
    // interaction works correctly by verifying the return value.
    //
    // The op_pool's insert_bls_to_execution_change returns true on first
    // insert and false on subsequent inserts for the same validator. We can
    // observe this through our manager.
    let (state, keypairs) = genesis_state_and_keypairs();
    let test_spec = test_spec();

    // Create a signed BLS change. The default genesis spec uses BLS withdrawal
    // credentials for interop, so validation should pass.
    let change = BlsToExecutionChange {
        validator_index: 0,
        from_bls_pubkey: keypairs[0].pk.compress(),
        to_execution_address: Address::repeat_byte(0x42),
    };

    let genesis_validators_root = state.genesis_validators_root();
    let domain = test_spec.compute_domain(
        Domain::BlsToExecutionChange,
        test_spec.genesis_fork_version,
        genesis_validators_root,
    );
    let message = change.signing_root(domain);
    let signature = keypairs[0].sk.sign(message);

    let signed_change = SignedBlsToExecutionChange {
        message: change,
        signature,
    };

    // Verify through the manager.
    let outcome = manager
        .verify_bls_to_execution_change(signed_change, &state)
        .expect("should verify BLS change");

    if let ObservationOutcome::New(verified) = outcome {
        let inserted = manager.import_bls_to_execution_change(verified, ReceivedPreCapella::No);
        assert!(inserted, "first insert should succeed");
    } else {
        panic!("should be new");
    }
}
