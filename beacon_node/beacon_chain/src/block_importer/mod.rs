//! Block import, chain segment processing, blob/data column processing, and availability methods.
//!
//! `BlockImporter<T>` owns the subsystems required to import blocks, blobs and data columns. It
//! holds `Arc`-shared handles to every piece of state it accesses directly (store, spec, slot
//! clock, canonical head, attestation manager, etc.). For cross-module verification helpers that
//! still take `&BeaconChain<T>`, a `Weak<BeaconChain<T>>` back-reference is installed
//! post-construction by the builder. The `Weak` avoids a reference cycle, allowing proper
//! cleanup in tests.

#[cfg(test)]
mod tests;

use crate::attestation_manager::AttestationManager;
use crate::beacon_chain::BeaconStore;
use crate::beacon_chain::{BeaconChainTypes, BeaconForkChoice};
use crate::blob_verification::GossipVerifiedBlob;
use crate::block_times_cache::BlockTimesCache;
use crate::block_verification::{
    BlockError, ExecutionPendingBlock, GossipVerifiedBlock, IntoExecutionPendingBlock,
    check_block_is_finalized_checkpoint_or_descendant, check_block_relevancy,
    signature_verify_chain_segment, verify_header_signature,
};
use crate::block_verification_types::{
    AsBlock, AvailableExecutedBlock, BlockImportData, ExecutedBlock, RangeSyncBlock,
};
use crate::canonical_head::CanonicalHead;

/// Alias to appease clippy.
pub(crate) type HashBlockTuple<E> = (Hash256, RangeSyncBlock<E>);
use crate::data_availability_checker::{
    Availability, AvailabilityCheckError, AvailableBlock, DataAvailabilityChecker,
    DataColumnReconstructionResult,
};
use crate::data_availability_manager::AvailabilityProcessingStatus;
use crate::data_availability_manager::DataAvailabilityManager;
use crate::data_column_verification::GossipVerifiedDataColumn;
use crate::errors::BeaconChainError as Error;
use crate::events::ServerSentEventHandler;
use crate::execution_payload::NotifyExecutionLayer;
use crate::fetch_blobs::EngineGetBlobsOutput;
use crate::observed_aggregates::Error as AttestationObservationError;
use crate::observed_block_producers::ObservedBlockProducers;
use crate::observed_data_sidecars::ObservedDataSidecars;
use crate::observed_slashable::ObservedSlashable;
use crate::validator_monitor::{
    HISTORIC_EPOCHS as VALIDATOR_MONITOR_HISTORIC_EPOCHS, ValidatorMonitor, get_slot_delay_ms,
};
use crate::{
    AvailabilityPendingExecutedBlock, BeaconChain, BeaconChainError, ChainConfig, metrics,
};
use eth2::types::{EventKind, SseBlobSidecar, SseBlock, SseDataColumnSidecar, SseHead};
use fork_choice::{PayloadVerificationStatus, ResetPayloadStatuses};
use futures::channel::mpsc::Sender;
use itertools::Itertools;
use logging::crit;
use parking_lot::{RwLock, RwLockWriteGuard};
use slasher::Slasher;
use slot_clock::SlotClock;
use state_processing::ConsensusContext;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use store::StoreOp;
use task_executor::{RayonPoolType, ShutdownReason, TaskExecutor};
use tracing::{debug, debug_span, error, info, info_span, instrument, warn};
use types::*;

/// Defines how old a block can be before it's no longer a candidate for the early attester cache.
pub(crate) const EARLY_ATTESTER_CACHE_HISTORIC_SLOTS: u64 = 4;

/// The result of a chain segment processing.
pub enum ChainSegmentResult {
    /// Processing this chain segment finished successfully.
    Successful {
        imported_blocks: Vec<(Hash256, Slot)>,
    },
    /// There was an error processing this chain segment. Before the error, some blocks could
    /// have been imported.
    Failed {
        imported_blocks: Vec<(Hash256, Slot)>,
        error: BlockError,
    },
}

impl ChainSegmentResult {
    pub fn into_block_error(self) -> Result<(), BlockError> {
        match self {
            ChainSegmentResult::Failed { error, .. } => Err(error),
            ChainSegmentResult::Successful { .. } => Ok(()),
        }
    }
}

pub enum BlockProcessStatus<E: EthSpec> {
    /// Block is not in any pre-import cache. Block may be in the data-base or in the fork-choice.
    Unknown,
    /// Block is currently processing but not yet validated.
    NotValidated(Arc<SignedBeaconBlock<E>>, BlockImportSource),
    /// Block is fully valid, but not yet imported. It's cached in the da_checker while awaiting
    /// missing block components.
    ExecutionValidated(Arc<SignedBeaconBlock<E>>),
}

pub type LightClientProducerEvent<T> = (Hash256, Slot, SyncAggregate<T>);

/// Gets the `LightClientBootstrap` object for a requested block root.
#[allow(clippy::type_complexity)]
pub fn get_light_client_bootstrap<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    block_root: &Hash256,
) -> Result<Option<(LightClientBootstrap<T::EthSpec>, ForkName)>, Error> {
    let head_state = &chain.canonical_head.cached_head().snapshot.beacon_state;
    let finalized_period = head_state
        .finalized_checkpoint()
        .epoch
        .sync_committee_period(&chain.spec)?;
    chain
        .block_importer
        .light_client_server_cache
        .get_light_client_bootstrap(&chain.store, block_root, finalized_period, &chain.spec)
}

/// Verify that the weak subjectivity checkpoint is consistent with the finalized chain.
pub fn verify_weak_subjectivity_checkpoint<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    wss_checkpoint: Checkpoint,
    beacon_block_root: Hash256,
    state: &BeaconState<T::EthSpec>,
) -> Result<(), BeaconChainError> {
    let finalized_checkpoint = state.finalized_checkpoint();
    info!(
        weak_subjectivity_epoch = %wss_checkpoint.epoch,
        weak_subjectivity_root = ?wss_checkpoint.root,
        "Verifying the configured weak subjectivity checkpoint"
    );
    if wss_checkpoint.epoch == finalized_checkpoint.epoch
        && wss_checkpoint.root != finalized_checkpoint.root
    {
        crit!(
            weak_subjectivity_root = ?wss_checkpoint.root,
            finalized_checkpoint_root = ?finalized_checkpoint.root,
             "Root found at the specified checkpoint differs"
        );
        return Err(BeaconChainError::WeakSubjectivtyVerificationFailure);
    } else if wss_checkpoint.epoch < finalized_checkpoint.epoch {
        let slot = wss_checkpoint
            .epoch
            .start_slot(T::EthSpec::slots_per_epoch());

        match crate::state_query::root_at_slot_from_state::<T>(
            &chain.store,
            slot,
            beacon_block_root,
            state,
        )? {
            Some(root) => {
                if root != wss_checkpoint.root {
                    crit!(
                        weak_subjectivity_root = ?wss_checkpoint.root,
                        finalized_checkpoint_root = ?finalized_checkpoint.root,
                         "Root found at the specified checkpoint differs"
                    );
                    return Err(BeaconChainError::WeakSubjectivtyVerificationFailure);
                }
            }
            None => {
                crit!(
                    wss_checkpoint_slot = ?slot,
                    "The root at the start slot of the given epoch could not be found"
                );
                return Err(BeaconChainError::WeakSubjectivtyVerificationFailure);
            }
        }
    }
    Ok(())
}

