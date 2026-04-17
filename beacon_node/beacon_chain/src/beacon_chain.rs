use crate::attestation_manager::AttestationManager;
use crate::beacon_proposer_cache::BeaconProposerCache;
use crate::block_times_cache::BlockTimesCache;
use crate::block_verification::BlockError;
use crate::block_verification_types::RangeSyncBlock;
pub use crate::canonical_head::CanonicalHead;
use crate::chain_config::ChainConfig;
use crate::custody_context::CustodyContextSsz;
use crate::data_availability_checker::{AvailableBlockData, DataAvailabilityChecker};
use crate::data_availability_manager::DataAvailabilityManager;
use crate::envelope_times_cache::EnvelopeTimesCache;
use crate::errors::BeaconChainError as Error;
use crate::events::ServerSentEventHandler;
use crate::execution_manager::ExecutionManager;
use crate::execution_payload::PreparePayloadHandle;
use crate::fork_choice_signal::{ForkChoiceSignalRx, ForkChoiceSignalTx};
use crate::graffiti_calculator::GraffitiCalculator;
use crate::light_client_server_cache::LightClientServerCache;
use crate::migrate::{BackgroundMigrator, ManualFinalizationNotification};
use crate::observed_block_producers::ObservedBlockProducers;
use crate::observed_data_sidecars::ObservedDataSidecars;
use crate::observed_slashable::ObservedSlashable;
use crate::operations_manager::OperationsManager;
use crate::payload_bid_verification::payload_bid_cache::GossipVerifiedPayloadBidCache;
use crate::pending_payload_envelopes::PendingPayloadEnvelopes;
use crate::persisted_custody::persist_custody_context;
use crate::pre_finalization_cache::PreFinalizationBlockCache;
use crate::proposer_preferences_verification::proposer_preference_cache::GossipVerifiedProposerPreferenceCache;
use crate::sync_committee_manager::SyncCommitteeManager;
use crate::validator_monitor::ValidatorMonitor;
use crate::validator_query_service::ValidatorQueryService;
use crate::{BeaconChainError, BeaconForkChoiceStore, metrics};
use bls::Signature;
use execution_layer::{ChainHealth, ExecutionLayer, FailedCondition};
use fork_choice::ForkChoice;
use futures::channel::mpsc::Sender;
use kzg::Kzg;
use logging::crit;
use operation_pool::{OperationPool, PersistedOperationPool};
use parking_lot::{Mutex, RwLock};
use rand::RngCore;
use slasher::Slasher;
use slot_clock::SlotClock;
use state_processing::per_block_processing::errors::AttestationValidationError;
use std::sync::Arc;
use std::time::Duration;
use store::{DatabaseBlock, Error as DBError, HotColdDB, HotStateSummary, StoreOp};
use task_executor::{ShutdownReason, TaskExecutor};
use tracing::{debug, error, info, warn};
use types::data::ColumnIndex;
use types::*;

pub type ForkChoiceError = fork_choice::Error<crate::ForkChoiceStoreError>;

/// Alias to appease clippy.
pub(crate) type HashBlockTuple<E> = (Hash256, RangeSyncBlock<E>);

// These keys are all zero because they get stored in different columns, see `DBColumn` type.
pub const BEACON_CHAIN_DB_KEY: Hash256 = Hash256::ZERO;
pub const OP_POOL_DB_KEY: Hash256 = Hash256::ZERO;
pub const FORK_CHOICE_DB_KEY: Hash256 = Hash256::ZERO;

/// Defines how old a block can be before it's no longer a candidate for the early attester cache.
pub(crate) const EARLY_ATTESTER_CACHE_HISTORIC_SLOTS: u64 = 4;

/// If the head is more than `MAX_PER_SLOT_FORK_CHOICE_DISTANCE` slots behind the wall-clock slot, DO NOT
/// run the per-slot tasks (primarily fork choice).
///
/// This prevents unnecessary work during sync.
///
/// The value is set to 256 since this would be just over one slot (12.8s) when syncing at
/// 20 slots/second. Having a single fork-choice run interrupt syncing would have very little
/// impact whilst having 8 epochs without a block is a comfortable grace period.
const MAX_PER_SLOT_FORK_CHOICE_DISTANCE: u64 = 256;

/// Reported to the user when the justified block has an invalid execution payload.
pub const INVALID_JUSTIFIED_PAYLOAD_SHUTDOWN_REASON: &str =
    "Justified block has an invalid execution payload.";

