//! Manages attestation pools, observation tracking, and shuffling caches.
//!
//! `AttestationManager<E>` owns the naive aggregation pool, observed attestation/attester/aggregator
//! tracking, early attester cache, and shuffling cache. It provides side-effect-free methods that
//! operate on these owned data structures.
//!
//! ## What stays on `BeaconChain`
//!
//! The following attestation-related methods remain on `BeaconChain` because they depend on
//! components the manager intentionally does not hold:
//!
//! - **Verification methods** (`verify_unaggregated_attestation_for_gossip`, etc.): The
//!   `attestation_verification` module takes `&BeaconChain<T>` directly and accesses fields like
//!   `canonical_head`, `observed_attestations`, `observed_aggregators`, and `with_committee_cache`.
//!   Refactoring that module is out of scope for this extraction.
//!
//! - **`produce_unaggregated_attestation`**: Needs `head_snapshot`, `store`, and `canonical_head`
//!   (fork choice) for optimistic block filtering.
//!
//! - **`validator_attestation_duties`**: Needs `canonical_head` (fork choice read lock) and
//!   `with_committee_cache` which itself needs `canonical_head`, `with_head`, and `store`.
//!
//! - **`apply_attestation_to_fork_choice`**: Needs `fork_choice_write_lock`.
//!
//! - **`with_committee_cache`**: Needs `canonical_head`, `with_head`, and `store` to load states
//!   and build committee caches on cache miss.
//!
//! - **`get_aggregated_attestation`** and variants: Need `filter_optimistic_attestation` which
//!   reads `canonical_head.fork_choice_read_lock()` to check execution status.
//!
//! - **`filter_op_pool_attestation`**: Delegates to `shuffling_is_compatible` which needs
//!   `canonical_head.fork_choice_read_lock()`.
//!
//! - **`shuffling_is_compatible`**: Needs `canonical_head.fork_choice_read_lock()` to load block
//!   shuffling IDs from fork choice. The `BeaconChain` wrapper obtains the block from fork choice
//!   and then delegates the pure shuffling comparison to this manager.
//!
//! - **`add_to_block_inclusion_pool`**: Directly accesses `op_pool` which is not owned by the
//!   manager (wrapping it in `Arc` would be too invasive).

#[cfg(test)]
mod tests;

use crate::early_attester_cache::EarlyAttesterCache;
use crate::errors::BeaconChainError as Error;
use crate::naive_aggregation_pool::{
    AggregatedAttestationMap, Error as NaiveAggregationError, NaiveAggregationPool,
};
use crate::observed_aggregates::ObservedAggregateAttestations;
use crate::observed_attesters::{ObservedAggregators, ObservedAttesters};
use crate::shuffling_cache::ShufflingCache;
use crate::{BeaconChainError, metrics};
use fork_choice;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{error, trace, warn};
use tree_hash::TreeHash;
use types::*;

/// Manages attestation-related pools, observation tracking, and the shuffling cache.
///
/// Generic over `E: EthSpec` rather than `T: BeaconChainTypes` to keep it decoupled
/// from the full beacon chain infrastructure.
pub struct AttestationManager<E: EthSpec> {
    /// The chain specification.
    pub(crate) spec: Arc<ChainSpec>,
    /// The root of the genesis block (used for shuffling compatibility checks).
    pub(crate) genesis_block_root: Hash256,
    /// Pool of unaggregated attestations for naive aggregation.
    pub naive_aggregation_pool: RwLock<NaiveAggregationPool<AggregatedAttestationMap<E>>>,
    /// Tracks aggregate attestations that have been observed.
    pub(crate) observed_attestations: RwLock<ObservedAggregateAttestations<E>>,
    /// Tracks validators that have sent gossip attestations.
    pub(crate) observed_gossip_attesters: RwLock<ObservedAttesters<E>>,
    /// Tracks validators whose attestations have been included in blocks.
    pub(crate) observed_block_attesters: RwLock<ObservedAttesters<E>>,
    /// Tracks aggregators that have produced signed aggregates.
    pub(crate) observed_aggregators: RwLock<ObservedAggregators<E>>,
    /// Cache for producing attestations when the head block is still being imported.
    pub early_attester_cache: EarlyAttesterCache<E>,
    /// Cache of committee shufflings keyed by shuffling ID.
    pub shuffling_cache: RwLock<ShufflingCache>,
}

