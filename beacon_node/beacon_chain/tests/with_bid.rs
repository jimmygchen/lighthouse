#![cfg(not(debug_assertions))]
#![allow(clippy::result_large_err)]

//! Harness tests for `produce_block_with_verification_gloas_with_bid` (beacon-APIs #627
//! `with_bid`): a proposer-supplied bid should win selection when it verifies, and fall back to
//! the local build when it doesn't (wrong parent hash, bad signature).

use beacon_chain::ProduceBlockVerification;
use beacon_chain::graffiti_calculator::GraffitiSettings;
use beacon_chain::proposer_preferences_verification::gossip_verified_proposer_preferences::GossipVerifiedProposerPreferences;
use beacon_chain::test_utils::{
    BeaconChainHarness, EphemeralHarnessType, HARNESS_GENESIS_TIME, test_spec,
};
use bls::{Keypair, PublicKeyBytes, Signature};
use execution_layer::test_utils::generate_genesis_header;
use genesis::{DEFAULT_ETH1_BLOCK_HASH, interop_genesis_state};
use ssz::Encode;
use state_processing::state_advance::complete_state_advance;
use std::sync::Arc;
use types::consts::gloas::{BUILDER_INDEX_SELF_BUILD, PAYLOAD_BUILDER_VERSION};
use types::*;

type E = MinimalEthSpec;
type TestHarness = BeaconChainHarness<EphemeralHarnessType<E>>;

const NUM_VALIDATORS: usize = 64;
/// Ample headroom over `min_deposit_amount` plus the bid value used below.
const BUILDER_BALANCE: u64 = 10_000_000_000;
/// Comfortably above `DEFAULT_MOCK_EL_PAYLOAD_VALUE_WEI` (1e16 wei) once converted to wei.
const SUPPLIED_BID_VALUE_GWEI: u64 = 1_000_000_000;
const SUPPLIED_BID_FEE_RECIPIENT: Address = Address::repeat_byte(0x55);

fn builder_withdrawal_credentials(pubkey: &bls::PublicKey, spec: &ChainSpec) -> Hash256 {
    let fake_execution_address = &ethereum_hashing::hash(&pubkey.as_ssz_bytes())[0..20];
    let mut credentials = [0u8; 32];
    credentials[0] = spec.builder_withdrawal_prefix_byte;
    credentials[12..].copy_from_slice(fake_execution_address);
    Hash256::from_slice(&credentials)
}

/// Builds a harness with one registered, funded builder (the validator set's keypairs are reused
/// for simplicity — the `builder_index`, not the pubkey's role elsewhere, is what addresses it).
/// Returns the harness, the builder's keypair, and its registry index.
fn get_harness() -> (TestHarness, Keypair, u64) {
    let spec = Arc::new(test_spec::<E>());
    let validator_keypairs = types::test_utils::generate_deterministic_keypairs(NUM_VALIDATORS);
    let builder_keypair = validator_keypairs[0].clone();

    let header = generate_genesis_header::<E>(&spec);
    let mut genesis_state = interop_genesis_state(
        &validator_keypairs,
        HARNESS_GENESIS_TIME,
        Hash256::from_slice(DEFAULT_ETH1_BLOCK_HASH),
        header,
        &spec,
    )
    .expect("should build genesis state");

    let creds = builder_withdrawal_credentials(&builder_keypair.pk, &spec);
    let builder_index = genesis_state
        .add_builder_to_registry(
            PublicKeyBytes::from(builder_keypair.pk.clone()),
            PAYLOAD_BUILDER_VERSION,
            creds,
            BUILDER_BALANCE,
            Slot::new(0),
            &spec,
        )
        .expect("should register builder");

    let harness = TestHarness::builder(E::default())
        .spec(spec)
        .keypairs(validator_keypairs)
        .genesis_state_ephemeral_store(genesis_state)
        .mock_execution_layer()
        .build();

    harness.advance_slot();

    (harness, builder_keypair, builder_index)
}

