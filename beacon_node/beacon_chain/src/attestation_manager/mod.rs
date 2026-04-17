//! Manages attestation pools, observation tracking, and shuffling caches.
//!
//! `AttestationManager<E>` owns the naive aggregation pool, observed attestation/attester/aggregator
//! tracking, early attester cache, and shuffling cache. It provides side-effect-free methods that
//! operate on these owned data structures.
//!
//! The free function `with_committee_cache` provides shared committee cache access logic used by
//! both `AttestationManager` methods and `AttestationVerificationContext`.
//!
//! ## What stays on `BeaconComponents`
//!
//! The following attestation-related methods remain on `BeaconComponents` because they depend on
//! components the manager intentionally does not hold:
//!
//! - **Verification methods** (`verify_unaggregated_attestation_for_gossip`, etc.): The
//!   `attestation_verification` module takes `AttestationVerificationContext` and accesses fields
//!   like `canonical_head`, `observed_attestations`, `observed_aggregators`, and
//!   `with_committee_cache`. Refactoring that module is out of scope for this extraction.
//!
//! - **`apply_attestation_to_fork_choice`**: Needs `fork_choice_write_lock` and `slot_clock`.
//!
//! - **`add_to_block_inclusion_pool`**: Directly accesses `op_pool` which is not owned by the
//!   manager (wrapping it in `Arc` would be too invasive).
//!
//! - **`get_aggregated_attestation`** and variants: Need `filter_optimistic_attestation` which
//!   reads `canonical_head.fork_choice_read_lock()` to check execution status.
//!
//! - **`filter_op_pool_attestation`**: Delegates to `shuffling_is_compatible` which needs
//!   `canonical_head.fork_choice_read_lock()`.
//!
//! - **`shuffling_is_compatible`**: Needs `canonical_head.fork_choice_read_lock()` to load block
//!   shuffling IDs from fork choice. The `BeaconComponents` wrapper obtains the block from fork choice
//!   and then delegates the pure shuffling comparison to this manager.

#[cfg(test)]
mod tests;

