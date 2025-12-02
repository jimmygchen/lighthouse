use super::*;
use async_trait::async_trait;
use mockall::mock;

// Create a mockable trait using async_trait
#[async_trait]
pub trait MockableValidatorStoreTrait<E: EthSpec>: Send + Sync {
    fn validator_index(&self, pubkey: &PublicKeyBytes) -> Option<u64>;
    fn num_voting_validators(&self) -> usize;
    fn prune_slashing_protection_db(&self, current_epoch: Epoch, first_run: bool);

    async fn sign_attestation(
        &self,
        validator_pubkey: PublicKeyBytes,
        validator_committee_position: usize,
        attestation: &mut Attestation<E>,
        current_epoch: Epoch,
    ) -> Result<(), Error<String>>;
}

// Use mock! macro to create the mock
mock! {
    pub ValidatorStoreInner<E: EthSpec> {}

    #[async_trait]
    impl<E: EthSpec> MockableValidatorStoreTrait<E> for ValidatorStoreInner<E> {
        fn validator_index(&self, pubkey: &PublicKeyBytes) -> Option<u64>;
        fn num_voting_validators(&self) -> usize;
        fn prune_slashing_protection_db(&self, current_epoch: Epoch, first_run: bool);

        async fn sign_attestation(
            &self,
            validator_pubkey: PublicKeyBytes,
            validator_committee_position: usize,
            attestation: &mut Attestation<E>,
            current_epoch: Epoch,
        ) -> Result<(), Error<String>>;
    }
}

/// Mock implementation of ValidatorStore for testing
pub struct MockValidatorStore<E: EthSpec> {
    pub inner: MockValidatorStoreInner<E>,
}

impl<E: EthSpec> MockValidatorStore<E> {
    pub fn new() -> Self {
        Self {
            inner: MockValidatorStoreInner::new(),
        }
    }
}

// Implement ValidatorStore by delegating to the mock
impl<E: EthSpec> ValidatorStore for MockValidatorStore<E> {
    type Error = String;
    type E = E;

    fn validator_index(&self, pubkey: &PublicKeyBytes) -> Option<u64> {
        self.inner.validator_index(pubkey)
    }

    fn voting_pubkeys<I, F>(&self, _filter_func: F) -> I
    where
        I: FromIterator<PublicKeyBytes>,
        F: Fn(DoppelgangerStatus) -> Option<PublicKeyBytes>,
    {
        I::from_iter(std::iter::empty())
    }

    fn doppelganger_protection_allows_signing(&self, _validator_pubkey: PublicKeyBytes) -> bool {
        true
    }

    fn num_voting_validators(&self) -> usize {
        self.inner.num_voting_validators()
    }

    fn graffiti(&self, _validator_pubkey: &PublicKeyBytes) -> Option<Graffiti> {
        None
    }

    fn get_fee_recipient(&self, _validator_pubkey: &PublicKeyBytes) -> Option<Address> {
        None
    }

    fn determine_builder_boost_factor(&self, _validator_pubkey: &PublicKeyBytes) -> Option<u64> {
        None
    }

    fn set_validator_index(&self, _validator_pubkey: &PublicKeyBytes, _index: u64) {
        // No-op
    }

    fn prune_slashing_protection_db(&self, current_epoch: Epoch, first_run: bool) {
        self.inner.prune_slashing_protection_db(current_epoch, first_run)
    }

    fn proposal_data(&self, _pubkey: &PublicKeyBytes) -> Option<ProposalData> {
        None
    }

    async fn randao_reveal(
        &self,
        _validator_pubkey: PublicKeyBytes,
        _signing_epoch: Epoch,
    ) -> Result<Signature, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }

    async fn sign_block(
        &self,
        _validator_pubkey: PublicKeyBytes,
        _block: UnsignedBlock<Self::E>,
        _current_slot: Slot,
    ) -> Result<SignedBlock<Self::E>, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }

    async fn sign_attestation(
        &self,
        validator_pubkey: PublicKeyBytes,
        validator_committee_position: usize,
        attestation: &mut Attestation<Self::E>,
        current_epoch: Epoch,
    ) -> Result<(), Error<Self::Error>> {
        self.inner
            .sign_attestation(validator_pubkey, validator_committee_position, attestation, current_epoch)
            .await
    }

    async fn sign_validator_registration_data(
        &self,
        _validator_registration_data: ValidatorRegistrationData,
    ) -> Result<SignedValidatorRegistrationData, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }

    async fn produce_signed_aggregate_and_proof(
        &self,
        _validator_pubkey: PublicKeyBytes,
        _aggregator_index: u64,
        _aggregate: Attestation<Self::E>,
        _selection_proof: SelectionProof,
    ) -> Result<SignedAggregateAndProof<Self::E>, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }

    async fn produce_selection_proof(
        &self,
        _validator_pubkey: PublicKeyBytes,
        _slot: Slot,
    ) -> Result<SelectionProof, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }

    async fn produce_sync_selection_proof(
        &self,
        _validator_pubkey: &PublicKeyBytes,
        _slot: Slot,
        _subnet_id: SyncSubnetId,
    ) -> Result<SyncSelectionProof, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }

    async fn produce_sync_committee_signature(
        &self,
        _slot: Slot,
        _beacon_block_root: Hash256,
        _validator_index: u64,
        _validator_pubkey: &PublicKeyBytes,
    ) -> Result<SyncCommitteeMessage, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }

    async fn produce_signed_contribution_and_proof(
        &self,
        _aggregator_index: u64,
        _aggregator_pubkey: PublicKeyBytes,
        _contribution: SyncCommitteeContribution<Self::E>,
        _selection_proof: SyncSelectionProof,
    ) -> Result<SignedContributionAndProof<Self::E>, Error<Self::Error>> {
        Err(Error::UnknownPubkey(PublicKeyBytes::empty()))
    }
}

impl<E: EthSpec> Default for MockValidatorStore<E> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::MinimalEthSpec;

    type E = MinimalEthSpec;

    #[test]
    fn can_create_mock() {
        let mut mock = MockValidatorStore::<E>::new();

        // Set up expectation using mockall
        mock.inner
            .expect_num_voting_validators()
            .times(1)
            .returning(|| 42);

        assert_eq!(mock.num_voting_validators(), 42);
    }

    #[test]
    fn can_set_expectations_on_prune() {
        let mut mock = MockValidatorStore::<E>::new();

        mock.inner
            .expect_prune_slashing_protection_db()
            .times(1)
            .returning(|_, _| ());

        mock.prune_slashing_protection_db(Epoch::new(0), false);
    }
}
