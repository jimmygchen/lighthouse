//! Free functions for state and block root queries on the canonical chain.
//!
//! Each function takes explicit params (`store`, `canonical_head`, `spec`, etc.)
//! instead of `&BeaconChain`. Thin delegations on `impl BeaconChain<T>` are
//! provided so existing callers can continue to use `chain.method()`.

use crate::beacon_chain::{
    BeaconChainTypes, BeaconStore, FinalizationAndCanonicity, StateSkipConfig, WhenSlotSkipped,
};
use crate::canonical_head::CanonicalHead;
use crate::errors::BeaconChainError as Error;
use fixed_bytes::FixedBytesExtended;
use itertools::Itertools;
use itertools::process_results;
use slot_clock::SlotClock;
use state_processing::per_slot_processing;
use std::cmp::Ordering;
use store::iter::{BlockRootsIterator, StateRootsIterator};
use tracing::{instrument, warn};
use types::*;

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Returns the current slot according to the given slot clock.
pub fn current_slot<S: SlotClock>(slot_clock: &S) -> Result<Slot, Error> {
    slot_clock.now().ok_or(Error::UnableToReadSlot)
}

/// Returns the current epoch according to the given slot clock.
pub fn current_epoch<E: EthSpec, S: SlotClock>(slot_clock: &S) -> Result<Epoch, Error> {
    current_slot(slot_clock).map(|slot| slot.epoch(E::slots_per_epoch()))
}

/// Iterates forwards across all `(block_root, slot)` pairs from `start_slot`
/// to the head of the chain (inclusive).
///
/// - `slot` always increases by `1`.
/// - Skipped slots contain the root of the closest prior non-skipped slot.
///
/// Returns `Err(HistoricalBlockOutOfRange)` if `start_slot` is before the
/// oldest stored block slot.
pub fn forwards_iter_block_roots<'a, T: BeaconChainTypes>(
    store: &'a BeaconStore<T>,
    canonical_head: &'a CanonicalHead<T>,
    start_slot: Slot,
) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + 'a, Error> {
    let oldest_block_slot = store.get_oldest_block_slot();
    if start_slot < oldest_block_slot {
        return Err(Error::HistoricalBlockOutOfRange {
            slot: start_slot,
            oldest_block_slot,
        });
    }

    let local_head = canonical_head.cached_head().snapshot;

    let iter = store.forwards_block_roots_iterator(
        start_slot,
        local_head.beacon_state.clone(),
        local_head.beacon_block_root,
    )?;

    Ok(iter.map(|result| result.map_err(Into::into)))
}

/// Efficient variant of [`forwards_iter_block_roots`] that avoids cloning the
/// head state when the requested range lies entirely within the freezer DB.
///
/// The range `[start_slot, end_slot]` is inclusive.
pub fn forwards_iter_block_roots_until<'a, T: BeaconChainTypes>(
    store: &'a BeaconStore<T>,
    canonical_head: &'a CanonicalHead<T>,
    start_slot: Slot,
    end_slot: Slot,
) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + 'a, Error> {
    let oldest_block_slot = store.get_oldest_block_slot();
    if start_slot < oldest_block_slot {
        return Err(Error::HistoricalBlockOutOfRange {
            slot: start_slot,
            oldest_block_slot,
        });
    }

    let head = canonical_head.cached_head().snapshot;
    let iter = store.forwards_block_roots_iterator_until(start_slot, end_slot, || {
        Ok((head.beacon_state.clone(), head.beacon_block_root))
    })?;
    Ok(iter
        .map(|result| result.map_err(Into::into))
        .take_while(move |result| result.as_ref().map_or(true, |(_, slot)| *slot <= end_slot)))
}

/// Traverse backwards from `block_root` to find the block roots of its ancestors.
///
/// - `slot` always decreases by `1`.
/// - Skipped slots contain the root of the closest prior non-skipped slot.
/// - The provided `block_root` is included as the first item.
pub fn rev_iter_block_roots_from<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    block_root: Hash256,
) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + '_, Error> {
    let block = store
        .get_blinded_block(&block_root)?
        .ok_or(Error::MissingBeaconBlock(block_root))?;
    // This method is only used in tests, so we may as well cache states to make CI go brr.
    let state = store
        .get_state(&block.state_root(), Some(block.slot()), true)?
        .ok_or_else(|| Error::MissingBeaconState(block.state_root()))?;
    let iter = BlockRootsIterator::owned(store, state);
    Ok(std::iter::once(Ok((block_root, block.slot())))
        .chain(iter)
        .map(|result| result.map_err(|e| e.into())))
}