impl<E: EthSpec> AttestationManager<E> {
    /// Create a new `AttestationManager`.
    pub fn new(
        spec: Arc<ChainSpec>,
        genesis_block_root: Hash256,
        shuffling_cache: ShufflingCache,
    ) -> Self {
        Self {
            spec,
            genesis_block_root,
            naive_aggregation_pool: <_>::default(),
            observed_attestations: <_>::default(),
            observed_gossip_attesters: <_>::default(),
            observed_block_attesters: <_>::default(),
            observed_aggregators: <_>::default(),
            early_attester_cache: <_>::default(),
            shuffling_cache: RwLock::new(shuffling_cache),
        }
    }

    /// Insert an unaggregated attestation into the naive aggregation pool.
    ///
    /// If the attestation is too old (low slot) to be included in the pool it is simply dropped
    /// and no error is returned.
    pub fn add_to_naive_aggregation_pool(
        &self,
        attestation: AttestationRef<'_, E>,
    ) -> Result<(), Error> {
        let _timer = metrics::start_timer(&metrics::ATTESTATION_PROCESSING_APPLY_TO_AGG_POOL);

        match self.naive_aggregation_pool.write().insert(attestation) {
            Ok(outcome) => trace!(
                ?outcome,
                index = attestation.committee_index(),
                slot = attestation.data().slot.as_u64(),
                "Stored unaggregated attestation"
            ),
            Err(NaiveAggregationError::SlotTooLow {
                slot,
                lowest_permissible_slot,
            }) => {
                trace!(
                    lowest_permissible_slot = lowest_permissible_slot.as_u64(),
                    slot = slot.as_u64(),
                    "Refused to store unaggregated attestation"
                );
            }
            Err(e) => {
                error!(
                    error = ?e,
                    index = attestation.committee_index(),
                    slot = attestation.data().slot.as_u64(),
                    "Failed to store unaggregated attestation"
                );
                return Err(e.into());
            }
        };

        Ok(())
    }

    /// Update the shuffling cache with committee caches from a newly imported block's state.
    pub fn import_block_update_shuffling_cache(
        &self,
        block_root: Hash256,
        state: &mut BeaconState<E>,
    ) {
        if let Err(e) = self.import_block_update_shuffling_cache_fallible(block_root, state) {
            warn!(
                error = ?e,
                "Failed to prime shuffling cache"
            );
        }
    }

    /// Fallible version of shuffling cache update during block import.
    fn import_block_update_shuffling_cache_fallible(
        &self,
        block_root: Hash256,
        state: &mut BeaconState<E>,
    ) -> Result<(), BeaconChainError> {
        for relative_epoch in [RelativeEpoch::Current, RelativeEpoch::Next] {
            let shuffling_id = AttestationShufflingId::new(block_root, state, relative_epoch)?;

            let shuffling_is_cached = self.shuffling_cache.read().contains(&shuffling_id);

            if !shuffling_is_cached {
                state.build_committee_cache(relative_epoch, &self.spec)?;
                let committee_cache = state.committee_cache(relative_epoch)?;
                self.shuffling_cache
                    .write()
                    .insert_committee_cache(shuffling_id, committee_cache);
            }
        }
        Ok(())
    }

    /// Returns an aggregated `Attestation`, if any, that has a matching `attestation.data`.
    ///
    /// Uses `execution_status_fn` to filter out attestations that reference optimistic/invalid
    /// blocks. The caller provides this function from fork choice.
    pub fn get_aggregated_attestation(
        &self,
        attestation: AttestationRef<'_, E>,
        execution_status_fn: impl Fn(&Hash256) -> Option<fork_choice::ExecutionStatus>,
    ) -> Result<Option<Attestation<E>>, Error> {
        match attestation {
            AttestationRef::Base(att) => {
                let key = crate::naive_aggregation_pool::AttestationKey::new_base(&att.data);
                self.get_from_pool_filtered(&key, &execution_status_fn)
            }
            AttestationRef::Electra(att) => {
                let committee_index = att
                    .committee_index()
                    .ok_or(Error::AttestationCommitteeIndexNotSet)?;
                let key = crate::naive_aggregation_pool::AttestationKey::new_electra(
                    att.data.slot,
                    att.data.tree_hash_root(),
                    committee_index,
                );
                self.get_from_pool_filtered(&key, &execution_status_fn)
            }
        }
    }

    /// Returns a pre-electra aggregated `Attestation`, if any, matching the given slot and root.
    pub fn get_pre_electra_aggregated_attestation_by_slot_and_root(
        &self,
        slot: Slot,
        attestation_data_root: &Hash256,
        execution_status_fn: impl Fn(&Hash256) -> Option<fork_choice::ExecutionStatus>,
    ) -> Result<Option<Attestation<E>>, Error> {
        let key = crate::naive_aggregation_pool::AttestationKey::new_base_from_slot_and_root(
            slot,
            *attestation_data_root,
        );
        self.get_from_pool_filtered(&key, &execution_status_fn)
    }