/// Registers proposer preferences for `proposal_slot` in the gossip-verified cache, matching the
/// harness's current head (as `insert_proposer_preferences` does for the plain v4 path in
/// `prepare_payload.rs`).
fn insert_proposer_preferences(
    harness: &TestHarness,
    proposal_slot: Slot,
    validator_index: u64,
    fee_recipient: Address,
    target_gas_limit: u64,
) {
    let dependent_root = harness
        .chain
        .head_snapshot()
        .beacon_state
        .proposer_shuffling_decision_root_at_epoch(
            proposal_slot.epoch(E::slots_per_epoch()),
            harness.head_block_root(),
            &harness.chain.spec,
        )
        .unwrap();
    harness
        .chain
        .gossip_verified_proposer_preferences_cache
        .insert_preferences(GossipVerifiedProposerPreferences {
            signed_preferences: Arc::new(SignedProposerPreferences {
                message: ProposerPreferences {
                    dependent_root,
                    proposal_slot,
                    validator_index,
                    fee_recipient,
                    target_gas_limit,
                },
                signature: Signature::empty(),
            }),
        });
}

/// State advanced to `slot`, with caches built, from the harness's current head.
fn advanced_state(harness: &TestHarness, slot: Slot) -> BeaconState<E> {
    let mut state = harness.get_current_state();
    complete_state_advance(&mut state, None, slot, None, &harness.chain.spec)
        .expect("should advance state to slot");
    state
        .build_caches(&harness.chain.spec)
        .expect("should build caches");
    state
}

/// A signed bid from `builder_index`, matching the harness's current head (the FULL parent
/// payload hash/root/randao/gas-limit) except for the overrides supplied by the caller.
#[allow(clippy::too_many_arguments)]
fn make_signed_bid(
    harness: &TestHarness,
    state: &BeaconState<E>,
    slot: Slot,
    builder_keypair: &Keypair,
    builder_index: u64,
    parent_block_hash: ExecutionBlockHash,
    sign: bool,
) -> Arc<SignedExecutionPayloadBid<E>> {
    let parent_bid = state
        .latest_execution_payload_bid()
        .expect("state should have a Gloas payload bid");
    let message = ExecutionPayloadBidGloas {
        parent_block_hash,
        parent_block_root: harness.head_block_root(),
        block_hash: ExecutionBlockHash::repeat_byte(0x99),
        prev_randao: *state
            .get_randao_mix(slot.epoch(E::slots_per_epoch()))
            .expect("should read current epoch randao mix"),
        fee_recipient: SUPPLIED_BID_FEE_RECIPIENT,
        gas_limit: parent_bid.gas_limit(),
        builder_index,
        slot,
        value: SUPPLIED_BID_VALUE_GWEI,
        execution_payment: 0,
        ..Default::default()
    };

    let signature = if sign {
        let domain = harness.chain.spec.get_domain(
            slot.epoch(E::slots_per_epoch()),
            Domain::BeaconBuilder,
            &state.fork(),
            state.genesis_validators_root(),
        );
        let signing_root = ExecutionPayloadBidRef::Gloas(&message).signing_root(domain);
        builder_keypair.sk.sign(signing_root)
    } else {
        Signature::empty()
    };

    Arc::new(SignedExecutionPayloadBid::Gloas(
        SignedExecutionPayloadBidGloas { message, signature },
    ))
}

fn randao_reveal(harness: &TestHarness, state: &BeaconState<E>, slot: Slot) -> Signature {
    let proposer_index = state
        .get_beacon_proposer_index(slot, &harness.chain.spec)
        .unwrap();
    harness.sign_randao_reveal(state, proposer_index, slot)
}

