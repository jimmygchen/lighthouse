use super::{
    AggregateAndProof, Attestation, ChainSpec, Domain, EthSpec, Fork, Hash256, SecretKey,
    SelectionProof, Signature, SignedRoot,
};
use crate::test_utils::TestRandom;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use test_random_derive::TestRandom;
use tree_hash_derive::TreeHash;

/// A Validators signed aggregate proof to publish on the `beacon_aggregate_and_proof`
/// gossipsub topic.
///
/// Spec v0.12.1
#[derive(
    Debug,
    Clone,
    PartialEq,
    Serialize,
    Deserialize,
    Encode,
    Decode,
    TestRandom,
    TreeHash,
    arbitrary::Arbitrary,
)]
#[serde(bound = "E: EthSpec")]
#[arbitrary(bound = "E: EthSpec")]
pub struct SignedAggregateAndProof<E: EthSpec> {
    /// The `AggregateAndProof` that was signed.
    pub message: AggregateAndProof<E>,
    /// The aggregate attestation.
    pub signature: Signature,
}

impl<E: EthSpec> SignedAggregateAndProof<E> {
    /// Produces a new `SignedAggregateAndProof` with a `selection_proof` generated by signing
    /// `aggregate.data.slot` with `secret_key`.
    ///
    /// If `selection_proof.is_none()` it will be computed locally.
    pub fn from_aggregate(
        aggregator_index: u64,
        aggregate: Attestation<E>,
        selection_proof: Option<SelectionProof>,
        secret_key: &SecretKey,
        fork: &Fork,
        genesis_validators_root: Hash256,
        spec: &ChainSpec,
    ) -> Self {
        let message = AggregateAndProof::from_aggregate(
            aggregator_index,
            aggregate,
            selection_proof,
            secret_key,
            fork,
            genesis_validators_root,
            spec,
        );

        let target_epoch = message.aggregate.data.slot.epoch(E::slots_per_epoch());
        let domain = spec.get_domain(
            target_epoch,
            Domain::AggregateAndProof,
            fork,
            genesis_validators_root,
        );
        let signing_message = message.signing_root(domain);

        SignedAggregateAndProof {
            message,
            signature: secret_key.sign(signing_message),
        }
    }
}
