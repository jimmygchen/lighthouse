//! Block verification, import state, and timing caches.
//!
//! `BlockWorkflow<T>` owns the caches and observation trackers directly related
//! to block processing: timing caches, observed block producers, and the
//! pre-finalization block cache. It provides simple accessor and pruning
//! methods on this owned state.
//!
//! ## What stays on `BeaconChain`
//!
//! The following block-related methods remain on `BeaconChain` because they are
//! deeply coupled to other chain components:
//!
//! - **`process_block`**: Uses `Arc<Self>`, `slot_clock`, `data_availability_checker`,
//!   spawns async tasks.
//!
//! - **`import_block`**: Accesses 14+ fields, acquires fork choice write locks with
//!   careful lock ordering (see `canonical_head.rs:9-32`), uses `Arc<Self>`.
//!
//! - **`import_available_block`** / **`check_block_availability_and_import`**: Use
//!   `data_availability_checker` and `Arc<Self>`.
//!
//! - **`verify_block_for_gossip`**: Delegates to `block_verification` which takes
//!   `&BeaconChain<T>`.
//!
//! - **`into_executed_block`**: Async, uses execution layer.
//!
//! - **`filter_chain_segment`** / **`process_chain_segment`**: Async pipeline.
//!
//! - **`is_pre_finalization_block`**: Uses `with_head` and `store` for disk lookups.
//!
//! - **`block_observed_after_attestation_deadline`**: Uses `slot_clock` to compute
//!   slot start time.

#[cfg(test)]
mod tests;

use crate::block_times_cache::BlockTimesCache;
use crate::envelope_times_cache::EnvelopeTimesCache;
use crate::observed_block_producers::ObservedBlockProducers;
use crate::observed_slashable::ObservedSlashable;
use crate::pre_finalization_cache::PreFinalizationBlockCache;
use parking_lot::RwLock;
use std::sync::Arc;
use types::{EthSpec, Slot};

/// Owns block-processing caches and observation trackers.
///
/// This struct groups the state that is specific to the block import path:
/// timing caches used for metrics/diagnostics, observed block producer
/// tracking for duplicate detection, and the pre-finalization block cache.
///
/// Generic over `E: EthSpec` because none of these fields require store
/// access or the slot clock type.
pub struct BlockWorkflow<E: EthSpec> {
    /// Cache tracking timestamps for block observation, verification,
    /// execution, import, and head-setting.
    pub block_times_cache: Arc<RwLock<BlockTimesCache>>,
    /// Cache tracking timestamps for payload envelope observation,
    /// verification, and import.
    pub envelope_times_cache: Arc<RwLock<EnvelopeTimesCache>>,
    /// Cache of pre-finalization block roots for quick rejection of
    /// attestations referencing blocks that are no longer in fork choice.
    pub pre_finalization_block_cache: PreFinalizationBlockCache,
    /// Tracks which validators have proposed blocks in recent slots,
    /// used to detect equivocating (duplicate) block proposals.
    pub observed_block_producers: RwLock<ObservedBlockProducers<E>>,
    /// Tracks slashable messages (equivocating block proposals) observed
    /// over gossip or RPC, supporting `broadcast_validation` in the
    /// Beacon API.
    pub observed_slashable: RwLock<ObservedSlashable<E>>,
}

impl<E: EthSpec> BlockWorkflow<E> {
    /// Create a new `BlockWorkflow` with default (empty) caches.
    pub fn new() -> Self {
        Self {
            block_times_cache: <_>::default(),
            envelope_times_cache: <_>::default(),
            pre_finalization_block_cache: <_>::default(),
            observed_block_producers: <_>::default(),
            observed_slashable: <_>::default(),
        }
    }

    /// Prune timing caches to only retain entries for recent slots.
    ///
    /// Should be called once per slot (e.g., from `per_slot_task`).
    pub fn prune_caches(&self, current_slot: Slot) {
        self.block_times_cache.write().prune(current_slot);
        self.envelope_times_cache.write().prune(current_slot);
    }

    /// Prune the observed block producers cache based on the finalized slot.
    ///
    /// Should be called after finalization updates (e.g., from
    /// `after_new_head`).
    pub fn prune_observed_block_producers(&self, finalized_slot: Slot) {
        self.observed_block_producers.write().prune(finalized_slot);
    }

    /// Prune the observed slashable cache based on the finalized slot.
    ///
    /// Should be called after finalization updates.
    pub fn prune_observed_slashable(&self, finalized_slot: Slot) {
        self.observed_slashable.write().prune(finalized_slot);
    }
}

impl<E: EthSpec> Default for BlockWorkflow<E> {
    fn default() -> Self {
        Self::new()
    }
}