/// A valid, correctly-signed supplied bid wins selection over the local build.
#[tokio::test]
async fn valid_supplied_bid_is_used() {
    let (harness, builder_keypair, builder_index) = get_harness();
    if !harness.chain.spec.is_gloas_scheduled() {
        return;
    }

    // Finalize at least one epoch so the registered builder (deposit_epoch 0) becomes active
    // (`is_active_builder` requires `deposit_epoch < finalized_checkpoint.epoch`).
    let epochs_to_finalize = 6;
    harness
        .extend_slots(E::slots_per_epoch() as usize * epochs_to_finalize)
        .await;
    assert!(
        harness
            .chain
            .head_snapshot()
            .beacon_state
            .finalized_checkpoint()
            .epoch
            > Epoch::new(0),
        "chain should have finalized at least one epoch"
    );
    harness.advance_slot();

    let proposal_slot = harness.chain.slot().unwrap();
    let state = advanced_state(&harness, proposal_slot);
    let proposer_index = state
        .get_beacon_proposer_index(proposal_slot, &harness.chain.spec)
        .unwrap() as u64;
    let parent_bid = state
        .latest_execution_payload_bid()
        .expect("state should have a Gloas payload bid");
    let parent_block_hash = parent_bid.block_hash();
    let gas_limit = parent_bid.gas_limit();

    insert_proposer_preferences(
        &harness,
        proposal_slot,
        proposer_index,
        SUPPLIED_BID_FEE_RECIPIENT,
        gas_limit,
    );

    let signed_bid = make_signed_bid(
        &harness,
        &state,
        proposal_slot,
        &builder_keypair,
        builder_index,
        parent_block_hash,
        true,
    );
    let reveal = randao_reveal(&harness, &state, proposal_slot);

    let (
        block,
        _post_state,
        _consensus_block_value,
        _execution_payload_value,
        _payload_contents,
        _builder_url,
    ) = harness
        .chain
        .produce_block_with_verification_gloas_with_bid(
            reveal,
            proposal_slot,
            GraffitiSettings::Unspecified,
            ProduceBlockVerification::VerifyRandao,
            signed_bid,
            100,
        )
        .await
        .expect("block production with a valid supplied bid should succeed");

    let included_bid = block
        .body()
        .signed_execution_payload_bid()
        .expect("Gloas block should carry a bid");
    assert_eq!(
        included_bid.message().builder_index(),
        builder_index,
        "the block should commit to the supplied bid's builder, not the local self-build"
    );
}

/// A supplied bid with the wrong parent block hash fails verification and the production falls
/// back to the local build.
#[tokio::test]
async fn supplied_bid_wrong_parent_hash_falls_back_to_local() {
    let (harness, builder_keypair, builder_index) = get_harness();
    if !harness.chain.spec.is_gloas_scheduled() {
        return;
    }

    let proposal_slot = Slot::new(1);
    let state = advanced_state(&harness, proposal_slot);

    insert_proposer_preferences(
        &harness,
        proposal_slot,
        state
            .get_beacon_proposer_index(proposal_slot, &harness.chain.spec)
            .unwrap() as u64,
        SUPPLIED_BID_FEE_RECIPIENT,
        state.latest_execution_payload_bid().unwrap().gas_limit(),
    );

    // Wrong parent hash: doesn't match the FULL parent the producer actually selected.
    let wrong_parent_hash = ExecutionBlockHash::repeat_byte(0xee);
    let signed_bid = make_signed_bid(
        &harness,
        &state,
        proposal_slot,
        &builder_keypair,
        builder_index,
        wrong_parent_hash,
        true,
    );
    let reveal = randao_reveal(&harness, &state, proposal_slot);

    let (block, ..) = harness
        .chain
        .produce_block_with_verification_gloas_with_bid(
            reveal,
            proposal_slot,
            GraffitiSettings::Unspecified,
            ProduceBlockVerification::VerifyRandao,
            signed_bid,
            100,
        )
        .await
        .expect("block production should fall back to the local build and still succeed");

    let included_bid = block
        .body()
        .signed_execution_payload_bid()
        .expect("Gloas block should carry a bid");
    assert_eq!(
        included_bid.message().builder_index(),
        BUILDER_INDEX_SELF_BUILD,
        "an invalid supplied bid (wrong parent hash) should fall back to the local self-build"
    );
}