/// Iterates backwards across all `(state_root, slot)` pairs starting from an
/// arbitrary `BeaconState` to the earliest reachable ancestor.
///
/// - `slot` always decreases by `1`.
/// - The first slot returned may be earlier than the wall-clock slot.
pub fn rev_iter_state_roots_from<'a, T: BeaconChainTypes>(
    store: &'a BeaconStore<T>,
    state_root: Hash256,
    state: &'a BeaconState<T::EthSpec>,
) -> impl Iterator<Item = Result<(Hash256, Slot), Error>> + 'a {
    std::iter::once(Ok((state_root, state.slot())))
        .chain(StateRootsIterator::new(store, state))
        .map(|result| result.map_err(Into::into))
}

/// Iterates forwards across all `(state_root, slot)` pairs from `start_slot`
/// to the head of the chain (inclusive).
pub fn forwards_iter_state_roots<'a, T: BeaconChainTypes>(
    store: &'a BeaconStore<T>,
    canonical_head: &'a CanonicalHead<T>,
    start_slot: Slot,
) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + 'a, Error> {
    let local_head = canonical_head.cached_head().snapshot;

    let iter = store.forwards_state_roots_iterator(
        start_slot,
        local_head.beacon_state_root(),
        local_head.beacon_state.clone(),
    )?;

    Ok(iter.map(|result| result.map_err(Into::into)))
}

/// Efficient variant of [`forwards_iter_state_roots`] that avoids cloning the
/// head state when the requested range lies entirely within the freezer DB.
///
/// The range `[start_slot, end_slot]` is inclusive.
pub fn forwards_iter_state_roots_until<'a, T: BeaconChainTypes>(
    store: &'a BeaconStore<T>,
    canonical_head: &'a CanonicalHead<T>,
    start_slot: Slot,
    end_slot: Slot,
) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + 'a, Error> {
    let head = canonical_head.cached_head().snapshot;
    let iter = store.forwards_state_roots_iterator_until(start_slot, end_slot, || {
        Ok((head.beacon_state.clone(), head.beacon_state_root()))
    })?;
    Ok(iter
        .map(|result| result.map_err(Into::into))
        .take_while(move |result| result.as_ref().map_or(true, |(_, slot)| *slot <= end_slot)))
}

/// Returns the block at the given slot, if any. Only returns blocks in the
/// canonical chain.
///
/// Use `skips` to define the behaviour when `request_slot` is a skipped slot.
pub fn block_at_slot<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_block_root: Hash256,
    request_slot: Slot,
    skips: WhenSlotSkipped,
) -> Result<Option<SignedBlindedBeaconBlock<T::EthSpec>>, Error> {
    let root = block_root_at_slot(
        store,
        canonical_head,
        spec,
        slot_clock,
        genesis_block_root,
        request_slot,
        skips,
    )?;

    if let Some(block_root) = root {
        Ok(store.get_blinded_block(&block_root)?)
    } else {
        Ok(None)
    }
}