pub const INVALID_FINALIZED_MERGE_TRANSITION_BLOCK_SHUTDOWN_REASON: &str =
    "Finalized merge transition block is invalid.";

/// Defines the behaviour when a block/block-root for a skipped slot is requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenSlotSkipped {
    /// If the slot is a skip slot, return `None`.
    ///
    /// This is how the HTTP API behaves.
    None,
    /// If the slot is a skip slot, return the previous non-skipped block.
    ///
    /// This is generally how the specification behaves.
    Prev,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AvailabilityProcessingStatus {
    MissingComponents(Slot, Hash256),
    Imported(Hash256),
}

impl TryInto<SignedBeaconBlockHash> for AvailabilityProcessingStatus {
    type Error = ();

    fn try_into(self) -> Result<SignedBeaconBlockHash, Self::Error> {
        match self {
            AvailabilityProcessingStatus::Imported(hash) => Ok(hash.into()),
            _ => Err(()),
        }
    }
}

impl TryInto<Hash256> for AvailabilityProcessingStatus {
    type Error = ();

    fn try_into(self) -> Result<Hash256, Self::Error> {
        match self {
            AvailabilityProcessingStatus::Imported(hash) => Ok(hash),
            _ => Err(()),
        }
    }
}

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

/// Configure the signature verification of produced blocks.
pub enum ProduceBlockVerification {
    VerifyRandao,
    NoVerification,
}

/// Payload attributes for which the `beacon_chain` crate is responsible.
pub struct PrePayloadAttributes {
    pub proposer_index: u64,
    pub prev_randao: Hash256,
    /// The block number of the block being built upon (same block as fcU `headBlockHash`).
    ///
    /// The parent block number is not part of the payload attributes sent to the EL, but *is*
    /// sent to builders via SSE.
    pub parent_block_number: Option<u64>,
    /// The block root of the block being built upon (same block as fcU `headBlockHash`).
    pub parent_beacon_block_root: Hash256,
}

/// Information about a state/block at a specific slot.
#[derive(Debug, Clone, Copy)]
pub struct FinalizationAndCanonicity {
    /// True if the slot of the state or block is finalized.
    ///
    /// This alone DOES NOT imply that the state/block is finalized, use `self.is_finalized()`.
    pub slot_is_finalized: bool,
    /// True if the state or block is canonical at its slot.
    pub canonical: bool,
}

/// Define whether a forkchoiceUpdate needs to be checked for an override (`Yes`) or has already
/// been checked (`AlreadyApplied`). It is safe to specify `Yes` even if re-orgs are disabled.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OverrideForkchoiceUpdate {
    #[default]
    Yes,
    AlreadyApplied,
}

#[derive(Debug, PartialEq)]
pub enum AttestationProcessingOutcome {
    Processed,
    EmptyAggregationBitfield,
    UnknownHeadBlock {
        beacon_block_root: Hash256,
    },
    /// The attestation is attesting to a state that is later than itself. (Viz., attesting to the
    /// future).
    AttestsToFutureBlock {
        block: Slot,
        attestation: Slot,
    },
    /// The slot is finalized, no need to import.
    FinalizedSlot {
        attestation: Slot,
        finalized: Slot,
    },
    FutureEpoch {
        attestation_epoch: Epoch,
        current_epoch: Epoch,
    },
    PastEpoch {
        attestation_epoch: Epoch,
        current_epoch: Epoch,
    },
    BadTargetEpoch,
    UnknownTargetRoot(Hash256),
    InvalidSignature,
    NoCommitteeForSlotAndIndex {
        slot: Slot,
        index: CommitteeIndex,
    },
    Invalid(AttestationValidationError),
}

/// Defines how a `BeaconState` should be "skipped" through skip-slots.
pub enum StateSkipConfig {
    /// Calculate the state root during each skip slot, producing a fully-valid `BeaconState`.
    WithStateRoots,
    /// Don't calculate the state root at each slot, instead just use the zero hash. This is orders
    /// of magnitude faster, however it produces a partially invalid state.
    ///
    /// This state is useful for operations that don't use the state roots; e.g., for calculating
    /// the shuffling.
    WithoutStateRoots,
}

pub trait BeaconChainTypes: Send + Sync + 'static {
    type HotStore: store::ItemStore<Self::EthSpec>;
    type ColdStore: store::ItemStore<Self::EthSpec>;
    type SlotClock: slot_clock::SlotClock;
    type EthSpec: types::EthSpec;
}

