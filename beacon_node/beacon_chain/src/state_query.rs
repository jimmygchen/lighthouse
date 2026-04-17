use crate::beacon_block_streamer::{BeaconBlockStreamer, CheckCaches};
use crate::beacon_chain::{
    BeaconChain, BeaconChainTypes, BeaconStore, BlockProcessStatus, FinalizationAndCanonicity,
    StateSkipConfig, WhenSlotSkipped,
};
use crate::errors::BeaconChainError as Error;
use crate::migrate::ManualFinalizationNotification;
use crate::{BeaconChainError, BeaconSnapshot, metrics};
use itertools::process_results;
use itertools::Itertools;
use state_processing::per_slot_processing;
use std::cmp::Ordering;
use std::sync::Arc;
use store::iter::{BlockRootsIterator, StateRootsIterator};
use store::{DatabaseBlock, HotStateSummary};
use tokio_stream::Stream;
use tracing::{debug, instrument, warn};
use types::*;

impl<T: BeaconChainTypes> BeaconChain<T> {
    /// Checks if a block is finalized.
    /// The finalization check is done with the block slot. The block root is used to verify that
    /// the finalized slot is in the canonical chain.
    pub fn is_finalized_block(
        &self,
        block_root: &Hash256,
        block_slot: Slot,
    ) -> Result<bool, Error> {
        let finalized_slot = self
            .canonical_head
            .cached_head()
            .finalized_checkpoint()
            .epoch
            .start_slot(T::EthSpec::slots_per_epoch());
        let is_canonical = self
            .block_root_at_slot(block_slot, WhenSlotSkipped::None)?
            .is_some_and(|canonical_root| block_root == &canonical_root);
        Ok(block_slot <= finalized_slot && is_canonical)
    }

    /// Checks if a state is finalized.
    /// The finalization check is done with the slot. The state root is used to verify that
    /// the finalized state is in the canonical chain.
    pub fn is_finalized_state(
        &self,
        state_root: &Hash256,
        state_slot: Slot,
    ) -> Result<bool, Error> {
        self.state_finalization_and_canonicity(state_root, state_slot)
            .map(FinalizationAndCanonicity::is_finalized)
    }

    /// Fetch the finalization and canonicity status of the state with `state_root`.
    pub fn state_finalization_and_canonicity(
        &self,
        state_root: &Hash256,
        state_slot: Slot,
    ) -> Result<FinalizationAndCanonicity, Error> {
        let finalized_slot = self
            .canonical_head
            .cached_head()
            .finalized_checkpoint()
            .epoch
            .start_slot(T::EthSpec::slots_per_epoch());
        let slot_is_finalized = state_slot <= finalized_slot;
        let canonical = self
            .state_root_at_slot(state_slot)?
            .is_some_and(|canonical_root| state_root == &canonical_root);
        Ok(FinalizationAndCanonicity {
            slot_is_finalized,
            canonical,
        })
    }

    /// Iterates across all `(block_root, slot)` pairs from `start_slot`
    /// to the head of the chain (inclusive).
    ///
    /// ## Notes
    ///
    /// - `slot` always increases by `1`.
    /// - Skipped slots contain the root of the closest prior
    ///   non-skipped slot (identical to the way they are stored in `state.block_roots`).
    /// - Iterator returns `(Hash256, Slot)`.
    ///
    /// Will return a `BlockOutOfRange` error if the requested start slot is before the period of
    /// history for which we have blocks stored. See `get_oldest_block_slot`.
    pub fn forwards_iter_block_roots(
        &self,
        start_slot: Slot,
    ) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + '_, Error> {
        let oldest_block_slot = self.store.get_oldest_block_slot();
        if start_slot < oldest_block_slot {
            return Err(Error::HistoricalBlockOutOfRange {
                slot: start_slot,
                oldest_block_slot,
            });
        }

        let local_head = self.head_snapshot();

        let iter = self.store.forwards_block_roots_iterator(
            start_slot,
            local_head.beacon_state.clone(),
            local_head.beacon_block_root,
        )?;