/// Returns the state root at the given slot, if any. Only returns state roots
/// in the canonical chain.
pub fn state_root_at_slot<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_state_root: Hash256,
    request_slot: Slot,
) -> Result<Option<Hash256>, Error> {
    if request_slot == spec.genesis_slot {
        return Ok(Some(genesis_state_root));
    } else if request_slot > slot_clock.now().ok_or(Error::UnableToReadSlot)? {
        return Ok(None);
    }

    // Check limits w.r.t historic state bounds.
    let (historic_lower_limit, historic_upper_limit) = store.get_historic_state_limits();
    if request_slot > historic_lower_limit && request_slot < historic_upper_limit {
        return Ok(None);
    }

    // Fast-path for the split slot (which usually corresponds to the finalized slot).
    // Post-Gloas, the split state root is always the Pending root but the canonical state root
    // at the finalized slot may be the Full root (from the state_roots vector). Skip the
    // fast-path for Gloas to ensure consistency with the forwards state root iterator.
    // TODO(gloas): revisit this if spec changes to finalize payload status.
    let split = store.get_split_info();
    if request_slot == split.slot
        && !spec
            .fork_name_at_slot::<T::EthSpec>(split.slot)
            .gloas_enabled()
    {
        return Ok(Some(split.state_root));
    }

    // Try an optimized path of reading the root directly from the head state.
    let head = canonical_head.cached_head().snapshot;
    let fast_lookup: Option<Hash256> = if head.beacon_block.slot() <= request_slot {
        // Return the head state root if all slots between the request and the head are skipped.
        Some(head.beacon_state_root())
    } else if let Ok(root) = head.beacon_state.get_state_root(request_slot) {
        // Return the root if it's easily accessible from the head state.
        Some(*root)
    } else {
        // Fast lookup is not possible.
        None
    };

    if let Some(root) = fast_lookup {
        return Ok(Some(root));
    }

    process_results(
        forwards_iter_state_roots_until(store, canonical_head, request_slot, request_slot)?,
        |mut iter| {
            if let Some((root, slot)) = iter.next() {
                if slot == request_slot {
                    Ok(Some(root))
                } else {
                    // Sanity check.
                    Err(Error::InconsistentForwardsIter { request_slot, slot })
                }
            } else {
                Ok(None)
            }
        },
    )?
}

/// Returns the block root at the given slot, if any. Only returns roots in
/// the canonical chain.
///
/// - Use `skips` to define behaviour when `request_slot` is a skipped slot.
/// - Returns `Ok(None)` for any slot higher than the current wall-clock slot,
///   or less than the oldest known block slot.
pub fn block_root_at_slot<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_block_root: Hash256,
    request_slot: Slot,
    skips: WhenSlotSkipped,
) -> Result<Option<Hash256>, Error> {
    match skips {
        WhenSlotSkipped::None => block_root_at_slot_skips_none(
            store,
            canonical_head,
            spec,
            slot_clock,
            genesis_block_root,
            request_slot,
        ),
        WhenSlotSkipped::Prev => block_root_at_slot_skips_prev(
            store,
            canonical_head,
            spec,
            slot_clock,
            genesis_block_root,
            request_slot,
        ),
    }
    .or_else(|e| match e {
        Error::HistoricalBlockOutOfRange { .. } => Ok(None),
        e => Err(e),
    })
}

/// Returns the block root at the given slot. Returns `Ok(None)` if the slot
/// was skipped.
fn block_root_at_slot_skips_none<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_block_root: Hash256,
    request_slot: Slot,
) -> Result<Option<Hash256>, Error> {
    if request_slot == spec.genesis_slot {
        return Ok(Some(genesis_block_root));
    } else if request_slot > slot_clock.now().ok_or(Error::UnableToReadSlot)? {
        return Ok(None);
    }

    let prev_slot = request_slot.saturating_sub(1_u64);

    // Try an optimized path of reading the root directly from the head state.
    let head = canonical_head.cached_head().snapshot;
    let state = &head.beacon_state;

    // Try find the root for the `request_slot`.
    let request_root_opt = match state.slot().cmp(&request_slot) {
        // It's always a skip slot if the head is less than the request slot, return early.
        Ordering::Less => return Ok(None),
        // The request slot is the head slot.
        Ordering::Equal => Some(head.beacon_block_root),
        // Try find the request slot in the state.
        Ordering::Greater => state.get_block_root(request_slot).ok().copied(),
    };

    if let Some(request_root) = request_root_opt
        && let Ok(prev_root) = state.get_block_root(prev_slot)
    {
        return Ok((*prev_root != request_root).then_some(request_root));
    }

    // Do not try to access the previous slot if it's older than the oldest block root
    // stored in the database. Instead, load just the block root at `oldest_block_slot`,
    // under the assumption that the `oldest_block_slot` *is not* a skipped slot (should be
    // true because it is set by the oldest *block*).
    if request_slot == store.get_anchor_info().oldest_block_slot {
        return block_root_at_slot_skips_prev(
            store,
            canonical_head,
            spec,
            slot_clock,
            genesis_block_root,
            request_slot,
        );
    }

    if let Some(((prev_root, _), (curr_root, curr_slot))) = process_results(
        forwards_iter_block_roots_until(store, canonical_head, prev_slot, request_slot)?,
        |iter| iter.tuple_windows().next(),
    )? {
        // Sanity check.
        if curr_slot != request_slot {
            return Err(Error::InconsistentForwardsIter {
                request_slot,
                slot: curr_slot,
            });
        }
        Ok((curr_root != prev_root).then_some(curr_root))
    } else {
        Ok(None)
    }
}