/// Handles block, blob, and data-column import into the beacon chain.
///
/// Owns injected `Arc` handles to every subsystem it reaches for directly, including the
/// canonical head, attestation manager, observed slashable cache, validator monitor,
/// optional event handler, data availability manager, store, spec, and various caches.
///
/// For cross-module verification helpers that still take `&BeaconChain<T>`
/// (`check_block_relevancy`, `signature_verify_chain_segment`, `GossipVerifiedBlock::new`,
/// `IntoExecutionPendingBlock::into_execution_pending_block`,
/// `check_block_is_finalized_checkpoint_or_descendant`, `verify_weak_subjectivity_checkpoint`,
/// `verify_header_signature`, `get_blobs_or_columns_store_op`, `state_at_slot`), a
/// `Weak<BeaconChain<T>>` back-reference is installed by the builder post-construction.
/// The `Weak` avoids a reference cycle, allowing proper cleanup in tests. Rewriting those
/// helper signatures to take only the dependencies they need would let us drop the
/// back-reference entirely.
pub struct BlockImporter<T: BeaconChainTypes> {
    // Arc-held subsystems cloned at construction.
    pub(crate) spec: Arc<ChainSpec>,
    pub(crate) store: BeaconStore<T>,
    pub(crate) data_availability_checker: Arc<DataAvailabilityChecker<T>>,
    pub(crate) data_availability_manager: Arc<DataAvailabilityManager<T>>,
    pub(crate) canonical_head: Arc<CanonicalHead<T>>,
    pub(crate) attestation_manager: Arc<AttestationManager<T::EthSpec>>,
    // Held as direct Arc handles even when current access still goes through free helpers that
    // take `&BeaconChain<T>`; retaining the Arcs here lets us migrate those helpers to
    // per-component inputs without churning this struct again.
    pub validator_monitor: Arc<RwLock<ValidatorMonitor<T::EthSpec>>>,
    pub observed_slashable: Arc<RwLock<ObservedSlashable<T::EthSpec>>>,
    pub event_handler: Option<Arc<ServerSentEventHandler<T::EthSpec>>>,
    /// Maintains a record of which validators have proposed blocks for each slot.
    pub observed_block_producers: Arc<RwLock<ObservedBlockProducers<T::EthSpec>>>,
    /// Maintains a record of blob sidecars seen over the gossip network.
    #[allow(clippy::type_complexity)]
    pub observed_blob_sidecars:
        Arc<RwLock<ObservedDataSidecars<BlobSidecar<T::EthSpec>, T::EthSpec>>>,
    /// Maintains a record of column sidecars seen over the gossip network.
    #[allow(clippy::type_complexity)]
    pub observed_column_sidecars:
        Arc<RwLock<ObservedDataSidecars<DataColumnSidecar<T::EthSpec>, T::EthSpec>>>,
    pub block_times_cache: Arc<RwLock<BlockTimesCache>>,
    pub envelope_times_cache: Arc<RwLock<crate::envelope_times_cache::EnvelopeTimesCache>>,
    pub pre_finalization_block_cache: crate::pre_finalization_cache::PreFinalizationBlockCache,
    pub slasher: Option<Arc<Slasher<T::EthSpec>>>,
    pub light_client_server_cache: crate::light_client_server_cache::LightClientServerCache<T>,
    pub light_client_server_tx: Option<Sender<LightClientProducerEvent<T::EthSpec>>>,
    pub(crate) config: Arc<ChainConfig>,
    // Copy/Clone value fields.
    pub(crate) slot_clock: T::SlotClock,
    pub(crate) genesis_block_root: Hash256,
    // Utilities.
    pub(crate) task_executor: TaskExecutor,
    pub shutdown_sender: Sender<ShutdownReason>,
    // Weak back-reference to the parent `BeaconChain`, installed post-construction by the
    // builder. Uses `Weak` to avoid a reference cycle that would prevent cleanup in tests.
    // Upgraded via `self.system()` inside method bodies; the upgrade never fails during the
    // lifetime of a running beacon chain.
    pub(crate) system: OnceLock<Weak<BeaconChain<T>>>,
}