        Ok(iter.map(|result| result.map_err(Into::into)))
    }

    /// Even more efficient variant of `forwards_iter_block_roots` that will avoid cloning the head
    /// state if it isn't required for the requested range of blocks.
    /// The range [start_slot, end_slot] is inclusive (ie `start_slot <= end_slot`)
    pub fn forwards_iter_block_roots_until(
        &self,
        start_slot: Slot,
        end_slot: Slot,
    ) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + '_, Error> {
        let oldest_block_slot = self.store.get_oldest_block_slot();
        if start_slot < oldest_block_slot {
            return Err(Error::HistoricalBlockOutOfRange {
                slot: start_slot,
                oldest_block_slot,
            });
        }

        self.with_head(move |head| {
            let iter =
                self.store
                    .forwards_block_roots_iterator_until(start_slot, end_slot, || {
                        Ok((head.beacon_state.clone(), head.beacon_block_root))
                    })?;
            Ok(iter
                .map(|result| result.map_err(Into::into))
                .take_while(move |result| {
                    result.as_ref().map_or(true, |(_, slot)| *slot <= end_slot)
                }))
        })
    }

    /// Traverse backwards from `block_root` to find the block roots of its ancestors.
    ///
    /// ## Notes
    ///
    /// - `slot` always decreases by `1`.
    /// - Skipped slots contain the root of the closest prior
    ///   non-skipped slot (identical to the way they are stored in `state.block_roots`) .
    /// - Iterator returns `(Hash256, Slot)`.
    /// - The provided `block_root` is included as the first item in the iterator.
    pub fn rev_iter_block_roots_from(
        &self,
        block_root: Hash256,
    ) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + '_, Error> {
        let block = self
            .get_blinded_block(&block_root)?
            .ok_or(Error::MissingBeaconBlock(block_root))?;
        // This method is only used in tests, so we may as well cache states to make CI go brr.
        // TODO(release-v7) move this method out of beacon chain and into `store_tests`` or something equivalent.
        let state = self
            .get_state(&block.state_root(), Some(block.slot()), true)?
            .ok_or_else(|| Error::MissingBeaconState(block.state_root()))?;
        let iter = BlockRootsIterator::owned(&self.store, state);
        Ok(std::iter::once(Ok((block_root, block.slot())))
            .chain(iter)
            .map(|result| result.map_err(|e| e.into())))
    }

    /// Iterates backwards across all `(state_root, slot)` pairs starting from
    /// an arbitrary `BeaconState` to the earliest reachable ancestor (may or may not be genesis).
    ///
    /// ## Notes
    ///
    /// - `slot` always decreases by `1`.
    /// - Iterator returns `(Hash256, Slot)`.
    /// - As this iterator starts at the `head` of the chain (viz., the best block), the first slot
    ///   returned may be earlier than the wall-clock slot.
    pub fn rev_iter_state_roots_from<'a>(
        &'a self,
        state_root: Hash256,
        state: &'a BeaconState<T::EthSpec>,
    ) -> impl Iterator<Item = Result<(Hash256, Slot), Error>> + 'a {
        std::iter::once(Ok((state_root, state.slot())))
            .chain(StateRootsIterator::new(&self.store, state))
            .map(|result| result.map_err(Into::into))
    }

    /// Iterates across all `(state_root, slot)` pairs from `start_slot`
    /// to the head of the chain (inclusive).
    ///
    /// ## Notes
    ///
    /// - `slot` always increases by `1`.
    /// - Iterator returns `(Hash256, Slot)`.
    pub fn forwards_iter_state_roots(
        &self,
        start_slot: Slot,
    ) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + '_, Error> {
        let local_head = self.head_snapshot();

        let iter = self.store.forwards_state_roots_iterator(
            start_slot,
            local_head.beacon_state_root(),
            local_head.beacon_state.clone(),
        )?;

        Ok(iter.map(|result| result.map_err(Into::into)))
    }

    /// Super-efficient forwards state roots iterator that avoids cloning the head if the state
    /// roots lie entirely within the freezer database.
    ///
    /// The iterator returned will include roots for `start_slot..=end_slot`, i.e.  it
    /// is endpoint inclusive.
    pub fn forwards_iter_state_roots_until(
        &self,
        start_slot: Slot,
        end_slot: Slot,
    ) -> Result<impl Iterator<Item = Result<(Hash256, Slot), Error>> + '_, Error> {
        self.with_head(move |head| {
            let iter =
                self.store
                    .forwards_state_roots_iterator_until(start_slot, end_slot, || {
                        Ok((head.beacon_state.clone(), head.beacon_state_root()))
                    })?;
            Ok(iter
                .map(|result| result.map_err(Into::into))
                .take_while(move |result| {
                    result.as_ref().map_or(true, |(_, slot)| *slot <= end_slot)
                }))
        })
    }

    /// Returns the block at the given slot, if any. Only returns blocks in the canonical chain.
    ///
    /// Use the `skips` parameter to define the behaviour when `request_slot` is a skipped slot.
    ///
    /// ## Errors
    ///
    /// May return a database error.
    pub fn block_at_slot(
        &self,
        request_slot: Slot,
        skips: WhenSlotSkipped,
    ) -> Result<Option<SignedBlindedBeaconBlock<T::EthSpec>>, Error> {
        let root = self.block_root_at_slot(request_slot, skips)?;

        if let Some(block_root) = root {
            Ok(self.store.get_blinded_block(&block_root)?)
        } else {
            Ok(None)
        }
    }

    /// Returns the state root at the given slot, if any. Only returns state roots in the canonical chain.
    ///
    /// ## Errors
    ///
    /// May return a database error.
    pub fn state_root_at_slot(&self, request_slot: Slot) -> Result<Option<Hash256>, Error> {
        if request_slot == self.spec.genesis_slot {
            return Ok(Some(self.genesis_state_root));
        } else if request_slot > self.slot_clock.now().ok_or(Error::UnableToReadSlot)? {
            return Ok(None);
        }

        // Check limits w.r.t historic state bounds.
        let (historic_lower_limit, historic_upper_limit) = self.store.get_historic_state_limits();
        if request_slot > historic_lower_limit && request_slot < historic_upper_limit {
            return Ok(None);
        }

        // Fast-path for the split slot (which usually corresponds to the finalized slot).
        // Post-Gloas, the split state root is always the Pending root but the canonical state root
        // at the finalized slot may be the Full root (from the state_roots vector). Skip the
        // fast-path for Gloas to ensure consistency with the forwards state root iterator.
        // TODO(gloas): revisit this if spec changes to finalize payload status.
        let split = self.store.get_split_info();
        if request_slot == split.slot
            && !self
                .spec
                .fork_name_at_slot::<T::EthSpec>(split.slot)
                .gloas_enabled()
        {
            return Ok(Some(split.state_root));
        }

        // Try an optimized path of reading the root directly from the head state.
        let fast_lookup: Option<Hash256> = self.with_head(|head| {
            if head.beacon_block.slot() <= request_slot {
                // Return the head state root if all slots between the request and the head are skipped.
                Ok(Some(head.beacon_state_root()))
            } else if let Ok(root) = head.beacon_state.get_state_root(request_slot) {
                // Return the root if it's easily accessible from the head state.
                Ok(Some(*root))
            } else {
                // Fast lookup is not possible.
                Ok::<_, Error>(None)
            }
        })?;

        if let Some(root) = fast_lookup {
            return Ok(Some(root));
        }

        process_results(
            self.forwards_iter_state_roots_until(request_slot, request_slot)?,
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

    /// Returns the block root at the given slot, if any. Only returns roots in the canonical chain.
    ///
    /// ## Notes
    ///
    /// - Use the `skips` parameter to define the behaviour when `request_slot` is a skipped slot.
    /// - Returns `Ok(None)` for any slot higher than the current wall-clock slot, or less than
    ///   the oldest known block slot.
    pub fn block_root_at_slot(
        &self,
        request_slot: Slot,
        skips: WhenSlotSkipped,
    ) -> Result<Option<Hash256>, Error> {
        match skips {
            WhenSlotSkipped::None => self.block_root_at_slot_skips_none(request_slot),
            WhenSlotSkipped::Prev => self.block_root_at_slot_skips_prev(request_slot),
        }
        .or_else(|e| match e {
            Error::HistoricalBlockOutOfRange { .. } => Ok(None),
            e => Err(e),
        })
    }

    /// Returns the block root at the given slot, if any. Only returns roots in the canonical chain.
    ///
    /// ## Notes
    ///
    /// - Returns `Ok(None)` if the given `Slot` was skipped.
    /// - Returns `Ok(None)` for any slot higher than the current wall-clock slot.
    ///
    /// ## Errors
    ///
    /// May return a database error.
    fn block_root_at_slot_skips_none(&self, request_slot: Slot) -> Result<Option<Hash256>, Error> {
        if request_slot == self.spec.genesis_slot {
            return Ok(Some(self.genesis_block_root));
        } else if request_slot > self.slot_clock.now().ok_or(Error::UnableToReadSlot)? {
            return Ok(None);
        }

        let prev_slot = request_slot.saturating_sub(1_u64);

        // Try an optimized path of reading the root directly from the head state.
        let fast_lookup: Option<Option<Hash256>> = self.with_head(|head| {
            let state = &head.beacon_state;

            // Try find the root for the `request_slot`.
            let request_root_opt = match state.slot().cmp(&request_slot) {
                // It's always a skip slot if the head is less than the request slot, return early.
                Ordering::Less => return Ok(Some(None)),
                // The request slot is the head slot.
                Ordering::Equal => Some(head.beacon_block_root),
                // Try find the request slot in the state.
                Ordering::Greater => state.get_block_root(request_slot).ok().copied(),
            };

            if let Some(request_root) = request_root_opt
                && let Ok(prev_root) = state.get_block_root(prev_slot)
            {
                return Ok(Some((*prev_root != request_root).then_some(request_root)));
            }

            // Fast lookup is not possible.
            Ok::<_, Error>(None)
        })?;
        if let Some(root_opt) = fast_lookup {
            return Ok(root_opt);
        }

        // Do not try to access the previous slot if it's older than the oldest block root
        // stored in the database. Instead, load just the block root at `oldest_block_slot`,
        // under the assumption that the `oldest_block_slot` *is not* a skipped slot (should be
        // true because it is set by the oldest *block*).
        if request_slot == self.store.get_anchor_info().oldest_block_slot {
            return self.block_root_at_slot_skips_prev(request_slot);
        }

        if let Some(((prev_root, _), (curr_root, curr_slot))) = process_results(
            self.forwards_iter_block_roots_until(prev_slot, request_slot)?,
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

    /// Returns the block root at the given slot, if any. Only returns roots in the canonical chain.
    ///
    /// ## Notes
    ///
    /// - Returns the root at the previous non-skipped slot if the given `Slot` was skipped.
    /// - Returns `Ok(None)` for any slot higher than the current wall-clock slot.
    ///
    /// ## Errors
    ///
    /// May return a database error.
    fn block_root_at_slot_skips_prev(&self, request_slot: Slot) -> Result<Option<Hash256>, Error> {
        if request_slot == self.spec.genesis_slot {
            return Ok(Some(self.genesis_block_root));
        } else if request_slot > self.slot_clock.now().ok_or(Error::UnableToReadSlot)? {
            return Ok(None);
        }

        // Try an optimized path of reading the root directly from the head state.
        let fast_lookup: Option<Hash256> = self.with_head(|head| {
            if head.beacon_block.slot() <= request_slot {
                // Return the head root if all slots between the request and the head are skipped.
                Ok(Some(head.beacon_block_root))
            } else if let Ok(root) = head.beacon_state.get_block_root(request_slot) {
                // Return the root if it's easily accessible from the head state.
                Ok(Some(*root))
            } else {
                // Fast lookup is not possible.
                Ok::<_, Error>(None)
            }
        })?;
        if let Some(root) = fast_lookup {
            return Ok(Some(root));
        }

        process_results(
            self.forwards_iter_block_roots_until(request_slot, request_slot)?,
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

    /// Returns the block at the given root, if any.
    ///
    /// Will also check the early attester cache for the block. Because of this, there's no
    /// guarantee that a block returned from this function has a `BeaconState` available in
    /// `self.store`. The expected use for this function is *only* for returning blocks requested
    /// from P2P peers.
    ///
    /// ## Errors
    ///
    /// May return a database error.
    #[allow(clippy::type_complexity)]
    pub fn get_blocks_checking_caches(
        self: &Arc<Self>,
        block_roots: Vec<Hash256>,
    ) -> Result<
        impl Stream<
            Item = (
                Hash256,
                Arc<Result<Option<Arc<SignedBeaconBlock<T::EthSpec>>>, Error>>,
            ),
        >,
        Error,
    > {
        Ok(BeaconBlockStreamer::<T>::new(self, CheckCaches::Yes)?.launch_stream(block_roots))
    }

    #[allow(clippy::type_complexity)]
    pub fn get_blocks(
        self: &Arc<Self>,
        block_roots: Vec<Hash256>,
    ) -> Result<
        impl Stream<
            Item = (
                Hash256,
                Arc<Result<Option<Arc<SignedBeaconBlock<T::EthSpec>>>, Error>>,
            ),
        >,
        Error,
    > {
        Ok(BeaconBlockStreamer::<T>::new(self, CheckCaches::No)?.launch_stream(block_roots))
    }

    /// Returns the block at the given root, if any.
    ///
    /// ## Errors
    ///
    /// May return a database error.
    pub async fn get_block(
        &self,
        block_root: &Hash256,
    ) -> Result<Option<SignedBeaconBlock<T::EthSpec>>, Error> {
        // Load block from database, returning immediately if we have the full block w payload
        // stored.
        let blinded_block = match self.store.try_get_full_block(block_root)? {
            Some(DatabaseBlock::Full(block)) => return Ok(Some(block)),
            Some(DatabaseBlock::Blinded(block)) => block,
            None => return Ok(None),
        };
        let fork = blinded_block.fork_name(&self.spec)?;

        // If we only have a blinded block, load the execution payload from the EL.
        let block_message = blinded_block.message();
        let execution_payload_header = block_message
            .execution_payload()
            .map_err(|_| Error::BlockVariantLacksExecutionPayload(*block_root))?
            .to_execution_payload_header();

        let exec_block_hash = execution_payload_header.block_hash();

        let execution_payload = self
            .execution_layer
            .as_ref()
            .ok_or(Error::ExecutionLayerMissing)?
            .get_payload_for_header(&execution_payload_header, fork)
            .await
            .map_err(|e| {
                Error::ExecutionLayerErrorPayloadReconstruction(exec_block_hash, Box::new(e))
            })?
            .ok_or(Error::BlockHashMissingFromExecutionLayer(exec_block_hash))?;

        // Verify payload integrity.
        let header_from_payload = ExecutionPayloadHeader::from(execution_payload.to_ref());
        if header_from_payload != execution_payload_header {
            for txn in execution_payload.transactions() {
                debug!(
                    bytes = format!("0x{}", hex::encode(&**txn)),
                    "Reconstructed txn"
                );
            }

            return Err(Error::InconsistentPayloadReconstructed {
                slot: blinded_block.slot(),
                exec_block_hash,
                canonical_transactions_root: execution_payload_header.transactions_root(),
                reconstructed_transactions_root: header_from_payload.transactions_root(),
            });
        }

        // Add the payload to the block to form a full block.
        blinded_block
            .try_into_full_block(Some(execution_payload))
            .ok_or(Error::AddPayloadLogicError)
            .map(Some)
    }

    pub fn get_blinded_block(
        &self,
        block_root: &Hash256,
    ) -> Result<Option<SignedBlindedBeaconBlock<T::EthSpec>>, Error> {
        Ok(self.store.get_blinded_block(block_root)?)
    }

    /// Return the status of a block as it progresses through the various caches of the beacon
    /// chain. Used by sync to learn the status of a block and prevent repeated downloads /
    /// processing attempts.
    pub fn get_block_process_status(&self, block_root: &Hash256) -> BlockProcessStatus<T::EthSpec> {
        if let Some(cached_block) = self.data_availability_checker.get_cached_block(block_root) {
            return cached_block;
        }

        BlockProcessStatus::Unknown
    }

    /// Returns the state at the given root, if any.
    ///
    /// ## Errors
    ///
    /// May return a database error.
    pub fn get_state(
        &self,
        state_root: &Hash256,
        slot: Option<Slot>,
        update_cache: bool,
    ) -> Result<Option<BeaconState<T::EthSpec>>, Error> {
        Ok(self.store.get_state(state_root, slot, update_cache)?)
    }

    /// Returns the current heads of the `BeaconChain`. For the canonical head, see `Self::head`.
    ///
    /// Returns `(block_root, block_slot)`.
    pub fn heads(&self) -> Vec<(Hash256, Slot)> {
        let fork_choice = self.canonical_head.fork_choice_read_lock();
        fork_choice
            .proto_array()
            .heads_descended_from_finalization::<T::EthSpec>(fork_choice.finalized_checkpoint())
            .iter()
            .map(|node| (node.root(), node.slot()))
            .collect()
    }

    /// Returns the `BeaconState` at the given slot.
    ///
    /// Returns `None` when the state is not found in the database or there is an error skipping
    /// to a future state.
    #[instrument(level = "debug", skip_all)]
    pub fn state_at_slot(
        &self,
        slot: Slot,
        config: StateSkipConfig,
    ) -> Result<BeaconState<T::EthSpec>, Error> {
        let head_state = self.head_beacon_state_cloned();

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
                    match per_slot_processing(&mut state, skip_state_root, &self.spec) {
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
                let state_root =
                    process_results(self.forwards_iter_state_roots_until(slot, slot)?, |iter| {
                        iter.take_while(|(_, current_slot)| *current_slot >= slot)
                            .find(|(_, current_slot)| *current_slot == slot)
                            .map(|(root, _slot)| root)
                    })?
                    .ok_or(Error::NoStateForSlot(slot))?;

                // This branch is mostly reached from the HTTP API when doing analysis, or in niche
                // situations when producing a block. In the HTTP API case we assume the user wants
                // to cache states so that future calls are faster, and that if the cache is
                // struggling due to non-finality that they will dial down inessential calls. In the
                // block proposal case we want to cache the state so that we can process the block
                // quickly after it has been signed.
                Ok(self
                    .get_state(&state_root, Some(slot), true)?
                    .ok_or(Error::NoStateForSlot(slot))?)
            }
        }
    }

    /// Returns the `BeaconState` the current slot (viz., `self.slot()`).
    ///
    ///  - A reference to the head state (note: this keeps a read lock on the head, try to use
    ///    sparingly).
    ///  - The head state, but with skipped slots (for states later than the head).
    ///
    ///  Returns `None` when there is an error skipping to a future state or the slot clock cannot
    ///  be read.
    pub fn wall_clock_state(&self) -> Result<BeaconState<T::EthSpec>, Error> {
        self.state_at_slot(self.slot_clock.now().ok_or(Error::UnableToReadSlot)?, StateSkipConfig::WithStateRoots)
    }

    /// Returns the block canonical root of the current canonical chain at a given slot, starting from the given state.
    ///
    /// Returns `None` if the given slot doesn't exist in the chain.
    pub fn root_at_slot_from_state(
        &self,
        target_slot: Slot,
        beacon_block_root: Hash256,
        state: &BeaconState<T::EthSpec>,
    ) -> Result<Option<Hash256>, Error> {
        let iter = BlockRootsIterator::new(&self.store, state);
        let iter_with_head = std::iter::once(Ok((beacon_block_root, state.slot())))
            .chain(iter)
            .map(|result| result.map_err(|e| e.into()));

        process_results(iter_with_head, |mut iter| {
            iter.find(|(_, slot)| *slot == target_slot)
                .map(|(root, _)| root)
        })
    }

    pub fn manually_compact_database(&self) {
        self.store_migrator.process_manual_compaction();
    }

    pub fn manually_finalize_state(
        &self,
        state_root: Hash256,
        checkpoint: Checkpoint,
    ) -> Result<(), Error> {
        let HotStateSummary {
            slot,
            latest_block_root,
            ..
        } = self
            .store
            .load_hot_state_summary(&state_root)
            .map_err(BeaconChainError::DBError)?
            .ok_or(BeaconChainError::MissingHotStateSummary(state_root))?;

        if slot != checkpoint.epoch.start_slot(T::EthSpec::slots_per_epoch())
            || latest_block_root != *checkpoint.root
        {
            return Err(BeaconChainError::InvalidCheckpoint {
                state_root,
                checkpoint,
            });
        }

        let notif = ManualFinalizationNotification {
            state_root: state_root.into(),
            checkpoint,
        };

        self.store_migrator.process_manual_finalization(notif);
        Ok(())
    }

    /// Dumps the entire canonical chain, from the head to genesis to a vector for analysis.
    ///
    /// This could be a very expensive operation and should only be done in testing/analysis
    /// activities.
    ///
    /// This dump function previously used a backwards iterator but has been swapped to a forwards
    /// iterator as it allows for MUCH better caching and rebasing. Memory usage of some tests went
    /// from 5GB per test to 90MB.
    #[allow(clippy::type_complexity)]
    pub fn chain_dump(
        &self,
    ) -> Result<Vec<BeaconSnapshot<T::EthSpec, BlindedPayload<T::EthSpec>>>, Error> {
        self.chain_dump_from_slot(Slot::new(0))
    }

    /// As for `chain_dump` but dumping only the portion of the chain newer than `from_slot`.
    #[allow(clippy::type_complexity)]
    pub fn chain_dump_from_slot(
        &self,
        from_slot: Slot,
    ) -> Result<Vec<BeaconSnapshot<T::EthSpec, BlindedPayload<T::EthSpec>>>, Error> {
        let mut dump = vec![];

        let mut prev_block_root = None;
        let mut prev_beacon_state = None;

        // Collect all blocks.
        let mut blocks = vec![];

        for res in self.forwards_iter_block_roots(from_slot)? {
            let (beacon_block_root, _) = res?;

            // Do not include snapshots at skipped slots.
            if Some(beacon_block_root) == prev_block_root {
                continue;
            }
            prev_block_root = Some(beacon_block_root);

            let beacon_block = self
                .store
                .get_blinded_block(&beacon_block_root)?
                .ok_or_else(|| {
                    Error::DBInconsistent(format!("Missing block {}", beacon_block_root))
                })?;
            blocks.push((beacon_block_root, Arc::new(beacon_block)));
        }

        // Collect states, using the next blocks to determine if states are full (have Gloas
        // payloads).
        for (i, (block_root, block)) in blocks.iter().enumerate() {
            let (opt_envelope, state_root) = if block.fork_name_unchecked().gloas_enabled() {
                let opt_envelope = self.store.get_payload_envelope(block_root)?.map(Arc::new);

                if let Some((_, next_block)) = blocks.get(i + 1) {
                    let block_hash = block.payload_bid_block_hash()?;
                    if next_block.is_parent_block_full(block_hash) {
                        let envelope = opt_envelope.ok_or_else(|| {
                            Error::DBInconsistent(format!("Missing envelope {block_root:?}"))
                        })?;
                        let state_root = envelope.message.state_root;
                        (Some(envelope), state_root)
                    } else {
                        (None, block.state_root())
                    }
                } else {
                    // Last block in the sequence: use canonical head to determine
                    // whether the payload is canonical.
                    let head = self.canonical_head.cached_head();
                    assert_eq!(head.head_block_root(), *block_root);
                    let payload_received = head.head_payload_status().as_state_payload_status()
                        == StatePayloadStatus::Full;
                    if payload_received {
                        let envelope = opt_envelope.ok_or_else(|| {
                            Error::DBInconsistent(format!("Missing envelope {block_root:?}"))
                        })?;
                        let state_root = envelope.message.state_root;
                        (Some(envelope), state_root)
                    } else {
                        (None, block.state_root())
                    }
                }
            } else {
                (None, block.state_root())
            };

            let mut beacon_state = self
                .store
                .get_state(&state_root, Some(block.slot()), true)?
                .ok_or_else(|| Error::DBInconsistent(format!("Missing state {:?}", state_root)))?;

            // This beacon state might come from the freezer DB, which means it could have pending
            // updates or lots of untethered memory. We rebase it on the previous state in order to
            // address this.
            beacon_state.apply_pending_mutations()?;
            if let Some(prev) = prev_beacon_state {
                beacon_state.rebase_on(&prev, &self.spec)?;
            }
            beacon_state.build_caches(&self.spec)?;
            prev_beacon_state = Some(beacon_state.clone());

            let snapshot = BeaconSnapshot {
                beacon_block: block.clone(),
                execution_envelope: opt_envelope,
                beacon_block_root: *block_root,
                beacon_state,
            };
            dump.push(snapshot);
        }

        Ok(dump)
    }
}