/// Returns the block root at the given slot. Returns the root at the
/// previous non-skipped slot if the given slot was skipped.
fn block_root_at_slot_skips_prev<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_block_root: Hash256,
    request_slot: Slot,
) -> Result<Option<Hash256>, Error> {
    if request_slot == spec.genesis_slot {
        return Ok(Some(genesis_block_root));
    } else if request_slot > slot_clock.now().ok_or(Error::UnableToReadSlot)? {
        return Ok(None);
    }

    // Try an optimized path of reading the root directly from the head state.
    let head = canonical_head.cached_head().snapshot;
    let fast_lookup: Option<Hash256> = if head.beacon_block.slot() <= request_slot {
        // Return the head root if all slots between the request and the head are skipped.
        Some(head.beacon_block_root)
    } else if let Ok(root) = head.beacon_state.get_block_root(request_slot) {
        // Return the root if it's easily accessible from the head state.
        Some(*root)
    } else {
        // Fast lookup is not possible.
        None
    };
    if let Some(root) = fast_lookup {
        return Ok(Some(root));
    }

    process_results(
        forwards_iter_block_roots_until(store, canonical_head, request_slot, request_slot)?,
        |mut iter| {
            if let Some((root, slot)) = iter.next() {
                if slot == request_slot {
                    Ok(Some(root))
                } else {
                    // Sanity check.
                    Err(Error::InconsistentForwardsIter { request_slot, slot })
                }
            } else {
                Ok(None)
            }
        },
    )?
}

/// Returns the `BeaconState` at the given slot.
///
/// Returns `Err(NoStateForSlot)` when the state is not found or there is an
/// error skipping to a future state.
#[instrument(level = "debug", skip_all)]
pub fn state_at_slot<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot: Slot,
    config: StateSkipConfig,
) -> Result<BeaconState<T::EthSpec>, Error> {
    // Don't clone whilst holding the read-lock, take an Arc-clone to reduce lock contention.
    let snapshot = canonical_head.cached_head().snapshot;
    let head_state = snapshot.beacon_state.clone();

    match slot.cmp(&head_state.slot()) {
        Ordering::Equal => Ok(head_state),
        Ordering::Greater => {
            if slot > head_state.slot() + T::EthSpec::slots_per_epoch() {
                warn!(
                    head_slot = %head_state.slot(),
                    request_slot = %slot,
                    "Skipping more than an epoch"
                )
            }

            let head_state_slot = head_state.slot();
            let mut state = head_state;

            let skip_state_root = match config {
                StateSkipConfig::WithStateRoots => None,
                StateSkipConfig::WithoutStateRoots => Some(Hash256::zero()),
            };

            while state.slot() < slot {
                // Note: supplying some `state_root` when it is known would be a cheap and easy
                // optimization.
                match per_slot_processing(&mut state, skip_state_root, spec) {
                    Ok(_) => (),
                    Err(e) => {
                        warn!(
                            error = ?e,
                            head_slot= %head_state_slot,
                            requested_slot = %slot,
                            "Unable to load state at slot"
                        );
                        return Err(Error::NoStateForSlot(slot));
                    }
                };
            }
            Ok(state)
        }
        Ordering::Less => {
            let state_root = process_results(
                forwards_iter_state_roots_until(store, canonical_head, slot, slot)?,
                |iter| {
                    iter.take_while(|(_, current_slot)| *current_slot >= slot)
                        .find(|(_, current_slot)| *current_slot == slot)
                        .map(|(root, _slot)| root)
                },
            )?
            .ok_or(Error::NoStateForSlot(slot))?;

            // This branch is mostly reached from the HTTP API when doing analysis, or in niche
            // situations when producing a block. In the HTTP API case we assume the user wants
            // to cache states so that future calls are faster, and that if the cache is
            // struggling due to non-finality that they will dial down inessential calls. In the
            // block proposal case we want to cache the state so that we can process the block
            // quickly after it has been signed.
            Ok(store
                .get_state(&state_root, Some(slot), true)?
                .ok_or(Error::NoStateForSlot(slot))?)
        }
    }
}