pub(crate) struct PartialBeaconBlock<E: EthSpec> {
    pub(crate) state: BeaconState<E>,
    pub(crate) slot: Slot,
    pub(crate) proposer_index: u64,
    pub(crate) parent_root: Hash256,
    pub(crate) randao_reveal: Signature,
    pub(crate) eth1_data: Eth1Data,
    pub(crate) graffiti: Graffiti,
    pub(crate) proposer_slashings: Vec<ProposerSlashing>,
    pub(crate) attester_slashings: Vec<AttesterSlashing<E>>,
    pub(crate) attestations: Vec<Attestation<E>>,
    pub(crate) deposits: Vec<Deposit>,
    pub(crate) voluntary_exits: Vec<SignedVoluntaryExit>,
    pub(crate) sync_aggregate: Option<SyncAggregate<E>>,
    pub(crate) prepare_payload_handle: Option<PreparePayloadHandle<E>>,
    pub(crate) bls_to_execution_changes: Vec<SignedBlsToExecutionChange>,
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

pub type BeaconForkChoice<T> = ForkChoice<
    BeaconForkChoiceStore<
        <T as BeaconChainTypes>::EthSpec,
        <T as BeaconChainTypes>::HotStore,
        <T as BeaconChainTypes>::ColdStore,
    >,
    <T as BeaconChainTypes>::EthSpec,
>;

pub type BeaconStore<T> = Arc<
    HotColdDB<
        <T as BeaconChainTypes>::EthSpec,
        <T as BeaconChainTypes>::HotStore,
        <T as BeaconChainTypes>::ColdStore,
    >,
>;

/// Represents the "Beacon Chain" component of Ethereum 2.0. Allows import of blocks and block
/// operations and chooses a canonical head.
pub struct BeaconChain<T: BeaconChainTypes> {
    pub spec: Arc<ChainSpec>,
    /// Configuration for `BeaconChain` runtime behaviour.
    pub config: ChainConfig,
    /// Persistent storage for blocks, states, etc. Typically an on-disk store, such as LevelDB.
    pub store: BeaconStore<T>,
    /// Used for spawning async and blocking tasks.
    pub task_executor: TaskExecutor,
    /// Database migrator for running background maintenance on the store.
    pub store_migrator: BackgroundMigrator<T::EthSpec, T::HotStore, T::ColdStore>,
    /// Reports the current slot, typically based upon the system clock.
    pub slot_clock: T::SlotClock,
    /// Stores all operations (e.g., `Attestation`, `Deposit`, etc) that are candidates for
    /// inclusion in a block.
    ///
    /// Shared `Arc` also held by `OperationsManager` and `SyncCommitteeManager`.
    /// New code should prefer accessing through those components; this top-level
    /// field exists for callers not yet migrated.
    pub op_pool: Arc<OperationPool<T::EthSpec>>,
    /// Manages attestation pools, observation tracking, and shuffling caches.
    pub attestation_manager: AttestationManager<T::EthSpec>,
    /// Manages voluntary exits, proposer/attester slashings, and BLS-to-execution changes.
    pub operations: OperationsManager<T::EthSpec>,
    /// Manages sync committee message and contribution verification, and the
    /// sync aggregation pool.
    pub sync_committee_manager: SyncCommitteeManager<T::EthSpec>,
    /// Maintains a record of which validators have proposed blocks for each slot.
    pub observed_block_producers: RwLock<ObservedBlockProducers<T::EthSpec>>,
    /// Maintains a record of blob sidecars seen over the gossip network.
    pub observed_blob_sidecars: RwLock<ObservedDataSidecars<BlobSidecar<T::EthSpec>, T::EthSpec>>,
    /// Maintains a record of column sidecars seen over the gossip network.
    pub observed_column_sidecars:
        RwLock<ObservedDataSidecars<DataColumnSidecar<T::EthSpec>, T::EthSpec>>,
    /// Maintains a record of slashable message seen over the gossip network or RPC.
    pub observed_slashable: RwLock<ObservedSlashable<T::EthSpec>>,
    /// Cache of pending execution payload envelopes for local block building.
    /// Envelopes are stored here during block production and eventually published.
    pub pending_payload_envelopes: RwLock<PendingPayloadEnvelopes<T::EthSpec>>,
    /// Interfaces with the execution client.
    ///
    /// Also held by `ExecutionManager`. New code should prefer
    /// `execution_manager.execution_layer()`; this field exists for callers not
    /// yet migrated.
    pub execution_layer: Option<ExecutionLayer<T::EthSpec>>,
    /// Stores information about the canonical head and finalized/justified checkpoints of the
    /// chain. Also contains the fork choice struct, for computing the canonical head.
    pub canonical_head: CanonicalHead<T>,
    /// The root of the genesis block.
    pub genesis_block_root: Hash256,
    /// The root of the genesis state.
    pub genesis_state_root: Hash256,
    /// The root of the list of genesis validators, used during syncing.
    pub genesis_validators_root: Hash256,
    /// Transmitter used to indicate that slot-start fork choice has completed running.
    pub fork_choice_signal_tx: Option<ForkChoiceSignalTx>,
    /// Receiver used by block production to wait on slot-start fork choice.
    pub fork_choice_signal_rx: Option<ForkChoiceSignalRx>,
    /// The genesis time of this `BeaconChain` (seconds since UNIX epoch).
    pub genesis_time: u64,
    /// A handler for events generated by the beacon chain. This is only initialized when the
    /// HTTP server is enabled.
    pub event_handler: Option<ServerSentEventHandler<T::EthSpec>>,
    /// Caches the beacon block proposer shuffling for a given epoch and shuffling key root.
    ///
    /// Shared `Arc` also held by `ExecutionManager`. New code should prefer
    /// `execution_manager.with_proposer_cache()`; this field exists for callers
    /// not yet migrated.
    pub beacon_proposer_cache: Arc<Mutex<BeaconProposerCache>>,
    /// Handles validator public key and index lookups.
    pub validator_query: ValidatorQueryService<T>,
    /// A cache used to keep track of various block timings.
    pub block_times_cache: Arc<RwLock<BlockTimesCache>>,
    /// A cache used to keep track of various envelope timings.
    pub envelope_times_cache: Arc<RwLock<EnvelopeTimesCache>>,
    /// A cache used to track pre-finalization block roots for quick rejection.
    pub pre_finalization_block_cache: PreFinalizationBlockCache,
    /// A cache used to store gossip verified payload bids.
    pub gossip_verified_payload_bid_cache: GossipVerifiedPayloadBidCache<T>,
    /// A cache used to store gossip verified proposer preferences.
    pub gossip_verified_proposer_preferences_cache: GossipVerifiedProposerPreferenceCache,
    /// A cache used to produce light_client server messages
    pub light_client_server_cache: LightClientServerCache<T>,
    /// Sender to signal the light_client server to produce new updates
    pub light_client_server_tx: Option<Sender<LightClientProducerEvent<T::EthSpec>>>,
    /// Sender given to tasks, so that if they encounter a state in which execution cannot
    /// continue they can request that everything shuts down.
    pub shutdown_sender: Sender<ShutdownReason>,
    /// Arbitrary bytes included in the blocks.
    pub(crate) graffiti_calculator: GraffitiCalculator<T>,
    /// Optional slasher.
    pub slasher: Option<Arc<Slasher<T::EthSpec>>>,
    /// Provides monitoring of a set of explicitly defined validators.
    pub validator_monitor: RwLock<ValidatorMonitor<T::EthSpec>>,
    /// The slot at which blocks are downloaded back to.
    pub genesis_backfill_slot: Slot,
    /// Provides a KZG verification and temporary storage for blocks and blobs as
    /// they are collected and combined.
    ///
    /// Shared `Arc` also held by `DataAvailabilityManager`. New code should
    /// prefer accessing through `data_availability_manager`; this field exists
    /// for callers not yet migrated.
    pub data_availability_checker: Arc<DataAvailabilityChecker<T>>,
    /// The KZG trusted setup used by this chain.
    ///
    /// Shared `Arc` also held by `DataAvailabilityManager`. New code should
    /// prefer accessing through `data_availability_manager`; this field exists
    /// for callers not yet migrated.
    pub kzg: Arc<Kzg>,
    /// RNG instance used by the chain. Currently used for shuffling column sidecars in block publishing.
    pub rng: Arc<Mutex<Box<dyn RngCore + Send>>>,
    /// Component managing data availability: DA boundary calculations, custody info,
    /// and blob/column retrieval.
    pub data_availability_manager: Arc<DataAvailabilityManager<T>>,
    /// Component managing execution layer integration, proposer cache, and
    /// fork choice signalling.
    pub execution_manager: Arc<ExecutionManager<T>>,
}

pub enum BeaconBlockResponseWrapper<E: EthSpec> {
    Full(BeaconBlockResponse<E, FullPayload<E>>),
    Blinded(BeaconBlockResponse<E, BlindedPayload<E>>),
}

impl<E: EthSpec> BeaconBlockResponseWrapper<E> {
    pub fn fork_name(&self, spec: &ChainSpec) -> Result<ForkName, InconsistentFork> {
        Ok(match self {
            BeaconBlockResponseWrapper::Full(resp) => resp.block.to_ref().fork_name(spec)?,
            BeaconBlockResponseWrapper::Blinded(resp) => resp.block.to_ref().fork_name(spec)?,
        })
    }