impl<T: BeaconChainTypes> BlockImporter<T> {
    /// Create a new `BlockImporter` from its injected dependencies.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn new(
        spec: Arc<ChainSpec>,
        store: BeaconStore<T>,
        data_availability_checker: Arc<DataAvailabilityChecker<T>>,
        data_availability_manager: Arc<DataAvailabilityManager<T>>,
        canonical_head: Arc<CanonicalHead<T>>,
        attestation_manager: Arc<AttestationManager<T::EthSpec>>,
        validator_monitor: Arc<RwLock<ValidatorMonitor<T::EthSpec>>>,
        observed_slashable: Arc<RwLock<ObservedSlashable<T::EthSpec>>>,
        event_handler: Option<Arc<ServerSentEventHandler<T::EthSpec>>>,
        observed_block_producers: Arc<RwLock<ObservedBlockProducers<T::EthSpec>>>,
        observed_blob_sidecars: Arc<
            RwLock<ObservedDataSidecars<BlobSidecar<T::EthSpec>, T::EthSpec>>,
        >,
        observed_column_sidecars: Arc<
            RwLock<ObservedDataSidecars<DataColumnSidecar<T::EthSpec>, T::EthSpec>>,
        >,
        block_times_cache: Arc<RwLock<BlockTimesCache>>,
        envelope_times_cache: Arc<RwLock<crate::envelope_times_cache::EnvelopeTimesCache>>,
        pre_finalization_block_cache: crate::pre_finalization_cache::PreFinalizationBlockCache,
        slasher: Option<Arc<Slasher<T::EthSpec>>>,
        light_client_server_cache: crate::light_client_server_cache::LightClientServerCache<T>,
        light_client_server_tx: Option<Sender<LightClientProducerEvent<T::EthSpec>>>,
        config: Arc<ChainConfig>,
        slot_clock: T::SlotClock,
        genesis_block_root: Hash256,
        task_executor: TaskExecutor,
        shutdown_sender: Sender<ShutdownReason>,
    ) -> Self {
        Self {
            spec,
            store,
            data_availability_checker,
            data_availability_manager,
            canonical_head,
            attestation_manager,
            validator_monitor,
            observed_slashable,
            event_handler,
            observed_block_producers,
            observed_blob_sidecars,
            observed_column_sidecars,
            block_times_cache,
            envelope_times_cache,
            pre_finalization_block_cache,
            slasher,
            light_client_server_cache,
            light_client_server_tx,
            config,
            slot_clock,
            genesis_block_root,
            task_executor,
            shutdown_sender,
            system: OnceLock::new(),
        }
    }

    /// Install the weak back-reference to the parent `BeaconChain`.
    ///
    /// Must be called once by the builder after `BeaconChain` has been wrapped in an `Arc`.
    pub fn set_system(&self, system: &Arc<BeaconChain<T>>) {
        let _ = self.system.set(Arc::downgrade(system));
    }

    /// Get the parent reference by upgrading the `Weak`.
    ///
    /// Panics if the parent has been dropped (programming error) or not installed yet.
    pub(crate) fn system(&self) -> Arc<BeaconChain<T>> {
        self.system
            .get()
            .expect("BlockImporter system not installed; builder bug")
            .upgrade()
            .expect("BeaconChain dropped while BlockImporter still alive")
    }

    pub fn filter_chain_segment(
        &self,
        chain_segment: Vec<RangeSyncBlock<T::EthSpec>>,
    ) -> Result<Vec<HashBlockTuple<T::EthSpec>>, Box<ChainSegmentResult>> {
        // This function will never import any blocks.
        let imported_blocks = vec![];
        let mut filtered_chain_segment = Vec::with_capacity(chain_segment.len());
        let chain = self.system().clone();

        // Produce a list of the parent root and slot of the child of each block.
        //
        // E.g., `children[0] == (chain_segment[1].parent_root(), chain_segment[1].slot())`
        let children = chain_segment
            .iter()
            .skip(1)
            .map(|block| (block.parent_root(), block.slot()))
            .collect::<Vec<_>>();

        for (i, block) in chain_segment.into_iter().enumerate() {
            // Ensure the block is the correct structure for the fork at `block.slot()`.
            if let Err(e) = block.as_block().fork_name(&self.spec) {
                return Err(Box::new(ChainSegmentResult::Failed {
                    imported_blocks,
                    error: BlockError::InconsistentFork(e),
                }));
            }

            let block_root = block.block_root();

            if let Some((child_parent_root, child_slot)) = children.get(i) {
                // If this block has a child in this chain segment, ensure that its parent root matches
                // the root of this block.
                //
                // Without this check it would be possible to have a block verified using the
                // incorrect shuffling. That would be bad, mmkay.
                if block_root != *child_parent_root {
                    return Err(Box::new(ChainSegmentResult::Failed {
                        imported_blocks,
                        error: BlockError::NonLinearParentRoots,
                    }));
                }

                // Ensure that the slots are strictly increasing throughout the chain segment.
                if *child_slot <= block.slot() {
                    return Err(Box::new(ChainSegmentResult::Failed {
                        imported_blocks,
                        error: BlockError::NonLinearSlots,
                    }));
                }
            }

            match check_block_relevancy(block.as_block(), block_root, &chain) {
                // If the block is relevant, add it to the filtered chain segment.
                Ok(_) => filtered_chain_segment.push((block_root, block)),
                // If the block is already known, simply ignore this block.
                //
                // Note that `check_block_relevancy` is incapable of returning
                // `DuplicateImportStatusUnknown` so we don't need to handle that case here.
                Err(BlockError::DuplicateFullyImported(_)) => continue,
                // If the block is the genesis block, simply ignore this block.
                Err(BlockError::GenesisBlock) => continue,
                // If the block is is for a finalized slot, simply ignore this block.
                //
                // The block is either:
                //
                // 1. In the canonical finalized chain.
                // 2. In some non-canonical chain at a slot that has been finalized already.
                //
                // In the case of (1), there's no need to re-import and later blocks in this
                // segement might be useful.
                //
                // In the case of (2), skipping the block is valid since we should never import it.
                // However, we will potentially get a `ParentUnknown` on a later block. The sync
                // protocol will need to ensure this is handled gracefully.
                Err(BlockError::WouldRevertFinalizedSlot { .. }) => continue,
                // The block has a known parent that does not descend from the finalized block.
                // There is no need to process this block or any children.
                Err(BlockError::NotFinalizedDescendant { block_parent_root }) => {
                    return Err(Box::new(ChainSegmentResult::Failed {
                        imported_blocks,
                        error: BlockError::NotFinalizedDescendant { block_parent_root },
                    }));
                }
                // If there was an error whilst determining if the block was invalid, return that
                // error.
                Err(BlockError::BeaconChainError(e)) => {
                    return Err(Box::new(ChainSegmentResult::Failed {
                        imported_blocks,
                        error: BlockError::BeaconChainError(e),
                    }));
                }
                // If the block was decided to be irrelevant for any other reason, don't include
                // this block or any of it's children in the filtered chain segment.
                _ => break,
            }
        }

        Ok(filtered_chain_segment)
    }

    /// Attempt to verify and import a chain of blocks to `chain`.
    ///
    /// The provided blocks _must_ each reference the previous block via `block.parent_root` (i.e.,
    /// be a chain). An error will be returned if this is not the case.
    ///
    /// This operation is not atomic; if one of the blocks in the chain is invalid then some prior
    /// blocks might be imported.
    ///
    /// This method is generally much more efficient than importing each block using
    /// `process_block`.
    pub async fn process_chain_segment(
        self: &Arc<Self>,
        chain_segment: Vec<RangeSyncBlock<T::EthSpec>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> ChainSegmentResult {
        for block in chain_segment.iter() {
            if let Err(error) = self.check_invalid_block_roots(block.block_root()) {
                return ChainSegmentResult::Failed {
                    imported_blocks: vec![],
                    error,
                };
            }
        }

        let mut imported_blocks = vec![];

        // Filter uninteresting blocks from the chain segment in a blocking task.
        let importer_clone = self.clone();
        let filtered_chain_segment_future = crate::utils::spawn_blocking_handle(
            &self.task_executor,
            move || importer_clone.filter_chain_segment(chain_segment),
            "filter_chain_segment",
        );
        let mut filtered_chain_segment = match filtered_chain_segment_future.await {
            Ok(Ok(filtered_segment)) => filtered_segment,
            Ok(Err(segment_result)) => return *segment_result,
            Err(error) => {
                return ChainSegmentResult::Failed {
                    imported_blocks,
                    error: BlockError::BeaconChainError(error.into()),
                };
            }
        };

        while let Some((_root, block)) = filtered_chain_segment.first() {
            // Determine the epoch of the first block in the remaining segment.
            let start_epoch = block.epoch();

            // The `last_index` indicates the position of the first block in an epoch greater
            // than the current epoch: partitioning the blocks into a run of blocks in the same
            // epoch and everything else. These same-epoch blocks can all be signature-verified with
            // the same `BeaconState`.
            let last_index = filtered_chain_segment
                .iter()
                .position(|(_root, block)| block.epoch() > start_epoch)
                .unwrap_or(filtered_chain_segment.len());

            let mut blocks = filtered_chain_segment.split_off(last_index);
            std::mem::swap(&mut blocks, &mut filtered_chain_segment);

            let chain_clone = self.system().clone();
            let signature_verification_future = crate::utils::spawn_blocking_handle(
                &self.task_executor,
                move || signature_verify_chain_segment(blocks, &chain_clone),
                "signature_verify_chain_segment",
            );

            // Verify the signature of the blocks, returning early if the signature is invalid.
            let signature_verified_blocks = match signature_verification_future.await {
                Ok(Ok(blocks)) => blocks,
                Ok(Err(error)) => {
                    return ChainSegmentResult::Failed {
                        imported_blocks,
                        error,
                    };
                }
                Err(error) => {
                    return ChainSegmentResult::Failed {
                        imported_blocks,
                        error: BlockError::BeaconChainError(error.into()),
                    };
                }
            };

            // Import the blocks into the chain.
            for signature_verified_block in signature_verified_blocks {
                let block_slot = signature_verified_block.slot();
                match self
                    .process_block(
                        signature_verified_block.block_root(),
                        signature_verified_block,
                        notify_execution_layer,
                        BlockImportSource::RangeSync,
                        || Ok(()),
                    )
                    .await
                {
                    Ok(status) => {
                        match status {
                            AvailabilityProcessingStatus::Imported(block_root) => {
                                // The block was imported successfully.
                                imported_blocks.push((block_root, block_slot));
                            }
                            AvailabilityProcessingStatus::MissingComponents(slot, block_root) => {
                                warn!(
                                    ?block_root,
                                    %slot,
                                    "Blobs missing in response to range request"
                                );
                                return ChainSegmentResult::Failed {
                                    imported_blocks,
                                    error: BlockError::AvailabilityCheck(
                                        AvailabilityCheckError::MissingBlobs,
                                    ),
                                };
                            }
                        }
                    }
                    Err(BlockError::DuplicateFullyImported(block_root)) => {
                        debug!(
                            ?block_root,
                            "Ignoring already known blocks while processing chain segment"
                        );
                        continue;
                    }
                    Err(error) => {
                        return ChainSegmentResult::Failed {
                            imported_blocks,
                            error,
                        };
                    }
                }
            }
        }

        ChainSegmentResult::Successful { imported_blocks }
    }

    /// Returns `Ok(GossipVerifiedBlock)` if the supplied `block` should be forwarded onto the
    /// gossip network. The block is not imported into the chain, it is just partially verified.
    ///
    /// The returned `GossipVerifiedBlock` should be provided to `process_block` immediately
    /// after it is returned, unless some other circumstance decides it should not be imported at
    /// all.
    ///
    /// ## Errors
    ///
    /// Returns an `Err` if the given block was invalid, or an error was encountered during
    pub async fn verify_block_for_gossip(
        &self,
        block: Arc<SignedBeaconBlock<T::EthSpec>>,
    ) -> Result<GossipVerifiedBlock<T>, BlockError> {
        let chain_clone = self.system().clone();
        self.task_executor
            .clone()
            .spawn_blocking_handle(
                move || {
                    let slot = block.slot();
                    let graffiti_string = block.message().body().graffiti().as_utf8_lossy();

                    match GossipVerifiedBlock::new(block, &chain_clone) {
                        Ok(verified) => {
                            let commitments_formatted = verified.block.commitments_formatted();
                            debug!(
                                graffiti = graffiti_string,
                                %slot,
                                root = ?verified.block_root(),
                                commitments = commitments_formatted,
                                "Successfully verified gossip block"
                            );

                            Ok(verified)
                        }
                        Err(e) => {
                            debug!(
                                error = e.to_string(),
                                graffiti = graffiti_string,
                                %slot,
                                "Rejected gossip block"
                            );

                            Err(e)
                        }
                    }
                },
                "gossip_block_verification_handle",
            )
            .ok_or(BeaconChainError::RuntimeShutdown)?
            .await
            .map_err(BeaconChainError::TokioJoin)?
    }

    /// Cache the blob in the processing cache, process it, then evict it from the cache if it was
    /// imported or errors.
    #[instrument(skip_all, level = "debug")]
    pub async fn process_gossip_blob(
        self: &Arc<Self>,
        blob: GossipVerifiedBlob<T>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let block_root = blob.block_root();
        let chain = self.system().clone();

        // If this block has already been imported to forkchoice it must have been available, so
        // we don't need to process its blobs again.
        if chain
            .canonical_head
            .fork_choice_read_lock()
            .contains_block(&block_root)
        {
            return Err(BlockError::DuplicateFullyImported(blob.block_root()));
        }

        // No need to process and import blobs beyond the PeerDAS epoch.
        if self.spec.is_peer_das_enabled_for_epoch(blob.epoch()) {
            return Err(BlockError::BlobNotRequired(blob.slot()));
        }

        emit_sse_blob_sidecar_events(&chain, &block_root, std::iter::once(blob.as_blob()));

        self.check_gossip_blob_availability_and_import(blob).await
    }

    /// Cache the data columns in the processing cache, process it, then evict it from the cache if it was
    /// imported or errors.
    #[instrument(skip_all, level = "debug")]
    pub async fn process_gossip_data_columns(
        self: &Arc<Self>,
        data_columns: Vec<GossipVerifiedDataColumn<T>>,
        publish_fn: impl FnOnce() -> Result<(), BlockError>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let Ok((slot, block_root)) = data_columns
            .iter()
            .map(|c| (c.slot(), c.block_root()))
            .unique()
            .exactly_one()
        else {
            return Err(BlockError::InternalError(
                "Columns should be from the same block".to_string(),
            ));
        };

        let chain = self.system().clone();

        // If this block has already been imported to forkchoice it must have been available, so
        // we don't need to process its samples again.
        if chain
            .canonical_head
            .fork_choice_read_lock()
            .contains_block(&block_root)
        {
            return Err(BlockError::DuplicateFullyImported(block_root));
        }

        emit_sse_data_column_sidecar_events(
            &chain,
            &block_root,
            data_columns.iter().map(|column| column.as_data_column()),
        );

        self.check_gossip_data_columns_availability_and_import(
            slot,
            block_root,
            data_columns,
            publish_fn,
        )
        .await
    }

    /// Cache the blobs in the processing cache, process it, then evict it from the cache if it was
    /// imported or errors.
    #[instrument(skip_all, level = "debug")]
    pub async fn process_rpc_blobs(
        self: &Arc<Self>,
        slot: Slot,
        block_root: Hash256,
        blobs: FixedBlobSidecarList<T::EthSpec>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let chain = self.system().clone();

        // If this block has already been imported to forkchoice it must have been available, so
        // we don't need to process its blobs again.
        if chain
            .canonical_head
            .fork_choice_read_lock()
            .contains_block(&block_root)
        {
            return Err(BlockError::DuplicateFullyImported(block_root));
        }

        // Reject RPC blobs referencing unknown parents. Otherwise we allow potentially invalid data
        // into the da_checker, where invalid = descendant of invalid blocks.
        // Note: blobs should have at least one item and all items have the same parent root.
        if let Some(parent_root) = blobs
            .iter()
            .filter_map(|b| b.as_ref().map(|b| b.block_parent_root()))
            .next()
            && !chain
                .canonical_head
                .fork_choice_read_lock()
                .contains_block(&parent_root)
        {
            return Err(BlockError::ParentUnknown { parent_root });
        }

        emit_sse_blob_sidecar_events(&chain, &block_root, blobs.iter().flatten().map(Arc::as_ref));

        self.check_rpc_blob_availability_and_import(slot, block_root, blobs)
            .await
    }

    /// Process blobs retrieved from the EL and returns the `AvailabilityProcessingStatus`.
    pub async fn process_engine_blobs(
        self: &Arc<Self>,
        slot: Slot,
        block_root: Hash256,
        engine_get_blobs_output: EngineGetBlobsOutput<T>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let chain = self.system().clone();

        // If this block has already been imported to forkchoice it must have been available, so
        // we don't need to process its blobs again.
        if chain
            .canonical_head
            .fork_choice_read_lock()
            .contains_block(&block_root)
        {
            return Err(BlockError::DuplicateFullyImported(block_root));
        }

        match &engine_get_blobs_output {
            EngineGetBlobsOutput::Blobs(blobs) => {
                emit_sse_blob_sidecar_events(
                    &chain,
                    &block_root,
                    blobs.iter().map(|b| b.as_blob()),
                );
            }
            EngineGetBlobsOutput::CustodyColumns(columns) => {
                emit_sse_data_column_sidecar_events(
                    &chain,
                    &block_root,
                    columns.iter().map(|column| column.as_data_column()),
                );
            }
        }

        self.check_engine_blobs_availability_and_import(slot, block_root, engine_get_blobs_output)
            .await
    }

    /// Cache the columns in the processing cache, process it, then evict it from the cache if it was
    /// imported or errors.
    // TODO(gloas) we need a separate code path for gloas. See TODO's below.
    pub async fn process_rpc_custody_columns(
        self: &Arc<Self>,
        custody_columns: DataColumnSidecarList<T::EthSpec>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let Ok((slot, block_root)) = custody_columns
            .iter()
            .map(|c| (c.slot(), c.block_root()))
            .unique()
            .exactly_one()
        else {
            return Err(BlockError::InternalError(
                "Columns should be from the same block".to_string(),
            ));
        };

        let chain = self.system().clone();

        // If this block has already been imported to forkchoice it must have been available, so
        // we don't need to process its columns again.
        // TODO(gloas) the block will be available in fork choice for gloas. This does not indicate availability
        // anymore.
        if chain
            .canonical_head
            .fork_choice_read_lock()
            .contains_block(&block_root)
        {
            return Err(BlockError::DuplicateFullyImported(block_root));
        }

        // Reject RPC columns referencing unknown parents. Otherwise we allow potentially invalid data
        // into the da_checker, where invalid = descendant of invalid blocks.
        // Note: custody_columns should have at least one item and all items have the same parent root.
        // TODO(gloas) ensure this check is no longer relevant post gloas
        if let Some(parent_root) = custody_columns
            .iter()
            .filter_map(|c| match c.as_ref() {
                DataColumnSidecar::Fulu(column) => Some(column.block_parent_root()),
                _ => None,
            })
            .next()
            && !chain
                .canonical_head
                .fork_choice_read_lock()
                .contains_block(&parent_root)
        {
            return Err(BlockError::ParentUnknown { parent_root });
        }

        emit_sse_data_column_sidecar_events(
            &chain,
            &block_root,
            custody_columns.iter().map(|column| column.as_ref()),
        );

        self.check_rpc_custody_columns_availability_and_import(slot, block_root, custody_columns)
            .await
    }

    pub async fn reconstruct_data_columns(
        self: &Arc<Self>,
        block_root: Hash256,
    ) -> Result<
        Option<(
            AvailabilityProcessingStatus,
            DataColumnSidecarList<T::EthSpec>,
        )>,
        BlockError,
    > {
        // As of now we only reconstruct data columns on supernodes, so if the block is already
        // available on a supernode, there's no need to reconstruct as the node must already have
        // all columns.
        if self
            .canonical_head
            .fork_choice_read_lock()
            .contains_block(&block_root)
        {
            return Ok(None);
        }

        let data_availability_checker = self.data_availability_checker.clone();

        let result = self
            .task_executor
            .spawn_blocking_with_rayon_async(RayonPoolType::HighPriority, move || {
                data_availability_checker.reconstruct_data_columns(&block_root)
            })
            .await
            .map_err(|_| BeaconChainError::RuntimeShutdown)??;

        match result {
            DataColumnReconstructionResult::Success((availability, data_columns_to_publish)) => {
                let Some(slot) = data_columns_to_publish.first().map(|d| d.slot()) else {
                    // This should be unreachable because empty result would return `RecoveredColumnsNotImported` instead of success.
                    return Ok(None);
                };

                self.process_availability(slot, availability, || Ok(()))
                    .await
                    .map(|availability_processing_status| {
                        Some((availability_processing_status, data_columns_to_publish))
                    })
            }
            DataColumnReconstructionResult::NotStarted(reason)
            | DataColumnReconstructionResult::RecoveredColumnsNotImported(reason) => {
                // We use metric here because logging this would be *very* noisy.
                metrics::inc_counter_vec(
                    &metrics::KZG_DATA_COLUMN_RECONSTRUCTION_INCOMPLETE_TOTAL,
                    &[reason],
                );
                Ok(None)
            }
        }
    }

    /// Check for known and configured invalid block roots before processing.
    pub fn check_invalid_block_roots(&self, block_root: Hash256) -> Result<(), BlockError> {
        if self.config.invalid_block_roots.contains(&block_root) {
            Err(BlockError::KnownInvalidExecutionPayload(block_root))
        } else {
            Ok(())
        }
    }

    /// Returns `Ok(block_root)` if the given `unverified_block` was successfully verified and
    /// imported into the chain.
    ///
    /// Items that implement `IntoExecutionPendingBlock` include:
    ///
    /// - `SignedBeaconBlock`
    /// - `GossipVerifiedBlock`
    /// - `RpcBlock`
    ///
    /// ## Errors
    ///
    /// Returns an `Err` if the given block was invalid, or an error was encountered during
    /// verification.
    #[instrument(skip_all, fields(block_root = ?block_root, block_source = %block_source))]
    pub async fn process_block<B: IntoExecutionPendingBlock<T>>(
        self: &Arc<Self>,
        block_root: Hash256,
        unverified_block: B,
        notify_execution_layer: NotifyExecutionLayer,
        block_source: BlockImportSource,
        publish_fn: impl FnOnce() -> Result<(), BlockError>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let block_slot = unverified_block.block().slot();

        // Set observed time if not already set. Usually this should be set by gossip or RPC,
        // but just in case we set it again here (useful for tests).
        if let Some(seen_timestamp) = self.slot_clock.now_duration() {
            self.block_times_cache.write().set_time_observed(
                block_root,
                block_slot,
                seen_timestamp,
                None,
                None,
            );
        }

        // Gloas blocks dont need to be inserted into the DA cache
        // they are always available.
        if !unverified_block
            .block()
            .fork_name_unchecked()
            .gloas_enabled()
        {
            self.data_availability_checker.put_pre_execution_block(
                block_root,
                unverified_block.block_cloned(),
                block_source,
            )?;
        }

        // Start the Prometheus timer.
        let _full_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_TIMES);

        // Increment the Prometheus counter for block processing requests.
        metrics::inc_counter(&metrics::BLOCK_PROCESSING_REQUESTS);

        // A small closure to group the verification and import errors.
        let chain_clone = self.system().clone();
        let importer_clone = self.clone();
        let import_block = async move {
            let execution_pending = unverified_block.into_execution_pending_block(
                block_root,
                &chain_clone,
                notify_execution_layer,
            )?;
            publish_fn()?;

            // Record the time it took to complete consensus verification.
            if let Some(timestamp) = importer_clone.slot_clock.now_duration() {
                importer_clone
                    .block_times_cache
                    .write()
                    .set_time_consensus_verified(block_root, block_slot, timestamp)
            }

            let executed_block = importer_clone
                .into_executed_block(execution_pending)
                .await
                .inspect_err(|_| {
                    // If the block fails execution for whatever reason (e.g. engine offline),
                    // and we keep it in the cache, then the node will NOT perform lookup and
                    // reprocess this block until the block is evicted from DA checker, causing the
                    // chain to get stuck temporarily if the block is canonical. Therefore we remove
                    // it from the cache if execution fails.
                    importer_clone
                        .data_availability_checker
                        .remove_block_on_execution_error(&block_root);
                })?;

            // Record the *additional* time it took to wait for execution layer verification.
            if let Some(timestamp) = importer_clone.slot_clock.now_duration() {
                importer_clone
                    .block_times_cache
                    .write()
                    .set_time_executed(block_root, block_slot, timestamp)
            }

            match executed_block {
                ExecutedBlock::Available(block) => {
                    importer_clone.import_available_block(Box::new(block)).await
                }
                ExecutedBlock::AvailabilityPending(block) => {
                    importer_clone
                        .check_block_availability_and_import(block)
                        .await
                }
            }
        };

        // Verify and import the block.
        match import_block.await {
            // The block was successfully verified and imported. Yay.
            Ok(status @ AvailabilityProcessingStatus::Imported(block_root)) => {
                debug!(
                    ?block_root,
                    %block_slot,
                    source = %block_source,
                    "Beacon block imported"
                );

                // Increment the Prometheus counter for block processing successes.
                metrics::inc_counter(&metrics::BLOCK_PROCESSING_SUCCESSES);

                Ok(status)
            }
            Ok(status @ AvailabilityProcessingStatus::MissingComponents(slot, block_root)) => {
                debug!(?block_root, %slot, "Beacon block awaiting blobs");

                Ok(status)
            }
            Err(BlockError::BeaconChainError(e)) => {
                match e.as_ref() {
                    BeaconChainError::TokioJoin(e) => {
                        debug!(
                            error = ?e,
                            "Beacon block processing cancelled"
                        );
                    }
                    _ => {
                        // There was an error whilst attempting to verify and import the block. The block might
                        // be partially verified or partially imported.
                        crit!(
                            error = ?e,
                            "Beacon block processing error"
                        );
                    }
                };
                Err(BlockError::BeaconChainError(e))
            }
            // The block failed verification.
            Err(other) => {
                debug!(reason = other.to_string(), "Beacon block rejected");
                Err(other)
            }
        }
    }

    /// Accepts a fully-verified block and awaits on its payload verification handle to
    /// get a fully `ExecutedBlock`.
    ///
    /// An error is returned if the verification handle couldn't be awaited.
    #[instrument(skip_all, level = "debug")]
    pub async fn into_executed_block(
        &self,
        execution_pending_block: ExecutionPendingBlock<T>,
    ) -> Result<ExecutedBlock<T::EthSpec>, BlockError> {
        let ExecutionPendingBlock {
            block,
            import_data,
            payload_verification_handle,
        } = execution_pending_block;

        let payload_verification_outcome = payload_verification_handle
            .await
            .map_err(BeaconChainError::TokioJoin)?
            .ok_or(BeaconChainError::RuntimeShutdown)??;

        Ok(ExecutedBlock::new(
            block,
            import_data,
            payload_verification_outcome,
        ))
    }

    /* Import methods */

    /// Checks if the block is available, and imports immediately if so, otherwise caches the block
    /// in the data availability checker.
    #[instrument(skip_all)]
    async fn check_block_availability_and_import(
        self: &Arc<Self>,
        block: AvailabilityPendingExecutedBlock<T::EthSpec>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let slot = block.block.slot();
        let availability = self.data_availability_checker.put_executed_block(block)?;
        self.process_availability(slot, availability, || Ok(()))
            .await
    }

    /// Checks if the provided blob can make any cached blocks available, and imports immediately
    /// if so, otherwise caches the blob in the data availability checker.
    async fn check_gossip_blob_availability_and_import(
        self: &Arc<Self>,
        blob: GossipVerifiedBlob<T>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let slot = blob.slot();
        if let Some(slasher) = self.slasher.as_ref() {
            slasher.accept_block_header(blob.signed_block_header());
        }
        let availability = self
            .data_availability_checker
            .put_gossip_verified_blobs(blob.block_root(), std::iter::once(blob))?;

        self.process_availability(slot, availability, || Ok(()))
            .await
    }

    /// Checks if the provided data column can make any cached blocks available, and imports immediately
    /// if so, otherwise caches the data column in the data availability checker.
    async fn check_gossip_data_columns_availability_and_import(
        self: &Arc<Self>,
        slot: Slot,
        block_root: Hash256,
        data_columns: Vec<GossipVerifiedDataColumn<T>>,
        publish_fn: impl FnOnce() -> Result<(), BlockError>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        if let Some(slasher) = self.slasher.as_ref() {
            for data_column in &data_columns {
                // TODO(gloas) different gossip checks in gloas
                // https://github.com/ethereum/consensus-specs/blob/81458afc6aad6985c533785c8d2860d87a993241/specs/gloas/p2p-interface.md?plain=1#L385
                if let DataColumnSidecar::Fulu(c) = data_column.as_data_column() {
                    slasher.accept_block_header(c.signed_block_header.clone());
                }
            }
        }

        let availability = self
            .data_availability_checker
            .put_gossip_verified_data_columns(block_root, slot, data_columns)?;

        self.process_availability(slot, availability, publish_fn)
            .await
    }

    /// Checks if the provided blobs can make any cached blocks available, and imports immediately
    /// if so, otherwise caches the blob in the data availability checker.
    async fn check_rpc_blob_availability_and_import(
        self: &Arc<Self>,
        slot: Slot,
        block_root: Hash256,
        blobs: FixedBlobSidecarList<T::EthSpec>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        check_blob_header_signature_and_slashability(
            &self.system(),
            block_root,
            blobs.iter().flatten().map(Arc::as_ref),
        )?;
        let availability = self
            .data_availability_checker
            .put_rpc_blobs(block_root, blobs)?;

        self.process_availability(slot, availability, || Ok(()))
            .await
    }

    async fn check_engine_blobs_availability_and_import(
        self: &Arc<Self>,
        slot: Slot,
        block_root: Hash256,
        engine_get_blobs_output: EngineGetBlobsOutput<T>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let availability = match engine_get_blobs_output {
            EngineGetBlobsOutput::Blobs(blobs) => {
                check_blob_header_signature_and_slashability(
                    &self.system(),
                    block_root,
                    blobs.iter().map(|b| b.as_blob()),
                )?;
                self.data_availability_checker
                    .put_kzg_verified_blobs(block_root, blobs)?
            }
            EngineGetBlobsOutput::CustodyColumns(data_columns) => {
                // TODO(gloas) verify that this check is no longer relevant for gloas
                check_data_column_sidecar_header_signature_and_slashability(
                    &self.system(),
                    block_root,
                    data_columns
                        .iter()
                        .filter_map(|c| match c.as_data_column() {
                            DataColumnSidecar::Fulu(column) => Some(column),
                            _ => None,
                        }),
                )?;
                self.data_availability_checker
                    .put_kzg_verified_custody_data_columns(block_root, data_columns)?
            }
        };

        self.process_availability(slot, availability, || Ok(()))
            .await
    }

    /// Checks if the provided columns can make any cached blocks available, and imports immediately
    /// if so, otherwise caches the columns in the data availability checker.
    async fn check_rpc_custody_columns_availability_and_import(
        self: &Arc<Self>,
        slot: Slot,
        block_root: Hash256,
        custody_columns: DataColumnSidecarList<T::EthSpec>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        // TODO(gloas) ensure that this check is no longer relevant post gloas
        check_data_column_sidecar_header_signature_and_slashability(
            &self.system(),
            block_root,
            custody_columns.iter().filter_map(|c| match c.as_ref() {
                DataColumnSidecar::Fulu(fulu) => Some(fulu),
                _ => None,
            }),
        )?;

        // This slot value is purely informative for the consumers of
        // `AvailabilityProcessingStatus::MissingComponents` to log an error with a slot.
        let availability = self.data_availability_checker.put_rpc_custody_columns(
            block_root,
            slot,
            custody_columns,
        )?;

        self.process_availability(slot, availability, || Ok(()))
            .await
    }

    /// Imports a fully available block. Otherwise, returns `AvailabilityProcessingStatus::MissingComponents`
    ///
    /// An error is returned if the block was unable to be imported. It may be partially imported
    /// (i.e., this function is not atomic).
    async fn process_availability(
        self: &Arc<Self>,
        slot: Slot,
        availability: Availability<T::EthSpec>,
        publish_fn: impl FnOnce() -> Result<(), BlockError>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        match availability {
            Availability::Available(block) => {
                publish_fn()?;
                // Block is fully available, import into fork choice
                self.import_available_block(block).await
            }
            Availability::MissingComponents(block_root) => Ok(
                AvailabilityProcessingStatus::MissingComponents(slot, block_root),
            ),
        }
    }

    #[instrument(skip_all)]
    pub async fn import_available_block(
        self: &Arc<Self>,
        block: Box<AvailableExecutedBlock<T::EthSpec>>,
    ) -> Result<AvailabilityProcessingStatus, BlockError> {
        let AvailableExecutedBlock {
            block,
            import_data,
            payload_verification_outcome,
        } = *block;

        let BlockImportData {
            block_root,
            state,
            parent_block,
            consensus_context,
        } = import_data;

        // Record the time at which this block's blobs/data columns became available.
        if let Some(blobs_available) = block.blobs_available_timestamp() {
            self.block_times_cache.write().set_time_blob_observed(
                block_root,
                block.slot(),
                blobs_available,
            );
        }

        let block_root = {
            let importer_clone = self.clone();
            crate::utils::spawn_blocking_handle(
                &self.task_executor,
                move || {
                    importer_clone.import_block(
                        block,
                        block_root,
                        state,
                        payload_verification_outcome.payload_verification_status,
                        parent_block,
                        consensus_context,
                    )
                },
                "payload_verification_handle",
            )
            .await??
        };

        Ok(AvailabilityProcessingStatus::Imported(block_root))
    }

    /// Accepts a fully-verified and available block and imports it into the chain without performing any
    /// additional verification.
    ///
    /// An error is returned if the block was unable to be imported. It may be partially imported
    /// (i.e., this function is not atomic).
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all)]
    fn import_block(
        &self,
        signed_block: AvailableBlock<T::EthSpec>,
        block_root: Hash256,
        mut state: BeaconState<T::EthSpec>,
        payload_verification_status: PayloadVerificationStatus,
        parent_block: SignedBlindedBeaconBlock<T::EthSpec>,
        mut consensus_context: ConsensusContext<T::EthSpec>,
    ) -> Result<Hash256, BlockError> {
        // ----------------------------- BLOCK NOT YET ATTESTABLE ----------------------------------
        // Everything in this initial section is on the hot path between processing the block and
        // being able to attest to it. DO NOT add any extra processing in this initial section
        // unless it must run before fork choice.
        // -----------------------------------------------------------------------------------------
        let chain = self.system().clone();
        let current_slot = self.slot_clock.now().ok_or(Error::UnableToReadSlot)?;
        let current_epoch = current_slot.epoch(T::EthSpec::slots_per_epoch());
        let block = signed_block.message();
        let post_exec_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_POST_EXEC_PROCESSING);

        // Check against weak subjectivity checkpoint.
        self.check_block_against_weak_subjectivity_checkpoint(block, block_root, &state)?;

        // If there are new validators in this block, update our pubkey cache.
        //
        // The only keys imported here will be ones for validators deposited in this block, because
        // the cache *must* already have been updated for the parent block when it was imported.
        // Newly deposited validators are not active and their keys are not required by other parts
        // of block processing. The reason we do this here and not after making the block attestable
        // is so we don't have to think about lock ordering with respect to the fork choice lock.
        // There are a bunch of places where we lock both fork choice and the pubkey cache and it
        // would be difficult to check that they all lock fork choice first.
        let mut ops = {
            let _timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_PUBKEY_CACHE_LOCK);
            let pubkey_cache = chain
                .validator_query
                .validator_pubkey_cache
                .upgradable_read();

            // Only take a write lock if there are new keys to import.
            if state.validators().len() > pubkey_cache.len() {
                let _pubkey_span = debug_span!(
                    "pubkey_cache_update",
                    new_validators = tracing::field::Empty,
                    cache_len_before = pubkey_cache.len()
                )
                .entered();

                parking_lot::RwLockUpgradableReadGuard::upgrade(pubkey_cache)
                    .import_new_pubkeys(&state)?
            } else {
                vec![]
            }
        };

        // Read the cached head prior to taking the fork choice lock to avoid potential deadlocks.
        let old_head_slot = self.canonical_head.cached_head().head_slot();

        // Take an upgradable read lock on fork choice so we can check if this block has already
        // been imported. We don't want to repeat work importing a block that is already imported.
        let fork_choice_reader = self.canonical_head.fork_choice_upgradable_read_lock();
        if fork_choice_reader.contains_block(&block_root) {
            return Err(BlockError::DuplicateFullyImported(block_root));
        }

        // Take an exclusive write-lock on fork choice. It's very important to prevent deadlocks by
        // avoiding taking other locks whilst holding this lock.
        let mut fork_choice = parking_lot::RwLockUpgradableReadGuard::upgrade(fork_choice_reader);

        // Do not import a block that doesn't descend from the finalized root.
        let signed_block =
            check_block_is_finalized_checkpoint_or_descendant(&chain, &fork_choice, signed_block)?;
        let block = signed_block.message();

        // Register the new block with the fork choice service.
        {
            let block_delay = self
                .slot_clock
                .seconds_from_current_slot_start()
                .ok_or(Error::UnableToComputeTimeAtSlot)?;

            fork_choice
                .on_block(
                    current_slot,
                    block,
                    block_root,
                    block_delay,
                    &state,
                    payload_verification_status,
                    &self.spec,
                )
                .map_err(|e| BlockError::BeaconChainError(Box::new(e.into())))?;
        }

        // If the block is recent enough and it was not optimistically imported, check to see if it
        // becomes the head block. If so, apply it to the early attester cache. This will allow
        // attestations to the block without waiting for the block and state to be inserted to the
        // database.
        //
        // Only performing this check on recent blocks avoids slowing down sync with lots of calls
        // to fork choice `get_head`.
        //
        // Optimistically imported blocks are not added to the cache since the cache is only useful
        // for a small window of time and the complexity of keeping track of the optimistic status
        // is not worth it.
        if !payload_verification_status.is_optimistic()
            && block.slot() + EARLY_ATTESTER_CACHE_HISTORIC_SLOTS >= current_slot
        {
            let fork_choice_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_FORK_CHOICE);
            match fork_choice.get_head(current_slot, &self.spec) {
                // This block became the head, add it to the early attester cache.
                Ok((new_head_root, _)) if new_head_root == block_root => {
                    if let Some(proto_block) = fork_choice.get_block(&block_root) {
                        let new_head_is_optimistic =
                            proto_block.execution_status.is_optimistic_or_invalid();

                        if let Err(e) = chain
                            .attestation_manager
                            .early_attester_cache
                            .add_head_block(block_root, &signed_block, proto_block, &state)
                        {
                            warn!(
                                error = ?e,
                                "Early attester cache insert failed"
                            );
                        } else {
                            let attestable_timestamp =
                                self.slot_clock.now_duration().unwrap_or_default();
                            self.block_times_cache.write().set_time_attestable(
                                block_root,
                                signed_block.slot(),
                                attestable_timestamp,
                            )
                        }

                        // Register a server-sent-event for a new head.
                        if let Some(event_handler) = self
                            .event_handler
                            .as_ref()
                            .filter(|handler| handler.has_head_subscribers())
                        {
                            let head_slot = state.slot();
                            let state_root = block.state_root();
                            let is_epoch_transition = state.current_epoch()
                                > old_head_slot.epoch(T::EthSpec::slots_per_epoch());

                            let dependent_root = state.attester_shuffling_decision_root(
                                self.genesis_block_root,
                                RelativeEpoch::Next,
                            );
                            let prev_dependent_root = state.attester_shuffling_decision_root(
                                self.genesis_block_root,
                                RelativeEpoch::Current,
                            );

                            match (dependent_root, prev_dependent_root) {
                                (
                                    Ok(current_duty_dependent_root),
                                    Ok(previous_duty_dependent_root),
                                ) => {
                                    event_handler.register(EventKind::Head(SseHead {
                                        slot: head_slot,
                                        block: block_root,
                                        state: state_root,
                                        current_duty_dependent_root,
                                        previous_duty_dependent_root,
                                        epoch_transition: is_epoch_transition,
                                        execution_optimistic: new_head_is_optimistic,
                                    }));
                                }
                                (Err(e), _) | (_, Err(e)) => {
                                    warn!(
                                        error = ?e,
                                        "Unable to find dependent roots, cannot register head event"
                                    );
                                }
                            }
                        }
                    } else {
                        warn!(?block_root, "Early attester block missing");
                    }
                }
                // This block did not become the head, nothing to do.
                Ok(_) => (),
                Err(e) => error!(
                    error = ?e,
                    "Failed to compute head during block import"
                ),
            }
            drop(fork_choice_timer);
        }
        drop(post_exec_timer);

        // ---------------------------- BLOCK PROBABLY ATTESTABLE ----------------------------------
        // Most blocks are now capable of being attested to thanks to the `early_attester_cache`
        // cache above. Resume non-essential processing.
        //
        // It is important NOT to return errors here before the database commit, because the block
        // has already been added to fork choice and the database would be left in an inconsistent
        // state if we returned early without committing. In other words, an error here would
        // corrupt the node's database permanently.
        // -----------------------------------------------------------------------------------------
        self.attestation_manager
            .import_block_update_shuffling_cache(block_root, &mut state);
        import_block_observe_attestations(
            &chain,
            block,
            &state,
            &mut consensus_context,
            current_epoch,
        );
        import_block_update_validator_monitor(
            &chain,
            block,
            &state,
            &mut consensus_context,
            current_slot,
            parent_block.slot(),
        );
        import_block_update_slasher(&chain, block, &state, &mut consensus_context);

        // Store the block and its state, and execute the confirmation batch for the intermediate
        // states, which will delete their temporary flags.
        // If the write fails, revert fork choice to the version from disk, else we can
        // end up with blocks in fork choice that are missing from disk.
        // See https://github.com/sigp/lighthouse/issues/2028
        let (_, signed_block, block_data) = signed_block.deconstruct();

        if let Some(blobs_or_columns_store_op) =
            crate::data_availability_manager::get_blobs_or_columns_store_op(
                &self.data_availability_manager,
                &self.spec,
                block_root,
                signed_block.slot(),
                block_data,
            )
        {
            ops.push(blobs_or_columns_store_op);
        }

        let block = signed_block.message();
        let db_write_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_DB_WRITE);
        ops.push(StoreOp::PutBlock(block_root, signed_block.clone()));
        ops.push(StoreOp::PutState(block.state_root(), &state));

        let db_span = info_span!("persist_blocks_and_blobs").entered();

        if let Err(e) = self.store.do_atomically_with_block_and_blobs_cache(ops) {
            error!(
                msg = "Restoring fork choice from disk",
                error = ?e,
                "Database write failed!"
            );
            return Err(handle_import_block_db_write_error(&chain, fork_choice)
                .err()
                .unwrap_or(e.into()));
        }

        drop(db_span);

        // The fork choice write-lock is dropped *after* the on-disk database has been updated.
        // This prevents inconsistency between the two at the expense of concurrency.
        drop(fork_choice);

        // We're declaring the block "imported" at this point, since fork choice and the DB know
        // about it.
        let block_time_imported = self.slot_clock.now_duration().unwrap_or(Duration::MAX);

        // compute state proofs for light client updates before inserting the state into the
        // snapshot cache.
        if self.config.enable_light_client_server {
            chain
                .block_importer
                .light_client_server_cache
                .cache_state_data(
                    &self.spec, block, block_root,
                    // mutable reference on the state is needed to compute merkle proofs
                    &mut state,
                )
                .unwrap_or_else(|e| {
                    debug!("error caching light_client data {:?}", e);
                });
        }

        metrics::stop_timer(db_write_timer);

        metrics::inc_counter(&metrics::BLOCK_PROCESSING_SUCCESSES);

        // Inform the unknown block cache, in case it was waiting on this block.
        chain
            .block_importer
            .pre_finalization_block_cache
            .block_processed(block_root);

        import_block_update_metrics_and_events(
            self,
            block,
            block_root,
            block_time_imported,
            payload_verification_status,
            current_slot,
        );

        Ok(block_root)
    }

    /// Check block's consistentency with any configured weak subjectivity checkpoint.
    pub(crate) fn check_block_against_weak_subjectivity_checkpoint(
        &self,
        block: BeaconBlockRef<T::EthSpec>,
        block_root: Hash256,
        state: &BeaconState<T::EthSpec>,
    ) -> Result<(), BlockError> {
        // Only perform the weak subjectivity check if it was configured.
        let Some(wss_checkpoint) = self.config.weak_subjectivity_checkpoint else {
            return Ok(());
        };
        // Note: we're using the finalized checkpoint from the head state, rather than fork
        // choice.
        //
        // We are doing this to ensure that we detect changes in finalization. It's possible
        // that fork choice has already been updated to the finalized checkpoint in the block
        // we're importing.
        let current_head_finalized_checkpoint =
            self.canonical_head.cached_head().finalized_checkpoint();
        // Compare the existing finalized checkpoint with the incoming block's finalized checkpoint.
        let new_finalized_checkpoint = state.finalized_checkpoint();

        // This ensures we only perform the check once.
        if current_head_finalized_checkpoint.epoch < wss_checkpoint.epoch
            && wss_checkpoint.epoch <= new_finalized_checkpoint.epoch
            && let Err(e) = verify_weak_subjectivity_checkpoint(
                &self.system(),
                wss_checkpoint,
                block_root,
                state,
            )
        {
            let mut shutdown_sender = self.shutdown_sender.clone();
            crit!(
                ?block_root,
                parent_root = ?block.parent_root(),
                old_finalized_epoch = ?current_head_finalized_checkpoint.epoch,
                new_finalized_epoch = ?new_finalized_checkpoint.epoch,
                weak_subjectivity_epoch = ?wss_checkpoint.epoch,
                error = ?e,
                "Weak subjectivity checkpoint verification failed while importing block!"
            );
            crit!(
                "You must use the `--purge-db` flag to clear the database and restart sync. \
                         You may be on a hostile network."
            );
            shutdown_sender
                .try_send(ShutdownReason::Failure(
                    "Weak subjectivity checkpoint verification failed. \
                             Provided block root is not a checkpoint.",
                ))
                .map_err(|err| {
                    BlockError::BeaconChainError(Box::new(
                        BeaconChainError::WeakSubjectivtyShutdownError(err),
                    ))
                })?;
            return Err(BlockError::WeakSubjectivityConflict);
        }

        Ok(())
    }
}