/// Returns the block canonical root of the current canonical chain at a given
/// slot, starting from the given state.
///
/// Returns `None` if the given slot doesn't exist in the chain.
pub fn root_at_slot_from_state<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    target_slot: Slot,
    beacon_block_root: Hash256,
    state: &BeaconState<T::EthSpec>,
) -> Result<Option<Hash256>, Error> {
    let iter = BlockRootsIterator::new(store, state);
    let iter_with_head = std::iter::once(Ok((beacon_block_root, state.slot())))
        .chain(iter)
        .map(|result| result.map_err(|e| e.into()));

    process_results(iter_with_head, |mut iter| {
        iter.find(|(_, slot)| *slot == target_slot)
            .map(|(root, _)| root)
    })
}

/// Returns the `BeaconState` at the current wall-clock slot.
pub fn wall_clock_state<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
) -> Result<BeaconState<T::EthSpec>, Error> {
    let slot = current_slot(slot_clock)?;
    state_at_slot(
        store,
        canonical_head,
        spec,
        slot,
        StateSkipConfig::WithStateRoots,
    )
}

/// Checks if a block is finalized.
pub fn is_finalized_block<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_block_root: Hash256,
    block_root: &Hash256,
    block_slot: Slot,
) -> Result<bool, Error> {
    let finalized_slot = canonical_head
        .cached_head()
        .finalized_checkpoint()
        .epoch
        .start_slot(T::EthSpec::slots_per_epoch());
    let is_canonical = block_root_at_slot(
        store,
        canonical_head,
        spec,
        slot_clock,
        genesis_block_root,
        block_slot,
        WhenSlotSkipped::None,
    )?
    .is_some_and(|canonical_root| block_root == &canonical_root);
    Ok(block_slot <= finalized_slot && is_canonical)
}

/// Checks if a state is finalized.
pub fn is_finalized_state<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_state_root: Hash256,
    state_root: &Hash256,
    state_slot: Slot,
) -> Result<bool, Error> {
    state_finalization_and_canonicity(
        store,
        canonical_head,
        spec,
        slot_clock,
        genesis_state_root,
        state_root,
        state_slot,
    )
    .map(FinalizationAndCanonicity::is_finalized)
}