use crate::canonical_head::CanonicalHead;
use crate::early_attester_cache::EarlyAttesterCache;
use crate::errors::BeaconChainError as Error;
use crate::naive_aggregation_pool::{
    AggregatedAttestationMap, Error as NaiveAggregationError, NaiveAggregationPool,
};
use crate::observed_aggregates::ObservedAggregateAttestations;
use crate::observed_attesters::{ObservedAggregators, ObservedAttesters};
use crate::shuffling_cache::{BlockShufflingIds, ShufflingCache};
use crate::{BeaconChainError, BeaconChainTypes, BeaconStore, metrics};
use fork_choice::{self, ExecutionStatus};
use parking_lot::RwLock;
use state_processing::state_advance::partial_state_advance;
use std::sync::Arc;
use tracing::{debug, error, trace, warn};
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
    pub observed_attestations: RwLock<ObservedAggregateAttestations<E>>,
    /// Tracks validators that have sent gossip attestations.
    pub observed_gossip_attesters: RwLock<ObservedAttesters<E>>,
    /// Tracks validators whose attestations have been included in blocks.
    pub observed_block_attesters: RwLock<ObservedAttesters<E>>,
    /// Tracks aggregators that have produced signed aggregates.
    pub observed_aggregators: RwLock<ObservedAggregators<E>>,
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

    /// Returns an aggregated electra `Attestation`, if any, matching the given slot, root and committee index.
    pub fn get_aggregated_attestation_electra(
        &self,
        slot: Slot,
        attestation_data_root: &Hash256,
        committee_index: CommitteeIndex,
        execution_status_fn: impl Fn(&Hash256) -> Option<fork_choice::ExecutionStatus>,
    ) -> Result<Option<Attestation<E>>, Error> {
        let key = crate::naive_aggregation_pool::AttestationKey::new_electra(
            slot,
            *attestation_data_root,
            committee_index,
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

    /// Produce an unaggregated `Attestation` that is valid for the given `slot` and `index`.
    ///
    /// The produced `Attestation` will not be valid until it has been signed by exactly one
    /// validator that is in the committee for `slot` and `index` in the canonical chain.
    ///
    /// Always attests to the canonical chain.
    ///
    /// ## Errors
    ///
    /// May return an error if the `request_slot` is too far behind the head state.
    #[allow(clippy::too_many_arguments)]
    pub fn produce_unaggregated_attestation<T: BeaconChainTypes<EthSpec = E>>(
        &self,
        request_slot: Slot,
        request_index: CommitteeIndex,
        canonical_head: &CanonicalHead<T>,
        store: &BeaconStore<T>,
        spec: &ChainSpec,
    ) -> Result<Attestation<E>, Error> {
        let _total_timer = metrics::start_timer(&metrics::ATTESTATION_PRODUCTION_SECONDS);

        // The early attester cache will return `Some(attestation)` in the scenario where there is a
        // block being imported that will become the head block, but that block has not yet been
        // inserted into the database and set as `self.canonical_head`.
        //
        // In effect, the early attester cache prevents slow database IO from causing missed
        // head/target votes.
        //
        // The early attester cache should never contain an optimistically imported block.
        match self
            .early_attester_cache
            .try_attest(request_slot, request_index, spec)
        {
            // The cache matched this request, return the value.
            Ok(Some(attestation)) => return Ok(attestation),
            // The cache did not match this request, proceed with the rest of this function.
            Ok(None) => (),
            // The cache returned an error. Log the error and proceed with the rest of this
            // function.
            Err(e) => warn!(
                error = ?e,
                "Early attester cache failed"
            ),
        }

        let slots_per_epoch = E::slots_per_epoch();
        let request_epoch = request_slot.epoch(slots_per_epoch);

        /*
         * Phase 1/2:
         *
         * Take a short-lived read-lock on the head and copy the necessary information from it.
         *
         * It is important that this first phase is as quick as possible; creating contention for
         * the head-lock is not desirable.
         */

        let beacon_block_root;
        let beacon_state_root;
        let target;
        let current_epoch_attesting_info: Option<(Checkpoint, usize)>;
        let head_timer = metrics::start_timer(&metrics::ATTESTATION_PRODUCTION_HEAD_SCRAPE_SECONDS);
        {
            let head = canonical_head.cached_head().snapshot;
            let head_state = &head.beacon_state;

            // There is no value in producing an attestation to a block that is pre-finalization and
            // it is likely to cause expensive and pointless reads to the freezer database. Exit
            // early if this is the case.
            let finalized_slot = head_state
                .finalized_checkpoint()
                .epoch
                .start_slot(slots_per_epoch);
            if request_slot < finalized_slot {
                return Err(Error::AttestingToFinalizedSlot {
                    finalized_slot,
                    request_slot,
                });
            }

            // This function will eventually fail when trying to access a slot which is
            // out-of-bounds of `state.block_roots`. This explicit error is intended to provide a
            // clearer message to the user than an ambiguous `SlotOutOfBounds` error.
            let slots_per_historical_root = E::slots_per_historical_root() as u64;
            let lowest_permissible_slot =
                head_state.slot().saturating_sub(slots_per_historical_root);
            if request_slot < lowest_permissible_slot {
                return Err(Error::AttestingToAncientSlot {
                    lowest_permissible_slot,
                    request_slot,
                });
            }

            if request_slot >= head_state.slot() {
                // When attesting to the head slot or later, always use the head of the chain.
                beacon_block_root = head.beacon_block_root;
                beacon_state_root = head.beacon_state_root();
            } else {
                // Permit attesting to slots *prior* to the current head. This is desirable when
                // the VC and BN are out-of-sync due to time issues or overloading.
                beacon_block_root = *head_state.get_block_root(request_slot)?;
                beacon_state_root = *head_state.get_state_root(request_slot)?;
            };

            let target_slot = request_epoch.start_slot(E::slots_per_epoch());
            let target_root = if head_state.slot() <= target_slot {
                // If the state is earlier than the target slot then the target *must* be the head
                // block root.
                beacon_block_root
            } else {
                *head_state.get_block_root(target_slot)?
            };
            target = Checkpoint {
                epoch: request_epoch,
                root: target_root,
            };

            current_epoch_attesting_info = if head_state.current_epoch() == request_epoch {
                // When the head state is in the same epoch as the request, all the information
                // required to attest is available on the head state.
                Some((
                    head_state.current_justified_checkpoint(),
                    head_state
                        .get_beacon_committee(request_slot, request_index)?
                        .committee
                        .len(),
                ))
            } else {
                // If the head state is in a *different* epoch to the request, more work is required
                // to determine the justified checkpoint and committee length.
                None
            };
        }
        drop(head_timer);

        // Only attest to a block if it is fully verified (i.e. not optimistic or invalid).
        match canonical_head
            .fork_choice_read_lock()
            .get_block_execution_status(&beacon_block_root)
        {
            Some(execution_status) if execution_status.is_valid_or_irrelevant() => (),
            Some(execution_status) => {
                return Err(Error::HeadBlockNotFullyVerified {
                    beacon_block_root,
                    execution_status,
                });
            }
            None => return Err(Error::HeadMissingFromForkChoice(beacon_block_root)),
        };

        /*
         *  Phase 2/2:
         *
         *  If the justified checkpoint and committee length from the head are suitable for this
         *  attestation, use them. If not, use the database, which will hit the state cache.
         */
        let (justified_checkpoint, committee_len) =
            if let Some((justified_checkpoint, committee_len)) = current_epoch_attesting_info {
                // The head state is in the same epoch as the attestation, so there is no more
                // required information.
                (justified_checkpoint, committee_len)
            } else {
                // We assume that the `Pending` state has the same shufflings as a `Full` state
                // for the same block. Analysis: https://hackmd.io/@dapplion/gloas_dependant_root
                let (advanced_state_root, mut state) = store
                    .get_advanced_hot_state(
                        beacon_block_root,
                        StatePayloadStatus::Pending,
                        request_slot,
                        beacon_state_root,
                    )?
                    .ok_or(Error::MissingBeaconState(beacon_state_root))?;
                if state.current_epoch() < request_epoch {
                    partial_state_advance(
                        &mut state,
                        Some(advanced_state_root),
                        request_epoch.start_slot(E::slots_per_epoch()),
                        spec,
                    )
                    .map_err(Error::StateAdvanceError)?;

                    state.build_committee_cache(RelativeEpoch::Current, spec)?;
                }

                (
                    state.current_justified_checkpoint(),
                    state
                        .get_beacon_committee(request_slot, request_index)?
                        .committee
                        .len(),
                )
            };

        Ok(Attestation::<E>::empty_for_signing(
            request_index,
            committee_len,
            request_slot,
            beacon_block_root,
            justified_checkpoint,
            target,
            spec,
        )?)
    }

    /// Return the attestation duties for the given `validator_indices` at `epoch`.
    ///
    /// Uses the shuffling cache when available, falling back to state loading on cache miss.
    pub fn validator_attestation_duties<T: BeaconChainTypes<EthSpec = E>>(
        &self,
        validator_indices: &[u64],
        epoch: Epoch,
        head_block_root: Hash256,
        canonical_head: &CanonicalHead<T>,
        store: &BeaconStore<T>,
        spec: &ChainSpec,
    ) -> Result<(Vec<Option<AttestationDuty>>, Hash256, ExecutionStatus), Error> {
        let execution_status = canonical_head
            .fork_choice_read_lock()
            .get_block_execution_status(&head_block_root)
            .ok_or(Error::AttestationHeadNotInForkChoice(head_block_root))?;

        let (duties, dependent_root) = with_committee_cache(
            head_block_root,
            epoch,
            canonical_head,
            self,
            store,
            spec,
            |committee_cache, dependent_root| {
                let duties = validator_indices
                    .iter()
                    .map(|validator_index| {
                        let validator_index = *validator_index as usize;
                        committee_cache.get_attestation_duties(validator_index)
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                Ok((duties, dependent_root))
            },
        )?;
        Ok((duties, dependent_root, execution_status))
    }
}

/// Provides access to the committee cache, loading from the store on cache miss.
///
/// This is a free function rather than a method to allow both `BeaconComponents` and
/// `AttestationVerificationContext` to share the same implementation.
///
/// The `map_fn` is applied to the committee cache and the shuffling decision block root.
pub fn with_committee_cache<T, F, R>(
    head_block_root: Hash256,
    shuffling_epoch: Epoch,
    canonical_head: &CanonicalHead<T>,
    attestation_manager: &AttestationManager<T::EthSpec>,
    store: &BeaconStore<T>,
    spec: &ChainSpec,
    map_fn: F,
) -> Result<R, Error>
where
    T: BeaconChainTypes,
    F: Fn(&CommitteeCache, Hash256) -> Result<R, Error>,
{
    let head_block = canonical_head
        .fork_choice_read_lock()
        .get_block(&head_block_root)
        .ok_or(Error::MissingBeaconBlock(head_block_root))?;

    let shuffling_id = BlockShufflingIds {
        current: head_block.current_epoch_shuffling_id.clone(),
        next: head_block.next_epoch_shuffling_id.clone(),
        previous: None,
        block_root: head_block.root,
    }
    .id_for_epoch(shuffling_epoch)
    .ok_or_else(|| Error::InvalidShufflingId {
        shuffling_epoch,
        head_block_epoch: head_block.slot.epoch(T::EthSpec::slots_per_epoch()),
    })?;

    // Obtain the shuffling cache, timing how long we wait.
    let mut shuffling_cache = {
        let _ = metrics::start_timer(&metrics::ATTESTATION_PROCESSING_SHUFFLING_CACHE_WAIT_TIMES);
        attestation_manager.shuffling_cache.write()
    };

    if let Some(cache_item) = shuffling_cache.get(&shuffling_id) {
        // The shuffling cache is no longer required, drop the write-lock to allow concurrent
        // access.
        drop(shuffling_cache);

        let committee_cache = cache_item.wait()?;
        map_fn(&committee_cache, shuffling_id.shuffling_decision_block)
    } else {
        // Create an entry in the cache that "promises" this value will eventually be computed.
        // This avoids the case where multiple threads attempt to produce the same value at the
        // same time.
        //
        // Creating the promise whilst we hold the `shuffling_cache` lock will prevent the same
        // promise from being created twice.
        let sender = shuffling_cache.create_promise(shuffling_id.clone())?;

        // Drop the shuffling cache to avoid holding the lock for any longer than
        // required.
        drop(shuffling_cache);

        debug!(
            shuffling_id = ?shuffling_epoch,
            head_block_root = head_block_root.to_string(),
            "Committee cache miss"
        );

        // If the block's state will be so far ahead of `shuffling_epoch` that even its
        // previous epoch committee cache will be too new, then error. Callers of this function
        // shouldn't be requesting such old shufflings for this `head_block_root`.
        let head_block_epoch = head_block.slot.epoch(T::EthSpec::slots_per_epoch());
        if head_block_epoch > shuffling_epoch + 1 {
            return Err(Error::InvalidStateForShuffling {
                state_epoch: head_block_epoch,
                shuffling_epoch,
            });
        }

        let state_read_timer =
            metrics::start_timer(&metrics::ATTESTATION_PROCESSING_STATE_READ_TIMES);

        // If the head of the chain can serve this request, use it.
        let head_state_opt = {
            let cached_head = canonical_head.cached_head();
            let snapshot = &cached_head.snapshot;
            if snapshot.beacon_block_root == head_block_root {
                Some((snapshot.beacon_state.clone(), snapshot.beacon_state_root()))
            } else {
                None
            }
        };

        // Compute the `target_slot` to advance the block's state to.
        //
        // Since there's a one-epoch look-ahead on the attester shuffling, it suffices to
        // only advance into the first slot of the epoch prior to `shuffling_epoch`.
        //
        // If the `head_block` is already ahead of that slot, then we should load the state
        // at that slot, as we've determined above that the `shuffling_epoch` cache will
        // not be too far in the past.
        let target_slot = std::cmp::max(
            shuffling_epoch
                .saturating_sub(1_u64)
                .start_slot(T::EthSpec::slots_per_epoch()),
            head_block.slot,
        );

        // If the head state is useful for this request, use it. Otherwise, read a state from
        // disk that is advanced as close as possible to `target_slot`.
        let (mut state, state_root) = if let Some((state, state_root)) = head_state_opt {
            (state, state_root)
        } else {
            // We assume that the `Pending` state has the same shufflings as a `Full` state
            // for the same block. Analysis: https://hackmd.io/@dapplion/gloas_dependant_root
            let (state_root, state) = store
                .get_advanced_hot_state(
                    head_block_root,
                    StatePayloadStatus::Pending,
                    target_slot,
                    head_block.state_root,
                )?
                .ok_or(Error::MissingBeaconState(head_block.state_root))?;
            (state, state_root)
        };

        metrics::stop_timer(state_read_timer);
        let state_skip_timer =
            metrics::start_timer(&metrics::ATTESTATION_PROCESSING_STATE_SKIP_TIMES);

        // If the state is still in an earlier epoch, advance it to the `target_slot` so
        // that its next epoch committee cache matches the `shuffling_epoch`.
        if state.current_epoch() + 1 < shuffling_epoch {
            // Advance the state into the required slot, using the "partial" method since the
            // state roots are not relevant for the shuffling.
            partial_state_advance(&mut state, Some(state_root), target_slot, spec)?;
        }
        metrics::stop_timer(state_skip_timer);

        let committee_building_timer =
            metrics::start_timer(&metrics::ATTESTATION_PROCESSING_COMMITTEE_BUILDING_TIMES);

        let relative_epoch = RelativeEpoch::from_epoch(state.current_epoch(), shuffling_epoch)
            .map_err(Error::IncorrectStateForAttestation)?;

        state.build_committee_cache(relative_epoch, spec)?;

        let committee_cache = state.committee_cache(relative_epoch)?.clone();
        let shuffling_decision_block = shuffling_id.shuffling_decision_block;

        attestation_manager
            .shuffling_cache
            .write()
            .insert_committee_cache(shuffling_id, &committee_cache);

        metrics::stop_timer(committee_building_timer);

        sender.send(committee_cache.clone());

        map_fn(&committee_cache, shuffling_decision_block)
    }
}