// --- Module-level private helpers ---
//
// These helpers either consume a `&BlockImporter<T>` (which provides `self.parent()` on demand
// for reaching `BeaconChain` fields not yet held by the importer) or take the specific
// components they need explicitly. Crucially they do NOT have a `chain: &BeaconChain<T>`
// parameter — the importer is the sole entry point for block-import state.

/// Process a block for the validator monitor, including all its constituent messages.
#[instrument(skip_all, level = "debug")]
fn import_block_update_validator_monitor<T: BeaconChainTypes>(
    components: &BeaconChain<T>,
    block: BeaconBlockRef<T::EthSpec>,
    state: &BeaconState<T::EthSpec>,
    ctxt: &mut ConsensusContext<T::EthSpec>,
    current_slot: Slot,
    parent_block_slot: Slot,
) {
    // Only register blocks with the validator monitor when the block is sufficiently close to
    // the current slot.
    if VALIDATOR_MONITOR_HISTORIC_EPOCHS as u64 * T::EthSpec::slots_per_epoch()
        + block.slot().as_u64()
        < current_slot.as_u64()
    {
        return;
    }

    // Allow the validator monitor to learn about a new valid state.
    components
        .block_importer
        .validator_monitor
        .write()
        .process_valid_state(
            current_slot.epoch(T::EthSpec::slots_per_epoch()),
            state,
            &components.spec,
        );

    let validator_monitor = components.block_importer.validator_monitor.read();

    // Sync aggregate.
    if let Ok(sync_aggregate) = block.body().sync_aggregate() {
        // `SyncCommittee` for the sync_aggregate should correspond to the duty slot
        let duty_epoch = block.epoch();

        let res = {
            let head_state = &components.canonical_head.head_snapshot().beacon_state;
            components.sync_committee_manager.sync_committee_at_epoch(
                duty_epoch,
                head_state,
                |load_slot| {
                    crate::state_query::state_at_slot(
                        &components.store,
                        &components.canonical_head,
                        &components.spec,
                        load_slot,
                        crate::state_query::StateSkipConfig::WithoutStateRoots,
                    )
                },
            )
        };
        match res {
            Ok(sync_committee) => {
                let participant_pubkeys = sync_committee
                    .pubkeys
                    .iter()
                    .zip(sync_aggregate.sync_committee_bits.iter())
                    .filter_map(|(pubkey, bit)| bit.then_some(pubkey))
                    .collect::<Vec<_>>();

                validator_monitor.register_sync_aggregate_in_block(
                    block.slot(),
                    block.parent_root(),
                    participant_pubkeys,
                );
            }
            Err(e) => {
                warn!(
                    epoch = %duty_epoch,
                    purpose = "validator monitor",
                    error = ?e,
                    "Unable to fetch sync committee"
                );
            }
        }
    }

    // Attestations.
    for attestation in block.body().attestations() {
        let indexed_attestation = match ctxt.get_indexed_attestation(state, attestation) {
            Ok(indexed) => indexed,
            Err(e) => {
                debug!(
                    purpose = "validator monitor",
                    attestation_slot = %attestation.data().slot,
                    error = ?e,
                    "Failed to get indexed attestation"
                );
                continue;
            }
        };
        validator_monitor.register_attestation_in_block(
            indexed_attestation,
            parent_block_slot,
            &components.spec,
        );
    }

    for exit in block.body().voluntary_exits() {
        validator_monitor.register_block_voluntary_exit(&exit.message)
    }

    for slashing in block.body().attester_slashings() {
        validator_monitor.register_block_attester_slashing(slashing)
    }

    for slashing in block.body().proposer_slashings() {
        validator_monitor.register_block_proposer_slashing(slashing)
    }
}