/// A supplied bid with a bad signature fails verification and the production falls back to the
/// local build.
#[tokio::test]
async fn supplied_bid_bad_signature_falls_back_to_local() {
    let (harness, builder_keypair, builder_index) = get_harness();
    if !harness.chain.spec.is_gloas_scheduled() {
        return;
    }

    // Finalize at least one epoch, exactly like `valid_supplied_bid_is_used`, so the registered
    // builder (deposit_epoch 0) is active (`is_active_builder` requires
    // `deposit_epoch < finalized_checkpoint.epoch`). Without this, `verify_direct_bid_consistency`
    // rejects the bid for an inactive builder *before* it ever reaches BLS verification, so the
    // bad signature below would never actually be exercised.
    let epochs_to_finalize = 6;
    harness
        .extend_slots(E::slots_per_epoch() as usize * epochs_to_finalize)
        .await;
    assert!(
        harness
            .chain
            .head_snapshot()
            .beacon_state
            .finalized_checkpoint()
            .epoch
            > Epoch::new(0),
        "chain should have finalized at least one epoch"
    );
    harness.advance_slot();

    let proposal_slot = harness.chain.slot().unwrap();
    let state = advanced_state(&harness, proposal_slot);
    let parent_bid = state.latest_execution_payload_bid().unwrap();

    insert_proposer_preferences(
        &harness,
        proposal_slot,
        state
            .get_beacon_proposer_index(proposal_slot, &harness.chain.spec)
            .unwrap() as u64,
        SUPPLIED_BID_FEE_RECIPIENT,
        parent_bid.gas_limit(),
    );

    // `sign: false` leaves the signature empty, which fails BLS verification.
    let signed_bid = make_signed_bid(
        &harness,
        &state,
        proposal_slot,
        &builder_keypair,
        builder_index,
        parent_bid.block_hash(),
        false,
    );
    let reveal = randao_reveal(&harness, &state, proposal_slot);

    let invalid_supplied_bids_before = beacon_chain::metrics::get_int_counter(
        &beacon_chain::metrics::BLOCK_PRODUCTION_SUPPLIED_BID_TOTAL,
        &["invalid"],
    )
    .map(|c| c.get())
    .unwrap_or(0);

    let (block, ..) = harness
        .chain
        .produce_block_with_verification_gloas_with_bid(
            reveal,
            proposal_slot,
            GraffitiSettings::Unspecified,
            ProduceBlockVerification::VerifyRandao,
            signed_bid,
            100,
        )
        .await
        .expect("block production should fall back to the local build and still succeed");

    let included_bid = block
        .body()
        .signed_execution_payload_bid()
        .expect("Gloas block should carry a bid");
    assert_eq!(
        included_bid.message().builder_index(),
        BUILDER_INDEX_SELF_BUILD,
        "an invalid supplied bid (bad signature) should fall back to the local self-build"
    );

    // Now that the builder is active, the only remaining defect is the bad signature, so this
    // specifically confirms BLS verification was reached and rejected it (rather than the
    // fallback happening to occur for an unrelated reason, as it would have before finalizing).
    let invalid_supplied_bids_after = beacon_chain::metrics::get_int_counter(
        &beacon_chain::metrics::BLOCK_PRODUCTION_SUPPLIED_BID_TOTAL,
        &["invalid"],
    )
    .map(|c| c.get())
    .unwrap_or(0);
    assert_eq!(
        invalid_supplied_bids_after,
        invalid_supplied_bids_before + 1,
        "the bad signature must be recorded as an invalid supplied bid"
    );
}