    /// Look up an attestation in the pool and filter it for optimistic/invalid execution status.
    fn get_from_pool_filtered(
        &self,
        key: &crate::naive_aggregation_pool::AttestationKey,
        execution_status_fn: &impl Fn(&Hash256) -> Option<fork_choice::ExecutionStatus>,
    ) -> Result<Option<Attestation<E>>, Error> {
        if let Some(attestation) = self.naive_aggregation_pool.read().get(key) {
            Self::filter_optimistic_attestation(attestation, execution_status_fn).map(Option::Some)
        } else {
            Ok(None)
        }
    }

    /// Returns `Ok(attestation)` if the attestation references a fully verified block.
    fn filter_optimistic_attestation(
        attestation: Attestation<E>,
        execution_status_fn: &impl Fn(&Hash256) -> Option<fork_choice::ExecutionStatus>,
    ) -> Result<Attestation<E>, Error> {
        let beacon_block_root = attestation.data().beacon_block_root;
        match execution_status_fn(&beacon_block_root) {
            None => Err(Error::CannotAttestToFinalizedBlock { beacon_block_root }),
            Some(execution_status) if execution_status.is_valid_or_irrelevant() => Ok(attestation),
            Some(execution_status) => Err(Error::HeadBlockNotFullyVerified {
                beacon_block_root,
                execution_status,
            }),
        }
    }

    /// Prune the naive aggregation pool, removing attestations with slots older than allowed.
    pub fn prune_naive_aggregation_pool(&self, current_slot: Slot) {
        self.naive_aggregation_pool.write().prune(current_slot);
    }

    /// Check whether a validator has been seen attesting or aggregating at the given epoch.
    ///
    /// This checks gossip attestations, block attestations, and aggregators. It does
    /// **not** check block production -- the caller should check `ObservedBlockProducers`
    /// separately if needed.
    pub fn validator_seen_at_epoch(&self, validator_index: usize, epoch: Epoch) -> bool {
        // It's necessary to assign these checks to intermediate variables to avoid a deadlock.
        //
        // See: https://github.com/sigp/lighthouse/pull/2230#discussion_r620013993
        let gossip_attested = self
            .observed_gossip_attesters
            .read()
            .index_seen_at_epoch(validator_index, epoch);
        let block_attested = self
            .observed_block_attesters
            .read()
            .index_seen_at_epoch(validator_index, epoch);
        let aggregated = self
            .observed_aggregators
            .read()
            .index_seen_at_epoch(validator_index, epoch);

        gossip_attested || block_attested || aggregated
    }

    /// Check that the shuffling at `block_root` is equal to one of the shufflings of `state`.
    ///
    /// The `block_shuffling_id` should be obtained from fork choice for the given `block_root`
    /// and `target_epoch`. The caller is responsible for determining the correct shuffling ID
    /// from the fork choice block entry.
    ///
    /// Returns `true` if the shufflings are compatible, `false` otherwise.
    pub fn shuffling_is_compatible(
        &self,
        block_root: &Hash256,
        target_epoch: Epoch,
        state: &BeaconState<E>,
        block_shuffling_id: AttestationShufflingId,
    ) -> bool {
        self.shuffling_is_compatible_result(block_root, target_epoch, state, block_shuffling_id)
            .unwrap_or_else(|e| {
                tracing::debug!(
                    ?block_root,
                    %target_epoch,
                    reason = ?e,
                    "Skipping attestation with incompatible shuffling"
                );
                false
            })
    }

    fn shuffling_is_compatible_result(
        &self,
        block_root: &Hash256,
        target_epoch: Epoch,
        state: &BeaconState<E>,
        block_shuffling_id: AttestationShufflingId,
    ) -> Result<bool, Error> {
        let relative_epoch = RelativeEpoch::from_epoch(state.current_epoch(), target_epoch)
            .map_err(|e| Error::BeaconStateError(e.into()))?;
        let head_shuffling_id =
            AttestationShufflingId::new(self.genesis_block_root, state, relative_epoch)?;

        if head_shuffling_id == block_shuffling_id {
            Ok(true)
        } else {
            tracing::debug!(
                ?block_root,
                %target_epoch,
                ?head_shuffling_id,
                ?block_shuffling_id,
                "Skipping attestation with incompatible shuffling"
            );
            Ok(false)
        }
    }
}