/// Iterate through the attestations in the block and register them as "observed".
///
/// This will stop us from propagating them on the gossip network.
#[instrument(skip_all, level = "debug")]
fn import_block_observe_attestations<T: BeaconChainTypes>(
    components: &BeaconChain<T>,
    block: BeaconBlockRef<T::EthSpec>,
    state: &BeaconState<T::EthSpec>,
    ctxt: &mut ConsensusContext<T::EthSpec>,
    current_epoch: Epoch,
) {
    // To avoid slowing down sync, only observe attestations if the block is from the
    // previous epoch or later.
    if state.current_epoch() + 1 < current_epoch {
        return;
    }

    let _timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_ATTESTATION_OBSERVATION);

    for a in block.body().attestations() {
        match components
            .attestation_manager
            .observed_attestations
            .write()
            .observe_item(a, None)
        {
            // If the observation was successful or if the slot for the attestation was too
            // low, continue.
            //
            // We ignore `SlotTooLow` since this will be very common whilst syncing.
            Ok(_) | Err(AttestationObservationError::SlotTooLow { .. }) => {}
            Err(e) => {
                debug!(
                    error = ?e,
                    epoch = %a.data().target.epoch,
                    "Failed to register observed attestation"
                );
            }
        }

        let indexed_attestation = match ctxt.get_indexed_attestation(state, a) {
            Ok(indexed) => indexed,
            Err(e) => {
                debug!(
                    purpose = "observation",
                    attestation_slot = %a.data().slot,
                    error = ?e,
                    "Failed to get indexed attestation"
                );
                continue;
            }
        };

        let mut observed_block_attesters = components
            .attestation_manager
            .observed_block_attesters
            .write();

        for &validator_index in indexed_attestation.attesting_indices_iter() {
            if let Err(e) = observed_block_attesters
                .observe_validator(a.data().target.epoch, validator_index as usize)
            {
                debug!(
                    error = ?e,
                    epoch = %a.data().target.epoch,
                    validator_index,
                    "Failed to register observed block attester"
                )
            }
        }
    }
}

