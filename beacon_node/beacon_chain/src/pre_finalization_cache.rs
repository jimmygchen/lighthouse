use crate::{BeaconChainError, BeaconChainTypes};
use itertools::process_results;
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::time::Duration;
use tracing::debug;
use types::Hash256;
use types::new_non_zero_usize;

const BLOCK_ROOT_CACHE_LIMIT: NonZeroUsize = new_non_zero_usize(512);
const LOOKUP_LIMIT: NonZeroUsize = new_non_zero_usize(8);
const METRICS_TIMEOUT: Duration = Duration::from_millis(100);

/// Cache for rejecting attestations to blocks from before finalization.
///
/// It stores a collection of block roots that are pre-finalization and therefore not known to fork
/// choice in `verify_head_block_is_known` during attestation processing.
#[derive(Default)]
pub struct PreFinalizationBlockCache {
    cache: Mutex<Cache>,
}

struct Cache {
    /// Set of block roots that are known to be pre-finalization.
    block_roots: LruCache<Hash256, ()>,
    /// Set of block roots that are the subject of single block lookups.
    in_progress_lookups: LruCache<Hash256, ()>,
}

impl Default for Cache {
    fn default() -> Self {
        Cache {
            block_roots: LruCache::new(BLOCK_ROOT_CACHE_LIMIT),
            in_progress_lookups: LruCache::new(LOOKUP_LIMIT),
        }
    }
}

impl PreFinalizationBlockCache {
    /// Check whether the block with `block_root` is known to be pre-finalization.
    ///
    /// This is a standalone version that accepts component refs instead of requiring
    /// `&BeaconChain<T>`, enabling use from `AttestationVerificationContext`.
    pub fn is_pre_finalization_block<T: BeaconChainTypes>(
        &self,
        block_root: Hash256,
        head_snapshot: &crate::beacon_snapshot::BeaconSnapshot<T::EthSpec>,
        store: &crate::BeaconStore<T>,
        spec: &types::ChainSpec,
    ) -> Result<bool, BeaconChainError> {
        let mut cache = self.cache.lock();

        // Check the cache to see if we already know this pre-finalization block root.
        if cache.block_roots.contains(&block_root) {
            return Ok(true);
        }

        // Avoid repeating the disk lookup for blocks that are already subject to a network lookup.
        if cache.in_progress_lookups.contains(&block_root) {
            return Ok(false);
        }

        // 1. Check memory for a recent pre-finalization block.
        let is_recent_finalized_block = process_results(
            head_snapshot.beacon_state.rev_iter_block_roots(spec),
            |mut iter| iter.any(|(_, root)| root == block_root),
        )
        .map_err(BeaconChainError::BeaconStateError)?;
        if is_recent_finalized_block {
            cache.block_roots.put(block_root, ());
            return Ok(true);
        }

        // 2. Check on disk.
        if store.get_blinded_block(&block_root)?.is_some() {
            cache.block_roots.put(block_root, ());
            return Ok(true);
        }

        // 3. Check the network with a single block lookup.
        cache.in_progress_lookups.put(block_root, ());
        if cache.in_progress_lookups.len() == LOOKUP_LIMIT.get() {
            debug!("Pre-finalization lookup cache is full");
        }
        Ok(false)
    }

    pub fn block_rejected(&self, block_root: Hash256) {
        // Future requests can know that this block is invalid without having to look it up again.
        let mut cache = self.cache.lock();
        cache.in_progress_lookups.pop(&block_root);
        cache.block_roots.put(block_root, ());
    }

    pub fn block_processed(&self, block_root: Hash256) {
        // Future requests will find this block in fork choice, so no need to cache it in the
        // ongoing lookup cache any longer.
        self.cache.lock().in_progress_lookups.pop(&block_root);
    }

    pub fn contains(&self, block_root: Hash256) -> bool {
        self.cache.lock().block_roots.contains(&block_root)
    }

    pub fn metrics(&self) -> Option<(usize, usize)> {
        let cache = self.cache.try_lock_for(METRICS_TIMEOUT)?;
        Some((cache.block_roots.len(), cache.in_progress_lookups.len()))
    }
}