    pub fn execution_payload_value(&self) -> Uint256 {
        match self {
            BeaconBlockResponseWrapper::Full(resp) => resp.execution_payload_value,
            BeaconBlockResponseWrapper::Blinded(resp) => resp.execution_payload_value,
        }
    }

    pub fn consensus_block_value_gwei(&self) -> u64 {
        match self {
            BeaconBlockResponseWrapper::Full(resp) => resp.consensus_block_value,
            BeaconBlockResponseWrapper::Blinded(resp) => resp.consensus_block_value,
        }
    }

    pub fn consensus_block_value_wei(&self) -> Uint256 {
        Uint256::from(self.consensus_block_value_gwei()) * Uint256::from(1_000_000_000)
    }

    pub fn is_blinded(&self) -> bool {
        matches!(self, BeaconBlockResponseWrapper::Blinded(_))
    }
}

/// The components produced when the local beacon node creates a new block to extend the chain
pub struct BeaconBlockResponse<E: EthSpec, Payload: AbstractExecPayload<E>> {
    /// The newly produced beacon block
    pub block: BeaconBlock<E, Payload>,
    /// The post-state after applying the new block
    pub state: BeaconState<E>,
    /// The Blobs / Proofs associated with the new block
    pub blob_items: Option<(KzgProofs<E>, BlobsList<E>)>,
    /// The execution layer reward for the block
    pub execution_payload_value: Uint256,
    /// The consensus layer reward to the proposer
    pub consensus_block_value: u64,
}

impl FinalizationAndCanonicity {
    pub fn is_finalized(self) -> bool {
        self.slot_is_finalized && self.canonical
    }
}

impl<T: BeaconChainTypes> Drop for BeaconChain<T> {
    fn drop(&mut self) {
        let drop = || -> Result<(), Error> {
            self.persist_fork_choice()?;
            persist_op_pool(&self.store, &self.op_pool)?;
            persist_custody_ctx::<T>(&self.spec, &self.data_availability_checker, &self.store)
        };

        if let Err(e) = drop() {
            error!(
                error = ?e,
                "Failed to persist on BeaconChain drop"
            )
        } else {
            info!("Saved beacon chain to disk")
        }
    }
}

impl From<DBError> for Error {
    fn from(e: DBError) -> Error {
        Error::DBError(e)
    }
}

impl From<ForkChoiceError> for Error {
    fn from(e: ForkChoiceError) -> Error {
        Error::ForkChoiceError(e)
    }
}

impl From<BeaconStateError> for Error {
    fn from(e: BeaconStateError) -> Error {
        Error::BeaconStateError(e)
    }
}

impl ChainSegmentResult {
    pub fn into_block_error(self) -> Result<(), BlockError> {
        match self {
            ChainSegmentResult::Failed { error, .. } => Err(error),
            ChainSegmentResult::Successful { .. } => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions: fork digest utilities
// ---------------------------------------------------------------------------

/// Returns the current ENR fork ID for the chain.
///
/// If the slot clock cannot be read, the genesis slot is used.
pub fn enr_fork_id<T: BeaconChainTypes>(
    slot_clock: &T::SlotClock,
    spec: &ChainSpec,
    genesis_validators_root: Hash256,
) -> EnrForkId {
    let slot = slot_clock.now().unwrap_or(spec.genesis_slot);
    spec.enr_fork_id::<T::EthSpec>(slot, genesis_validators_root)
}

/// Returns the fork digest corresponding to an epoch.
///
/// See [`ChainSpec::compute_fork_digest`].
pub fn compute_fork_digest(
    spec: &ChainSpec,
    genesis_validators_root: Hash256,
    epoch: Epoch,
) -> [u8; 4] {
    spec.compute_fork_digest(genesis_validators_root, epoch)
}

/// Calculates the `Duration` to the next fork digest (this could be either a regular or BPO
/// hard fork) if it exists and returns it with its corresponding `Epoch`.
pub fn duration_to_next_digest<T: BeaconChainTypes>(
    slot_clock: &T::SlotClock,
    spec: &ChainSpec,
) -> Option<(Epoch, Duration)> {
    let slot = slot_clock.now().unwrap_or(spec.genesis_slot);
    let epoch = slot.epoch(T::EthSpec::slots_per_epoch());

    let next_digest_epoch = spec.next_digest_epoch(epoch)?;
    let next_digest_slot = next_digest_epoch.start_slot(T::EthSpec::slots_per_epoch());

    slot_clock
        .duration_to_slot(next_digest_slot)
        .map(|duration| (next_digest_epoch, duration))
}

/// Returns the current heads of the `BeaconChain`. For the canonical head, see
/// `CanonicalHead::cached_head`.
///
/// Returns `(block_root, block_slot)`.
pub fn heads<T: BeaconChainTypes>(canonical_head: &CanonicalHead<T>) -> Vec<(Hash256, Slot)> {
    let fork_choice = canonical_head.fork_choice_read_lock();
    fork_choice
        .proto_array()
        .heads_descended_from_finalization::<T::EthSpec>(fork_choice.finalized_checkpoint())
        .iter()
        .map(|node| (node.root(), node.slot()))
        .collect()
}

// ---------------------------------------------------------------------------
// Free functions: methods extracted from `impl BeaconChain<T>`
// ---------------------------------------------------------------------------

/// Persists `op_pool` to disk.
pub fn persist_op_pool<E: EthSpec, Hot: store::ItemStore<E>, Cold: store::ItemStore<E>>(
    store: &HotColdDB<E, Hot, Cold>,
    op_pool: &OperationPool<E>,
) -> Result<(), Error> {
    let _timer = metrics::start_timer(&metrics::PERSIST_OP_POOL);
    store.put_item(
        &OP_POOL_DB_KEY,
        &PersistedOperationPool::from_operation_pool(op_pool),
    )?;
    Ok(())
}

/// Persists the custody information to disk.
pub fn persist_custody_ctx<T: BeaconChainTypes>(
    spec: &ChainSpec,
    data_availability_checker: &DataAvailabilityChecker<T>,
    store: &BeaconStore<T>,
) -> Result<(), Error> {
    if !spec.is_peer_das_scheduled() {
        return Ok(());
    }

    let custody_context: CustodyContextSsz =
        data_availability_checker.custody_context().as_ref().into();

    let CustodyContextSsz {
        validator_custody_at_head,
        epoch_validator_custody_requirements,
        persisted_is_supernode: _,
    } = &custody_context;
    debug!(
        validator_custody_at_head,
        ?epoch_validator_custody_requirements,
        "Persisting custody context to store"
    );

    persist_custody_context::<T::EthSpec, T::HotStore, T::ColdStore>(
        store.clone(),
        custody_context,
    )?;

    Ok(())
}

/// Returns the block at the given root, reconstructing the execution payload from the EL if
/// needed.
pub async fn get_block<T: BeaconChainTypes>(
    store: &BeaconStore<T>,
    execution_layer: Option<&ExecutionLayer<T::EthSpec>>,
    spec: &ChainSpec,
    block_root: &Hash256,
) -> Result<Option<SignedBeaconBlock<T::EthSpec>>, Error> {
    let blinded_block = match store.try_get_full_block(block_root)? {
        Some(DatabaseBlock::Full(block)) => return Ok(Some(block)),
        Some(DatabaseBlock::Blinded(block)) => block,
        None => return Ok(None),
    };
    let fork = blinded_block.fork_name(spec)?;

    let block_message = blinded_block.message();
    let execution_payload_header = block_message
        .execution_payload()
        .map_err(|_| Error::BlockVariantLacksExecutionPayload(*block_root))?
        .to_execution_payload_header();

    let exec_block_hash = execution_payload_header.block_hash();

    let execution_payload = execution_layer
        .ok_or(Error::ExecutionLayerMissing)?
        .get_payload_for_header(&execution_payload_header, fork)
        .await
        .map_err(|e| Error::ExecutionLayerErrorPayloadReconstruction(exec_block_hash, Box::new(e)))?
        .ok_or(Error::BlockHashMissingFromExecutionLayer(exec_block_hash))?;

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

    blinded_block
        .try_into_full_block(Some(execution_payload))
        .ok_or(Error::AddPayloadLogicError)
        .map(Some)
}

/// Spawn a blocking task via the task executor.
pub async fn spawn_blocking_handle<F, R>(
    task_executor: &TaskExecutor,
    task: F,
    name: &'static str,
) -> Result<R, Error>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let handle = task_executor
        .spawn_blocking_handle(task, name)
        .ok_or(Error::RuntimeShutdown)?;
    handle.await.map_err(Error::TokioJoin)
}

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
        chain.block_times_cache.write().prune(slot);
        chain.envelope_times_cache.write().prune(slot);
        chain.gossip_verified_payload_bid_cache.prune(slot);
        chain.gossip_verified_proposer_preferences_cache.prune(slot);

        if chain.best_slot() + MAX_PER_SLOT_FORK_CHOICE_DISTANCE < slot {
            return;
        }

        chain.recompute_head_at_current_slot().await;

        let chain_clone = chain.clone();
        chain.task_executor.clone().spawn_blocking(
            move || {
                if let Some(tx) = &chain_clone.fork_choice_signal_tx
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

/// Returns data columns for the given block root, checking all caches first.
pub fn get_data_columns_checking_all_caches<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    block_root: Hash256,
    indices: &[ColumnIndex],
) -> Result<DataColumnSidecarList<T::EthSpec>, Error> {
    let all_cached_columns_opt = chain
        .data_availability_checker
        .get_data_columns(block_root)
        .or_else(|| {
            chain
                .attestation_manager
                .early_attester_cache
                .get_data_columns(block_root)
        });

    if let Some(mut all_cached_columns) = all_cached_columns_opt {
        all_cached_columns.retain(|col| indices.contains(col.index()));
        Ok(all_cached_columns)
    } else if let Some(block) = chain.store.get_blinded_block(&block_root)? {
        indices
            .iter()
            .filter_map(|index| {
                chain
                    .data_availability_manager
                    .get_data_column(&block_root, index, block.fork_name_unchecked())
                    .transpose()
            })
            .collect::<Result<_, _>>()
    } else {
        Ok(vec![])
    }
}

/// Returns a store op for writing blobs or data columns, filtering by custody columns.
pub fn get_blobs_or_columns_store_op<'a, T: BeaconChainTypes>(
    data_availability_manager: &DataAvailabilityManager<T>,
    spec: &ChainSpec,
    block_root: Hash256,
    block_slot: Slot,
    block_data: AvailableBlockData<T::EthSpec>,
) -> Option<StoreOp<'a, T::EthSpec>> {
    match block_data {
        AvailableBlockData::NoData => None,
        AvailableBlockData::Blobs(blobs) => {
            debug!(
                %block_root,
                count = blobs.len(),
                "Writing blobs to store"
            );
            Some(StoreOp::PutBlobs(block_root, blobs))
        }
        AvailableBlockData::DataColumns(mut data_columns) => {
            let columns_to_custody = data_availability_manager
                .custody_columns_for_epoch(Some(block_slot.epoch(T::EthSpec::slots_per_epoch())));
            if columns_to_custody.len() != spec.number_of_custody_groups as usize {
                data_columns.retain(|data_column| columns_to_custody.contains(data_column.index()));
            }
            debug!(
                %block_root,
                count = data_columns.len(),
                "Writing data columns to store"
            );
            Some(StoreOp::PutDataColumns(block_root, data_columns))
        }
    }
}

/// Determine chain health for builder fallback decisions.
pub fn is_healthy<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    parent_root: &Hash256,
) -> Result<ChainHealth, Error> {
    let cached_head = chain.canonical_head.cached_head();
    if let Some(head_hash) = cached_head.forkchoice_update_parameters().head_hash {
        if ExecutionBlockHash::zero() == head_hash {
            return Ok(ChainHealth::PreMerge);
        }
    } else {
        return Ok(ChainHealth::PreMerge);
    };

    if let Some(execution_status) = chain
        .canonical_head
        .fork_choice_read_lock()
        .get_block_execution_status(parent_root)
        && execution_status.is_strictly_optimistic()
    {
        return Ok(ChainHealth::Optimistic);
    }

    if chain.config.builder_fallback_disable_checks {
        return Ok(ChainHealth::Healthy);
    }

    let current_slot = chain.slot_clock.now().ok_or(Error::UnableToReadSlot)?;

    let prev_slot = current_slot.saturating_sub(Slot::new(1));
    let head_skips = prev_slot.saturating_sub(cached_head.head_slot());
    let head_skips_check = head_skips.as_usize() <= chain.config.builder_fallback_skips;

    let current_epoch = current_slot.epoch(T::EthSpec::slots_per_epoch());
    let epochs_since_finalization =
        current_epoch.saturating_sub(cached_head.finalized_checkpoint().epoch);
    let finalization_check = epochs_since_finalization.as_usize()
        <= chain.config.builder_fallback_epochs_since_finalization;

    let start_slot = current_slot.saturating_sub(T::EthSpec::slots_per_epoch());
    let mut epoch_skips = 0;
    for slot in start_slot.as_u64()..current_slot.as_u64() {
        if chain
            .block_root_at_slot(Slot::new(slot), WhenSlotSkipped::None)?
            .is_none()
        {
            epoch_skips += 1;
        }
    }
    let epoch_skips_check = epoch_skips <= chain.config.builder_fallback_skips_per_epoch;

    if !head_skips_check {
        Ok(ChainHealth::Unhealthy(FailedCondition::Skips))
    } else if !finalization_check {
        Ok(ChainHealth::Unhealthy(
            FailedCondition::EpochsSinceFinalization,
        ))
    } else if !epoch_skips_check {
        Ok(ChainHealth::Unhealthy(FailedCondition::SkipsPerEpoch))
    } else {
        Ok(ChainHealth::Healthy)
    }
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

        match chain.root_at_slot_from_state(slot, beacon_block_root, state)? {
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

/// Checks if attestations have been seen from the given `validator_index` at the given `epoch`.
pub fn validator_seen_at_epoch<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    validator_index: usize,
    epoch: Epoch,
) -> bool {
    let attested_or_aggregated = chain
        .attestation_manager
        .validator_seen_at_epoch(validator_index, epoch);
    let produced_block = chain
        .observed_block_producers
        .read()
        .index_seen_at_epoch(validator_index as u64, epoch);
    attested_or_aggregated || produced_block
}

/// Gets the `LightClientBootstrap` object for a requested block root.
#[allow(clippy::type_complexity)]
pub fn get_light_client_bootstrap<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    block_root: &Hash256,
) -> Result<Option<(LightClientBootstrap<T::EthSpec>, ForkName)>, Error> {
    let head_state = &chain.head().snapshot.beacon_state;
    let finalized_period = head_state
        .finalized_checkpoint()
        .epoch
        .sync_committee_period(&chain.spec)?;
    chain.light_client_server_cache.get_light_client_bootstrap(
        &chain.store,
        block_root,
        finalized_period,
        &chain.spec,
    )
}

/// Finalize the state at the given root via the background migrator.
pub fn manually_finalize_state<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    state_root: Hash256,
    checkpoint: Checkpoint,
) -> Result<(), Error> {
    let HotStateSummary {
        slot,
        latest_block_root,
        ..
    } = chain
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

    chain.store_migrator.process_manual_finalization(notif);
    Ok(())
}