/// If a slasher is configured, provide the attestations from the block.
#[instrument(skip_all, level = "debug")]
fn import_block_update_slasher<T: BeaconChainTypes>(
    components: &BeaconChain<T>,
    block: BeaconBlockRef<T::EthSpec>,
    state: &BeaconState<T::EthSpec>,
    ctxt: &mut ConsensusContext<T::EthSpec>,
) {
    if let Some(slasher) = components.block_importer.slasher.as_ref() {
        for attestation in block.body().attestations() {
            let indexed_attestation = match ctxt.get_indexed_attestation(state, attestation) {
                Ok(indexed) => indexed,
                Err(e) => {
                    debug!(
                        purpose = "slasher",
                        attestation_slot = %attestation.data().slot,
                        error = ?e,
                        "Failed to get indexed attestation"
                    );
                    continue;
                }
            };
            slasher.accept_attestation(indexed_attestation.clone_as_indexed_attestation());
        }
    }
}

fn import_block_update_metrics_and_events<T: BeaconChainTypes>(
    importer: &BlockImporter<T>,
    block: BeaconBlockRef<T::EthSpec>,
    block_root: Hash256,
    block_time_imported: Duration,
    payload_verification_status: PayloadVerificationStatus,
    current_slot: Slot,
) {
    // Only present some metrics for blocks from the previous epoch or later.
    //
    // This helps avoid noise in the metrics during sync.
    if block.slot() + 2 * T::EthSpec::slots_per_epoch() >= current_slot {
        metrics::observe(
            &metrics::OPERATIONS_PER_BLOCK_ATTESTATION,
            block.body().attestations_len() as f64,
        );

        if let Ok(sync_aggregate) = block.body().sync_aggregate() {
            metrics::set_gauge(
                &metrics::BLOCK_SYNC_AGGREGATE_SET_BITS,
                sync_aggregate.num_set_bits() as i64,
            );
        }
    }

    let block_delay_total =
        get_slot_delay_ms(block_time_imported, block.slot(), &importer.slot_clock);

    // Do not write to the cache for blocks older than 2 epochs, this helps reduce writes to
    // the cache during sync.
    if block_delay_total < importer.slot_clock.slot_duration() * 64 {
        // Store the timestamp of the block being imported into the cache.
        importer.block_times_cache.write().set_time_imported(
            block_root,
            current_slot,
            block_time_imported,
        );
    }

    if let Some(event_handler) = importer.event_handler.as_ref()
        && event_handler.has_block_subscribers()
    {
        event_handler.register(EventKind::Block(SseBlock {
            slot: block.slot(),
            block: block_root,
            execution_optimistic: payload_verification_status.is_optimistic(),
        }));
    }

    // Do not trigger light_client server update producer for old blocks, to extra work
    // during sync.
    if importer.config.enable_light_client_server
        && block_delay_total < importer.slot_clock.slot_duration() * 32
        && let Some(mut light_client_server_tx) = importer.light_client_server_tx.clone()
        && let Ok(sync_aggregate) = block.body().sync_aggregate()
        && let Err(e) = light_client_server_tx.try_send((
            block.parent_root(),
            block.slot(),
            sync_aggregate.clone(),
        ))
    {
        warn!(
            error = ?e,
            "Failed to send light_client server event"
        );
    }
}

