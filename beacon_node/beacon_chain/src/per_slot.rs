//! Per-slot tasks run by the slot timer.

use crate::beacon_chain::{BeaconChain, BeaconChainTypes};
use slot_clock::SlotClock;
use std::sync::Arc;
use tracing::{debug, warn};

/// If the head is more than `MAX_PER_SLOT_FORK_CHOICE_DISTANCE` slots behind the wall-clock slot, DO NOT
/// run the per-slot tasks (primarily fork choice).
///
/// This prevents unnecessary work during sync.
///
/// The value is set to 256 since this would be just over one slot (12.8s) when syncing at
/// 20 slots/second. Having a single fork-choice run interrupt syncing would have very little
/// impact whilst having 8 epochs without a block is a comfortable grace period.
const MAX_PER_SLOT_FORK_CHOICE_DISTANCE: u64 = 256;

/// Called by the timer on every slot.
///
/// Note: this function **MUST** be called from a non-async context since
/// it contains a call to `fork_choice` which may eventually call
/// `tokio::runtime::block_on` in certain cases.
pub async fn per_slot_task<T: BeaconChainTypes>(chain: &Arc<BeaconChain<T>>) {
    if let Some(slot) = chain.slot_clock.now() {
        debug!(?slot, "Running beacon chain per slot tasks");

        chain
            .attestation_manager
            .naive_aggregation_pool
            .write()
            .prune(slot);
        chain.block_importer.block_times_cache.write().prune(slot);
        chain
            .block_importer
            .envelope_times_cache
            .write()
            .prune(slot);
        chain
            .block_producer
            .gossip_verified_payload_bid_cache
            .prune(slot);
        chain
            .block_producer
            .gossip_verified_proposer_preferences_cache
            .prune(slot);

        if chain.canonical_head.best_slot() + MAX_PER_SLOT_FORK_CHOICE_DISTANCE < slot {
            return;
        }

        crate::canonical_head::recompute_head_at_current_slot(chain).await;

        let chain_clone = chain.clone();
        chain.task_executor.clone().spawn_blocking(
            move || {
                if let Some(tx) = &chain_clone.canonical_head.fork_choice_signal_tx
                    && let Err(e) = tx.notify_fork_choice_complete(slot)
                {
                    warn!(
                        error = ?e,
                        %slot,
                        "Error signalling fork choice waiter"
                    );
                }
            },
            "per_slot_task_fc_signal_tx",
        );
    }
}