/// Check that the shuffling at `block_root` is equal to one of the shufflings of `state`.
///
/// This is a free function extracted from `BeaconChain` to avoid coupling to `self`. It loads the
/// block's shuffling ID from fork choice and delegates to `AttestationManager::shuffling_is_compatible`.
pub fn shuffling_is_compatible_with_fork_choice<T: BeaconChainTypes>(
    block_root: &Hash256,
    target_epoch: Epoch,
    state: &BeaconState<T::EthSpec>,
    canonical_head: &CanonicalHead<T>,
    attestation_manager: &AttestationManager<T::EthSpec>,
) -> bool {
    let result = (|| -> Result<bool, Error> {
        let fork_choice_lock = canonical_head.fork_choice_read_lock();
        let block = fork_choice_lock
            .get_block(block_root)
            .ok_or(Error::AttestationHeadNotInForkChoice(*block_root))?;
        drop(fork_choice_lock);

        let block_shuffling_id = if target_epoch == block.current_epoch_shuffling_id.shuffling_epoch
        {
            block.current_epoch_shuffling_id
        } else if target_epoch == block.next_epoch_shuffling_id.shuffling_epoch {
            block.next_epoch_shuffling_id
        } else if target_epoch > block.next_epoch_shuffling_id.shuffling_epoch {
            AttestationShufflingId {
                shuffling_epoch: target_epoch,
                shuffling_decision_block: *block_root,
            }
        } else {
            debug!(
                ?block_root,
                %target_epoch,
                reason = "target epoch less than block epoch",
                "Skipping attestation with incompatible shuffling"
            );
            return Ok(false);
        };

        Ok(attestation_manager.shuffling_is_compatible(
            block_root,
            target_epoch,
            state,
            block_shuffling_id,
        ))
    })();

    result.unwrap_or_else(|e| {
        debug!(
            ?block_root,
            %target_epoch,
            reason = ?e,
            "Skipping attestation with incompatible shuffling"
        );
        false
    })
}