fn emit_sse_blob_sidecar_events<'a, T: BeaconChainTypes, I>(
    components: &BeaconChain<T>,
    block_root: &Hash256,
    blobs_iter: I,
) where
    I: Iterator<Item = &'a BlobSidecar<T::EthSpec>>,
{
    if let Some(event_handler) = components.block_importer.event_handler.as_ref()
        && event_handler.has_blob_sidecar_subscribers()
    {
        let imported_blobs = components
            .data_availability_manager
            .data_availability_checker()
            .cached_blob_indexes(block_root)
            .unwrap_or_default();
        let new_blobs = blobs_iter.filter(|b| !imported_blobs.contains(&b.index));

        for blob in new_blobs {
            event_handler.register(EventKind::BlobSidecar(SseBlobSidecar::from_blob_sidecar(
                blob,
            )));
        }
    }
}

fn emit_sse_data_column_sidecar_events<'a, T: BeaconChainTypes, I>(
    components: &BeaconChain<T>,
    block_root: &Hash256,
    data_columns_iter: I,
) where
    I: Iterator<Item = &'a DataColumnSidecar<T::EthSpec>>,
{
    if let Some(event_handler) = components.block_importer.event_handler.as_ref()
        && event_handler.has_data_column_sidecar_subscribers()
    {
        let imported_data_columns = components
            .data_availability_manager
            .data_availability_checker()
            .cached_data_column_indexes(block_root)
            .unwrap_or_default();
        let new_data_columns =
            data_columns_iter.filter(|b| !imported_data_columns.contains(b.index()));

        for data_column in new_data_columns {
            event_handler.register(EventKind::DataColumnSidecar(
                SseDataColumnSidecar::from_data_column_sidecar(data_column),
            ));
        }
    }
}