/// Fetch the finalization and canonicity status of the state with `state_root`.
pub fn state_finalization_and_canonicity<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    canonical_head: &CanonicalHead<T>,
    spec: &ChainSpec,
    slot_clock: &T::SlotClock,
    genesis_state_root: Hash256,
    state_root: &Hash256,
    state_slot: Slot,
) -> Result<FinalizationAndCanonicity, Error> {
    let finalized_slot = canonical_head
        .cached_head()
        .finalized_checkpoint()
        .epoch
        .start_slot(T::EthSpec::slots_per_epoch());
    let slot_is_finalized = state_slot <= finalized_slot;
    let canonical = state_root_at_slot(
        store,
        canonical_head,
        spec,
        slot_clock,
        genesis_state_root,
        state_slot,
    )?
    .is_some_and(|canonical_root| state_root == &canonical_root);
    Ok(FinalizationAndCanonicity {
        slot_is_finalized,
        canonical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{BeaconChainHarness, EphemeralHarnessType, test_spec};
    use bls::Keypair;
    use std::sync::Arc;
    use std::sync::LazyLock;
    use types::{ChainSpec, MinimalEthSpec};

    type E = MinimalEthSpec;

    const VALIDATOR_COUNT: usize = 48;

    static KEYPAIRS: LazyLock<Vec<Keypair>> =
        LazyLock::new(|| types::test_utils::generate_deterministic_keypairs(VALIDATOR_COUNT));

    fn get_harness(spec: Arc<ChainSpec>) -> BeaconChainHarness<EphemeralHarnessType<E>> {
        let harness = BeaconChainHarness::builder(MinimalEthSpec)
            .spec(spec)
            .keypairs(KEYPAIRS[..VALIDATOR_COUNT].to_vec())
            .fresh_ephemeral_store()
            .mock_execution_layer()
            .build();

        harness.advance_slot();
        harness
    }

    #[test]
    fn current_slot_returns_clock_time() {
        let spec = Arc::new(test_spec::<E>());
        let harness = get_harness(spec);

        // Advance the clock a few slots without producing blocks.
        harness.advance_slot();
        harness.advance_slot();

        let expected_slot = harness.chain.slot_clock.now().unwrap();
        let result = current_slot(&harness.chain.slot_clock).unwrap();
        assert_eq!(result, expected_slot);
    }

    #[tokio::test]
    async fn block_root_at_slot_returns_none_for_skip_slot() {
        let spec = Arc::new(test_spec::<E>());
        let harness = get_harness(spec);

        // Produce blocks at slots 1..=3.
        harness.extend_slots(3).await;

        // Create a skip slot at slot 4: advance the clock past slot 4 without
        // producing a block there, then produce a block at slot 5.
        harness.advance_slot();
        harness.advance_slot();
        harness.extend_slots(1).await;

        let skip_slot = Slot::new(4);

        // When we query the skip slot with WhenSlotSkipped::None, we get None.
        let result = block_root_at_slot(
            &harness.chain.store,
            &harness.chain.canonical_head,
            &harness.chain.spec,
            &harness.chain.slot_clock,
            harness.chain.genesis_block_root,
            skip_slot,
            WhenSlotSkipped::None,
        )
        .unwrap();

        assert_eq!(result, None, "skip slot should return None");
    }

    #[tokio::test]
    async fn block_root_at_slot_returns_prev_for_skip_slot() {
        let spec = Arc::new(test_spec::<E>());
        let harness = get_harness(spec);

        // Produce blocks at slots 1..=3.
        harness.extend_slots(3).await;

        // Get the block root at slot 3 (the last produced block before the skip).
        let prev_root = block_root_at_slot(
            &harness.chain.store,
            &harness.chain.canonical_head,
            &harness.chain.spec,
            &harness.chain.slot_clock,
            harness.chain.genesis_block_root,
            Slot::new(3),
            WhenSlotSkipped::None,
        )
        .unwrap()
        .expect("slot 3 should have a block");

        // Create a skip slot at slot 4: advance past it, then produce a block at slot 5.
        harness.advance_slot();
        harness.advance_slot();
        harness.extend_slots(1).await;

        let skip_slot = Slot::new(4);

        // When we query the skip slot with WhenSlotSkipped::Prev, we get the root
        // from the previous non-skipped slot.
        let result = block_root_at_slot(
            &harness.chain.store,
            &harness.chain.canonical_head,
            &harness.chain.spec,
            &harness.chain.slot_clock,
            harness.chain.genesis_block_root,
            skip_slot,
            WhenSlotSkipped::Prev,
        )
        .unwrap()
        .expect("WhenSlotSkipped::Prev should return Some");

        assert_eq!(
            result, prev_root,
            "skip slot with Prev should return the previous non-skipped block root"
        );
    }

    #[tokio::test]
    async fn state_root_at_slot_returns_correct_root() {
        let spec = Arc::new(test_spec::<E>());
        let harness = get_harness(spec);

        // Produce a few blocks so we have state roots to query.
        harness.extend_slots(3).await;

        let query_slot = Slot::new(2);

        let result = state_root_at_slot(
            &harness.chain.store,
            &harness.chain.canonical_head,
            &harness.chain.spec,
            &harness.chain.slot_clock,
            harness.chain.genesis_state_root,
            query_slot,
        )
        .unwrap();

        assert!(
            result.is_some(),
            "state root should exist for a slot with a block"
        );

        // Verify the returned root matches the state root stored in the head state.
        let head_state = &harness
            .chain
            .canonical_head
            .cached_head()
            .snapshot
            .beacon_state;
        let expected_root = *head_state.get_state_root(query_slot).unwrap();
        assert_eq!(result.unwrap(), expected_root);
    }
}
