#[cfg(test)]
mod tests;

use crate::BeaconChainTypes;
use crate::beacon_proposer_cache::{BeaconProposerCache, EpochBlockProposers};
use crate::canonical_head::CanonicalHead;
use crate::errors::BeaconChainError;
use execution_layer::ExecutionLayer;
use parking_lot::Mutex;
use std::sync::Arc;
use types::{
    AbstractExecPayload, BeaconState, BeaconStateError, ChainSpec, Epoch, EthSpec, Hash256,
    SignedBeaconBlock, Slot,
};

/// Manages execution layer integration and proposer cache access.
///
/// Generic over `T: BeaconChainTypes` because `ExecutionLayer` is generic over
/// `E: EthSpec` which comes from `T`.
///
/// State is passed as method parameters where possible. The component never
/// fetches head state or slot clock values on its own.
pub struct ExecutionManager<T: BeaconChainTypes> {
    spec: Arc<ChainSpec>,
    execution_layer: Option<ExecutionLayer<T::EthSpec>>,
    beacon_proposer_cache: Arc<Mutex<BeaconProposerCache>>,
}

impl<T: BeaconChainTypes> ExecutionManager<T> {
    /// Create a new `ExecutionManager`.
    pub fn new(
        spec: Arc<ChainSpec>,
        execution_layer: Option<ExecutionLayer<T::EthSpec>>,
        beacon_proposer_cache: Arc<Mutex<BeaconProposerCache>>,
    ) -> Self {
        Self {
            spec,
            execution_layer,
            beacon_proposer_cache,
        }
    }

    /// Return a reference to the execution layer, if configured.
    pub fn execution_layer(&self) -> Option<&ExecutionLayer<T::EthSpec>> {
        self.execution_layer.as_ref()
    }

    /// Return a reference to the beacon proposer cache.
    pub fn beacon_proposer_cache(&self) -> &Arc<Mutex<BeaconProposerCache>> {
        &self.beacon_proposer_cache
    }

    /// Return a reference to the chain spec.
    pub fn spec(&self) -> &Arc<ChainSpec> {
        &self.spec
    }

    /// Returns `true` if the given slot is prior to the `bellatrix_fork_epoch`.
    pub fn slot_is_prior_to_bellatrix(&self, slot: Slot) -> bool {
        self.spec
            .bellatrix_fork_epoch
            .is_none_or(|bellatrix| slot.epoch(T::EthSpec::slots_per_epoch()) < bellatrix)
    }

    /// Returns `Ok(true)` if the block has `ExecutionStatus::Optimistic` or `Invalid`.
    /// Returns `Ok(false)` if the block is pre-Bellatrix or has `ExecutionStatus::Valid`.
    pub fn is_optimistic_or_invalid_block<Payload: AbstractExecPayload<T::EthSpec>>(
        &self,
        canonical_head: &CanonicalHead<T>,
        block: &SignedBeaconBlock<T::EthSpec, Payload>,
    ) -> Result<bool, BeaconChainError> {
        if self.slot_is_prior_to_bellatrix(block.slot()) {
            Ok(false)
        } else {
            canonical_head
                .fork_choice_read_lock()
                .is_optimistic_or_invalid_block(&block.canonical_root())
                .map_err(BeaconChainError::ForkChoiceError)
        }
    }

    /// Like `is_optimistic_or_invalid_block` but uses `no_fallback` variant.
    /// Should only be used on the head block or when the block is expected in fork choice.
    pub fn is_optimistic_or_invalid_head_block<Payload: AbstractExecPayload<T::EthSpec>>(
        &self,
        canonical_head: &CanonicalHead<T>,
        head_block: &SignedBeaconBlock<T::EthSpec, Payload>,
    ) -> Result<bool, BeaconChainError> {
        if self.slot_is_prior_to_bellatrix(head_block.slot()) {
            Ok(false)
        } else {
            canonical_head
                .fork_choice_read_lock()
                .is_optimistic_or_invalid_block_no_fallback(&head_block.canonical_root())
                .map_err(BeaconChainError::ForkChoiceError)
        }
    }

    /// Returns the value of `execution_optimistic` for the current head block.
    pub fn is_optimistic_or_invalid_head(
        &self,
        canonical_head: &CanonicalHead<T>,
    ) -> Result<bool, BeaconChainError> {
        canonical_head
            .head_execution_status()
            .map(|status| status.is_optimistic_or_invalid())
    }

    /// Provides safe and efficient multi-threaded access to the beacon proposer
    /// cache.
    ///
    /// - `shuffling_decision_block`: The block root of the decision block for
    ///   the desired proposer shuffling.
    /// - `proposal_epoch`: The epoch at which the proposer shuffling is
    ///   required.
    /// - `accessor`: A closure to run against the proposers for the selected
    ///   epoch.
    /// - `state_provider`: A closure to compute a state suitable for
    ///   determining the shuffling. Evaluated lazily only on cache miss.
    pub fn with_proposer_cache<V, E: From<BeaconChainError> + From<BeaconStateError>>(
        &self,
        shuffling_decision_block: Hash256,
        proposal_epoch: Epoch,
        accessor: impl Fn(&EpochBlockProposers) -> Result<V, BeaconChainError>,
        state_provider: impl FnOnce() -> Result<(Hash256, BeaconState<T::EthSpec>), E>,
    ) -> Result<V, E> {
        crate::beacon_proposer_cache::with_proposer_cache(
            &self.beacon_proposer_cache,
            shuffling_decision_block,
            proposal_epoch,
            accessor,
            state_provider,
            &self.spec,
        )
    }
}