fn check_blob_header_signature_and_slashability<'a, T: BeaconChainTypes>(
    components: &BeaconChain<T>,
    block_root: Hash256,
    blobs: impl IntoIterator<Item = &'a BlobSidecar<T::EthSpec>>,
) -> Result<(), BlockError> {
    let mut slashable_cache = components.block_importer.observed_slashable.write();
    for header in blobs
        .into_iter()
        .map(|b| b.signed_block_header.clone())
        .unique()
    {
        // Return an error if *any* header signature is invalid, we do not want to import this
        // list of blobs into the DA checker. However, we will process any valid headers prior
        // to the first invalid header in the slashable cache & slasher.
        verify_header_signature::<T, BlockError>(components, &header)?;

        slashable_cache
            .observe_slashable(
                header.message.slot,
                header.message.proposer_index,
                block_root,
            )
            .map_err(|e| BlockError::BeaconChainError(Box::new(e.into())))?;
        if let Some(slasher) = components.block_importer.slasher.as_ref() {
            slasher.accept_block_header(header);
        }
    }
    Ok(())
}

fn check_data_column_sidecar_header_signature_and_slashability<'a, T: BeaconChainTypes>(
    components: &BeaconChain<T>,
    block_root: Hash256,
    custody_columns: impl IntoIterator<Item = &'a DataColumnSidecarFulu<T::EthSpec>>,
) -> Result<(), BlockError> {
    let mut slashable_cache = components.block_importer.observed_slashable.write();
    // Process all unique block headers - previous logic assumed all headers were identical and
    // only processed the first one. However, we should not make assumptions about data received
    // from RPC.
    for header in custody_columns
        .into_iter()
        .map(|c| c.signed_block_header.clone())
        .unique()
    {
        // Return an error if *any* header signature is invalid, we do not want to import this
        // list of blobs into the DA checker. However, we will process any valid headers prior
        // to the first invalid header in the slashable cache & slasher.
        verify_header_signature::<T, BlockError>(components, &header)?;

        slashable_cache
            .observe_slashable(
                header.message.slot,
                header.message.proposer_index,
                block_root,
            )
            .map_err(|e| BlockError::BeaconChainError(Box::new(e.into())))?;
        if let Some(slasher) = components.block_importer.slasher.as_ref() {
            slasher.accept_block_header(header);
        }
    }
    Ok(())
}

fn handle_import_block_db_write_error<T: BeaconChainTypes>(
    components: &BeaconChain<T>,
    // We don't actually need this value, however it's always present when we call this function
    // and it needs to be dropped to prevent a dead-lock. Requiring it to be passed here is
    // defensive programming.
    fork_choice_write_lock: RwLockWriteGuard<BeaconForkChoice<T>>,
) -> Result<(), BlockError> {
    // Clear the early attester cache to prevent attestations which we would later be unable
    // to verify due to the failure.
    components.attestation_manager.early_attester_cache.clear();

    // Since the write failed, try to revert the canonical head back to what was stored
    // in the database. This attempts to prevent inconsistency between the database and
    // fork choice.
    if let Err(e) = components.canonical_head.restore_from_store(
        fork_choice_write_lock,
        ResetPayloadStatuses::always_reset_conditionally(
            components.config.always_reset_payload_statuses,
        ),
        &components.store,
        &components.spec,
    ) {
        crit!(
            error = ?e,
            warning = "The database is likely corrupt now, consider --purge-db",
            "No stored fork choice found to restore from"
        );
        Err(BlockError::BeaconChainError(Box::new(e)))
    } else {
        Ok(())
    }
}
