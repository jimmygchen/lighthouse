use crate::attestation_manager::AttestationManager;
use crate::attestation_verification::{
    Error as AttestationError, VerifiedAggregatedAttestation, VerifiedAttestation,
    VerifiedUnaggregatedAttestation, batch_verify_aggregated_attestations,
    batch_verify_unaggregated_attestations,
};
use crate::beacon_block_streamer::{BeaconBlockStreamer, CheckCaches};
use crate::beacon_proposer_cache::BeaconProposerCache;
use crate::blob_verification::{GossipBlobError, GossipVerifiedBlob};
use crate::block_import_state::BlockImportState;
use crate::block_times_cache::BlockTimesCache;
use crate::block_verification::{
    BlockError, ExecutionPendingBlock, GossipVerifiedBlock, IntoExecutionPendingBlock,
    check_block_is_finalized_checkpoint_or_descendant, check_block_relevancy,
    signature_verify_chain_segment, verify_header_signature,
};
use crate::block_verification_types::{
    AsBlock, AvailableExecutedBlock, BlockImportData, ExecutedBlock, RangeSyncBlock,
};
pub use crate::canonical_head::CanonicalHead;
use crate::chain_config::ChainConfig;
use crate::custody_context::CustodyContextSsz;
use crate::data_availability_checker::{
    Availability, AvailabilityCheckError, AvailableBlock, AvailableBlockData,
    DataAvailabilityChecker, DataColumnReconstructionResult,
};
use crate::data_availability_manager::DataAvailabilityManager;
use crate::data_column_verification::{GossipDataColumnError, GossipVerifiedDataColumn};
use crate::envelope_times_cache::EnvelopeTimesCache;
use crate::errors::{BeaconChainError as Error, BlockProductionError};
use crate::events::ServerSentEventHandler;
use crate::execution_manager::ExecutionManager;
use crate::execution_payload::{NotifyExecutionLayer, PreparePayloadHandle, get_execution_payload};
use crate::fetch_blobs::EngineGetBlobsOutput;
use crate::fork_choice_signal::{ForkChoiceSignalRx, ForkChoiceSignalTx};
use crate::graffiti_calculator::{GraffitiCalculator, GraffitiSettings};
use crate::light_client_finality_update_verification::{
    Error as LightClientFinalityUpdateError, VerifiedLightClientFinalityUpdate,
};
use crate::light_client_optimistic_update_verification::{
    Error as LightClientOptimisticUpdateError, VerifiedLightClientOptimisticUpdate,
};
use crate::light_client_server_cache::LightClientServerCache;
use crate::migrate::{BackgroundMigrator, ManualFinalizationNotification};
use crate::observed_aggregates::Error as AttestationObservationError;
use crate::observed_block_producers::ObservedBlockProducers;
use crate::observed_data_sidecars::ObservedDataSidecars;
use crate::observed_operations::ObservationOutcome;
use crate::observed_slashable::ObservedSlashable;
use crate::operations_manager::OperationsManager;
use crate::payload_bid_verification::payload_bid_cache::GossipVerifiedPayloadBidCache;
#[cfg(not(test))]
use crate::payload_envelope_streamer::{EnvelopeRequestSource, launch_payload_envelope_stream};
use crate::pending_payload_envelopes::PendingPayloadEnvelopes;
use crate::persisted_beacon_chain::PersistedBeaconChain;
use crate::persisted_custody::persist_custody_context;
use crate::persisted_fork_choice::PersistedForkChoice;
use crate::pre_finalization_cache::PreFinalizationBlockCache;
use crate::proposer_preferences_verification::proposer_preference_cache::GossipVerifiedProposerPreferenceCache;
use crate::shuffling_cache::BlockShufflingIds;
use crate::sync_committee_manager::SyncCommitteeManager;
use crate::sync_committee_verification::{
    Error as SyncCommitteeError, VerifiedSyncCommitteeMessage, VerifiedSyncContribution,
};
use crate::validator_monitor::{
    HISTORIC_EPOCHS as VALIDATOR_MONITOR_HISTORIC_EPOCHS, ValidatorMonitor, get_slot_delay_ms,
};
use crate::validator_query_service::ValidatorQueryService;
use crate::{
    AvailabilityPendingExecutedBlock, BeaconChainError, BeaconForkChoiceStore, BeaconSnapshot,
    CachedHead, metrics,
};
use bls::Signature;
use eth2::beacon_response::ForkVersionedResponse;
use eth2::types::{
    EventKind, SseBlobSidecar, SseBlock, SseDataColumnSidecar, SseExtendedPayloadAttributes,
    SseHead,
};
use execution_layer::{
    BlockProposalContents, BlockProposalContentsType, BuilderParams, ChainHealth, ExecutionLayer,
    FailedCondition, PayloadAttributes, PayloadStatus,
};
use fixed_bytes::FixedBytesExtended;
use fork_choice::{
    AttestationFromBlock, ExecutionStatus, ForkChoice, ForkchoiceUpdateParameters,
    InvalidationOperation, PayloadVerificationStatus, ResetPayloadStatuses,
};
use futures::channel::mpsc::Sender;
use itertools::Itertools;
use itertools::process_results;
use kzg::Kzg;
use logging::crit;
use operation_pool::{CompactAttestationRef, OperationPool, PersistedOperationPool};
use parking_lot::{Mutex, RwLock, RwLockWriteGuard};
use proto_array::{DoNotReOrg, ProposerHeadError};
use rand::RngCore;
use safe_arith::SafeArith;
use slasher::Slasher;
use slot_clock::SlotClock;
use ssz::Encode;
use state_processing::{
    BlockSignatureStrategy, ConsensusContext, VerifyBlockRoot, VerifyOperation,
    common::get_attesting_indices_from_state,
    epoch_cache::initialize_epoch_cache,
    per_block_processing,
    per_block_processing::{
        VerifySignatures, errors::AttestationValidationError, get_expected_withdrawals,
        verify_attestation_for_block_inclusion,
    },
    per_slot_processing,
    state_advance::{complete_state_advance, partial_state_advance},
};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::prelude::*;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;
use store::iter::{BlockRootsIterator, ParentRootBlockIterator, StateRootsIterator};
use store::{
    BlobSidecarListFromRoot, DBColumn, DatabaseBlock, Error as DBError, HotColdDB, HotStateSummary,
    KeyValueStore, KeyValueStoreOp, StoreItem, StoreOp,
};
use task_executor::{RayonPoolType, ShutdownReason, TaskExecutor};
use tokio_stream::Stream;
use tracing::{debug, debug_span, error, info, info_span, instrument, trace, warn};
use tree_hash::TreeHash;
use types::data::{ColumnIndex, FixedBlobSidecarList};
use types::execution::BlockProductionVersion;
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
    pub data_availability_checker: Arc<DataAvailabilityChecker<T>>,
    /// The KZG trusted setup used by this chain.
    pub kzg: Arc<Kzg>,
    /// RNG instance used by the chain. Currently used for shuffling column sidecars in block publishing.
    pub rng: Arc<Mutex<Box<dyn RngCore + Send>>>,
    /// Component managing data availability: DA boundary calculations, custody info,
    /// and blob/column retrieval.
    pub data_availability_manager: Arc<DataAvailabilityManager<T>>,
    /// Component managing execution layer integration, proposer cache, and
    /// fork choice signalling.
    pub execution_manager: Arc<ExecutionManager<T>>,
    /// Block import state: timing caches, observed block producers,
    /// and observed slashable tracking.
    pub block_import_state: BlockImportState<T::EthSpec>,
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

impl<T: BeaconChainTypes> BeaconChain<T> {
    /// Return a database operation for writing the `PersistedBeaconChain` to disk.
    ///
    /// These days the `PersistedBeaconChain` is only used to store the genesis block root, so it
    /// should only ever be written once at startup. It used to be written more frequently, but
    /// this is no longer necessary.
    pub fn persist_head_in_batch_standalone(genesis_block_root: Hash256) -> KeyValueStoreOp {
        PersistedBeaconChain { genesis_block_root }.as_kv_store_op(BEACON_CHAIN_DB_KEY)
    }

    /// Load fork choice from disk, returning `None` if it isn't found.
    pub fn load_fork_choice(
        store: BeaconStore<T>,
        reset_payload_statuses: ResetPayloadStatuses,
        spec: &ChainSpec,
    ) -> Result<Option<BeaconForkChoice<T>>, Error> {
        let Some(persisted_fork_choice_bytes) = store
            .hot_db
            .get_bytes(DBColumn::ForkChoice, FORK_CHOICE_DB_KEY.as_slice())?
        else {
            return Ok(None);
        };

        let persisted_fork_choice =
            PersistedForkChoice::from_bytes(&persisted_fork_choice_bytes, store.get_config())?;
        let fc_store =
            BeaconForkChoiceStore::from_persisted(persisted_fork_choice.fork_choice_store, store)?;

        Ok(Some(ForkChoice::from_persisted(
            persisted_fork_choice.fork_choice,
            reset_payload_statuses,
            fc_store,
            spec,
        )?))
    }

    /// Persists `self.op_pool` to disk.
    ///
    /// ## Notes
    ///
    /// This operation is typically slow and causes a lot of allocations. It should be used
    /// sparingly.
    pub fn persist_op_pool(&self) -> Result<(), Error> {
        let _timer = metrics::start_timer(&metrics::PERSIST_OP_POOL);

        self.store.put_item(
            &OP_POOL_DB_KEY,
            &PersistedOperationPool::from_operation_pool(&self.op_pool),
        )?;

        Ok(())
    }

    /// Persists the custody information to disk.
    pub fn persist_custody_context(&self) -> Result<(), Error> {
        if !self.spec.is_peer_das_scheduled() {
            return Ok(());
        }

        let custody_context: CustodyContextSsz = self
            .data_availability_checker
            .custody_context()
            .as_ref()
            .into();

        // Pattern match to avoid accidentally missing fields and to ignore deprecated fields.
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
            self.store.clone(),
            custody_context,
        )?;

        Ok(())
    }

    /// Returns the slot _right now_ according to `self.slot_clock`. Returns `Err` if the slot is
    /// unavailable.
    ///
    /// The slot might be unavailable due to an error with the system clock, or if the present time
    /// is before genesis (i.e., a negative slot).
    pub fn slot(&self) -> Result<Slot, Error> {
        self.slot_clock.now().ok_or(Error::UnableToReadSlot)
    }

    /// Returns the epoch _right now_ according to `self.slot_clock`. Returns `Err` if the epoch is
    /// unavailable.
    ///
    /// The epoch might be unavailable due to an error with the system clock, or if the present time
    /// is before genesis (i.e., a negative epoch).
    pub fn epoch(&self) -> Result<Epoch, Error> {
        self.slot()
            .map(|slot| slot.epoch(T::EthSpec::slots_per_epoch()))
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
        } else if request_slot > self.slot()? {
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
        } else if request_slot > self.slot()? {
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
        } else if request_slot > self.slot()? {
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

    pub fn get_blobs_checking_early_attester_cache(
        &self,
        block_root: &Hash256,
    ) -> Result<BlobSidecarListFromRoot<T::EthSpec>, Error> {
        self.attestation_manager
            .early_attester_cache
            .get_blobs(*block_root)
            .map(Into::into)
            .map_or_else(|| self.data_availability_manager.get_blobs(block_root), Ok)
    }

    #[cfg(not(test))]
    #[allow(clippy::type_complexity)]
    pub fn get_payload_envelopes(
        self: &Arc<Self>,
        block_roots: Vec<Hash256>,
        request_source: EnvelopeRequestSource,
    ) -> impl Stream<
        Item = (
            Hash256,
            Arc<Result<Option<Arc<SignedExecutionPayloadEnvelope<T::EthSpec>>>, Error>>,
        ),
    > {
        launch_payload_envelope_stream(self.clone(), block_roots, request_source)
    }

    pub fn get_data_columns_checking_all_caches(
        &self,
        block_root: Hash256,
        indices: &[ColumnIndex],
    ) -> Result<DataColumnSidecarList<T::EthSpec>, Error> {
        let all_cached_columns_opt = self
            .data_availability_checker
            .get_data_columns(block_root)
            .or_else(|| {
                self.attestation_manager
                    .early_attester_cache
                    .get_data_columns(block_root)
            });

        if let Some(mut all_cached_columns) = all_cached_columns_opt {
            all_cached_columns.retain(|col| indices.contains(col.index()));
            Ok(all_cached_columns)
        } else if let Some(block) = self.get_blinded_block(&block_root)? {
            indices
                .iter()
                .filter_map(|index| {
                    self.data_availability_manager
                        .get_data_column(&block_root, index, block.fork_name_unchecked())
                        .transpose()
                })
                .collect::<Result<_, _>>()
        } else {
            Ok(vec![])
        }
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

    pub fn get_payload_envelope(
        &self,
        block_root: &Hash256,
    ) -> Result<Option<SignedExecutionPayloadEnvelope<T::EthSpec>>, Error> {
        Ok(self.store.get_payload_envelope(block_root)?)
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

    /// Return the sync committee at `slot + 1` from the canonical chain.
    ///
    /// This is useful when dealing with sync committee messages, because messages are signed
    /// and broadcast one slot prior to the slot of the sync committee (which is relevant at
    /// sync committee period boundaries).
    pub fn sync_committee_at_next_slot(
        &self,
        slot: Slot,
    ) -> Result<Arc<SyncCommittee<T::EthSpec>>, Error> {
        let epoch = slot.safe_add(1)?.epoch(T::EthSpec::slots_per_epoch());
        self.sync_committee_at_epoch(epoch)
    }

    /// Return the sync committee at `epoch` from the canonical chain.
    pub fn sync_committee_at_epoch(
        &self,
        epoch: Epoch,
    ) -> Result<Arc<SyncCommittee<T::EthSpec>>, Error> {
        // Try to read a committee from the head. This will work most of the time, but will fail
        // for faraway committees, or if there are skipped slots at the transition to Altair.
        let spec = &self.spec;
        let committee_from_head =
            self.with_head(
                |head| match head.beacon_state.get_built_sync_committee(epoch, spec) {
                    Ok(committee) => Ok(Some(committee.clone())),
                    Err(BeaconStateError::SyncCommitteeNotKnown { .. })
                    | Err(BeaconStateError::IncorrectStateVariant) => Ok(None),
                    Err(e) => Err(Error::from(e)),
                },
            )?;

        if let Some(committee) = committee_from_head {
            Ok(committee)
        } else {
            // Slow path: load a state (or advance the head).
            let sync_committee_period = epoch.sync_committee_period(spec)?;
            let committee = self
                .state_for_sync_committee_period(sync_committee_period)?
                .get_built_sync_committee(epoch, spec)?
                .clone();
            Ok(committee)
        }
    }

    /// Load a state suitable for determining the sync committee for the given period.
    ///
    /// Specifically, the state at the start of the *previous* sync committee period.
    ///
    /// This is sufficient for historical duties, and efficient in the case where the head
    /// is lagging the current period and we need duties for the next period (because we only
    /// have to transition the head to start of the current period).
    ///
    /// We also need to ensure that the load slot is after the Altair fork.
    ///
    /// **WARNING**: the state returned will have dummy state roots. It should only be used
    /// for its sync committees (determining duties, etc).
    pub fn state_for_sync_committee_period(
        &self,
        sync_committee_period: u64,
    ) -> Result<BeaconState<T::EthSpec>, Error> {
        let altair_fork_epoch = self
            .spec
            .altair_fork_epoch
            .ok_or(Error::AltairForkDisabled)?;

        let load_slot = std::cmp::max(
            self.spec.epochs_per_sync_committee_period * sync_committee_period.saturating_sub(1),
            altair_fork_epoch,
        )
        .start_slot(T::EthSpec::slots_per_epoch());

        self.state_at_slot(load_slot, StateSkipConfig::WithoutStateRoots)
    }

    pub fn recompute_and_cache_light_client_updates(
        &self,
        (parent_root, slot, sync_aggregate): LightClientProducerEvent<T::EthSpec>,
    ) -> Result<(), Error> {
        self.light_client_server_cache.recompute_and_cache_updates(
            self.store.clone(),
            slot,
            &parent_root,
            &sync_aggregate,
            &self.spec,
        )
    }

    pub fn get_light_client_updates(
        &self,
        sync_committee_period: u64,
        count: u64,
    ) -> Result<Vec<LightClientUpdate<T::EthSpec>>, Error> {
        self.light_client_server_cache.get_light_client_updates(
            &self.store,
            sync_committee_period,
            count,
            &self.spec,
        )
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
        self.state_at_slot(self.slot()?, StateSkipConfig::WithStateRoots)
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

    /// Returns the attestation duties for the given validator indices using the shuffling cache.
    ///
    /// An error may be returned if `head_block_root` is a finalized block, this function is only
    /// designed for operations at the head of the chain.
    ///
    /// The returned `Vec` will have the same length as `validator_indices`, any
    /// non-existing/inactive validators will have `None` values.
    ///
    /// ## Notes
    ///
    /// This function will try to use the shuffling cache to return the value. If the value is not
    /// in the shuffling cache, it will be added. Care should be taken not to wash out the
    /// shuffling cache with historical/useless values.
    pub fn validator_attestation_duties(
        &self,
        validator_indices: &[u64],
        epoch: Epoch,
        head_block_root: Hash256,
    ) -> Result<(Vec<Option<AttestationDuty>>, Hash256, ExecutionStatus), Error> {
        let execution_status = self
            .canonical_head
            .fork_choice_read_lock()
            .get_block_execution_status(&head_block_root)
            .ok_or(Error::AttestationHeadNotInForkChoice(head_block_root))?;

        let (duties, dependent_root) = self.with_committee_cache(
            head_block_root,
            epoch,
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

    pub fn get_aggregated_attestation(
        &self,
        attestation: AttestationRef<T::EthSpec>,
    ) -> Result<Option<Attestation<T::EthSpec>>, Error> {
        match attestation {
            AttestationRef::Base(att) => self.get_aggregated_attestation_base(&att.data),
            AttestationRef::Electra(att) => self.get_aggregated_attestation_electra(
                att.data.slot,
                &att.data.tree_hash_root(),
                att.committee_index()
                    .ok_or(Error::AttestationCommitteeIndexNotSet)?,
            ),
        }
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

    /// Returns an aggregated `Attestation`, if any, that has a matching `attestation.data`.
    ///
    /// The attestation will be obtained from `self.attestation_manager.naive_aggregation_pool`.
    pub fn get_aggregated_attestation_base(
        &self,
        data: &AttestationData,
    ) -> Result<Option<Attestation<T::EthSpec>>, Error> {
        let attestation_key = crate::naive_aggregation_pool::AttestationKey::new_base(data);
        if let Some(attestation) = self
            .attestation_manager
            .naive_aggregation_pool
            .read()
            .get(&attestation_key)
        {
            self.filter_optimistic_attestation(attestation)
                .map(Option::Some)
        } else {
            Ok(None)
        }
    }

    pub fn get_aggregated_attestation_electra(
        &self,
        slot: Slot,
        attestation_data_root: &Hash256,
        committee_index: CommitteeIndex,
    ) -> Result<Option<Attestation<T::EthSpec>>, Error> {
        let attestation_key = crate::naive_aggregation_pool::AttestationKey::new_electra(
            slot,
            *attestation_data_root,
            committee_index,
        );
        if let Some(attestation) = self
            .attestation_manager
            .naive_aggregation_pool
            .read()
            .get(&attestation_key)
        {
            self.filter_optimistic_attestation(attestation)
                .map(Option::Some)
        } else {
            Ok(None)
        }
    }

    /// Returns an aggregated `Attestation`, if any, that has a matching
    /// `attestation.data.tree_hash_root()`.
    ///
    /// The attestation will be obtained from `self.attestation_manager.naive_aggregation_pool`.
    ///
    /// NOTE: This function will *only* work with pre-electra attestations and it only
    ///       exists to support the pre-electra validator API method.
    pub fn get_pre_electra_aggregated_attestation_by_slot_and_root(
        &self,
        slot: Slot,
        attestation_data_root: &Hash256,
    ) -> Result<Option<Attestation<T::EthSpec>>, Error> {
        let attestation_key =
            crate::naive_aggregation_pool::AttestationKey::new_base_from_slot_and_root(
                slot,
                *attestation_data_root,
            );

        if let Some(attestation) = self
            .attestation_manager
            .naive_aggregation_pool
            .read()
            .get(&attestation_key)
        {
            self.filter_optimistic_attestation(attestation)
                .map(Option::Some)
        } else {
            Ok(None)
        }
    }

    /// Returns `Ok(attestation)` if the supplied `attestation` references a valid
    /// `beacon_block_root`.
    fn filter_optimistic_attestation(
        &self,
        attestation: Attestation<T::EthSpec>,
    ) -> Result<Attestation<T::EthSpec>, Error> {
        let beacon_block_root = attestation.data().beacon_block_root;
        match self
            .canonical_head
            .fork_choice_read_lock()
            .get_block_execution_status(&beacon_block_root)
        {
            // The attestation references a block that is not in fork choice, it must be
            // pre-finalization.
            None => Err(Error::CannotAttestToFinalizedBlock { beacon_block_root }),
            // The attestation references a fully valid `beacon_block_root`.
            Some(execution_status) if execution_status.is_valid_or_irrelevant() => Ok(attestation),
            // The attestation references a block that has not been verified by an EL (i.e. it
            // is optimistic or invalid). Don't return the block, return an error instead.
            Some(execution_status) => Err(Error::HeadBlockNotFullyVerified {
                beacon_block_root,
                execution_status,
            }),
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
    #[instrument(name = "lh_produce_unaggregated_attestation", skip_all, fields(%request_slot, %request_index), level = "debug")]
    pub fn produce_unaggregated_attestation(
        &self,
        request_slot: Slot,
        request_index: CommitteeIndex,
    ) -> Result<Attestation<T::EthSpec>, Error> {
        let _total_timer = metrics::start_timer(&metrics::ATTESTATION_PRODUCTION_SECONDS);

        // The early attester cache will return `Some(attestation)` in the scenario where there is a
        // block being imported that will become the head block, but that block has not yet been
        // inserted into the database and set as `self.canonical_head`.
        //
        // In effect, the early attester cache prevents slow database IO from causing missed
        // head/target votes.
        //
        // The early attester cache should never contain an optimistically imported block.
        match self.attestation_manager.early_attester_cache.try_attest(
            request_slot,
            request_index,
            &self.spec,
        ) {
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

        let slots_per_epoch = T::EthSpec::slots_per_epoch();
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
        let head_span = debug_span!("attestation_production_head_scrape").entered();
        // The following braces are to prevent the `cached_head` Arc from being held for longer than
        // required. It also helps reduce the diff for a very large PR (#3244).
        {
            let head = self.head_snapshot();
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
            let slots_per_historical_root = T::EthSpec::slots_per_historical_root() as u64;
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

            let target_slot = request_epoch.start_slot(T::EthSpec::slots_per_epoch());
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
        drop(head_span);
        drop(head_timer);

        // Only attest to a block if it is fully verified (i.e. not optimistic or invalid).
        match self
            .canonical_head
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
                let (advanced_state_root, mut state) = self
                    .store
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
                        request_epoch.start_slot(T::EthSpec::slots_per_epoch()),
                        &self.spec,
                    )
                    .map_err(Error::StateAdvanceError)?;

                    state.build_committee_cache(RelativeEpoch::Current, &self.spec)?;
                }

                (
                    state.current_justified_checkpoint(),
                    state
                        .get_beacon_committee(request_slot, request_index)?
                        .committee
                        .len(),
                )
            };

        Ok(Attestation::<T::EthSpec>::empty_for_signing(
            request_index,
            committee_len,
            request_slot,
            beacon_block_root,
            justified_checkpoint,
            target,
            &self.spec,
        )?)
    }

    /// Performs the same validation as `Self::verify_unaggregated_attestation_for_gossip`, but for
    /// multiple attestations using batch BLS verification. Batch verification can provide
    /// significant CPU-time savings compared to individual verification.
    pub fn batch_verify_unaggregated_attestations_for_gossip<'a, I>(
        &self,
        attestations: I,
    ) -> Result<
        Vec<Result<VerifiedUnaggregatedAttestation<'a, T>, AttestationError>>,
        AttestationError,
    >
    where
        I: Iterator<Item = (&'a SingleAttestation, Option<SubnetId>)> + ExactSizeIterator,
    {
        batch_verify_unaggregated_attestations(attestations, self)
    }

    /// Accepts some `Attestation` from the network and attempts to verify it, returning `Ok(_)` if
    /// it is valid to be (re)broadcast on the gossip network.
    ///
    /// The attestation must be "unaggregated", that is it must have exactly one
    /// aggregation bit set.
    pub fn verify_unaggregated_attestation_for_gossip<'a>(
        &self,
        unaggregated_attestation: &'a SingleAttestation,
        subnet_id: Option<SubnetId>,
    ) -> Result<VerifiedUnaggregatedAttestation<'a, T>, AttestationError> {
        metrics::inc_counter(&metrics::UNAGGREGATED_ATTESTATION_PROCESSING_REQUESTS);
        let _timer =
            metrics::start_timer(&metrics::UNAGGREGATED_ATTESTATION_GOSSIP_VERIFICATION_TIMES);

        VerifiedUnaggregatedAttestation::verify(unaggregated_attestation, subnet_id, self).inspect(
            |v| {
                // This method is called for API and gossip attestations, so this covers all unaggregated attestation events
                if let Some(event_handler) = self.event_handler.as_ref() {
                    if event_handler.has_single_attestation_subscribers() {
                        let current_fork = self
                            .spec
                            .fork_name_at_slot::<T::EthSpec>(v.attestation().data().slot);
                        if current_fork.electra_enabled() {
                            event_handler.register(EventKind::SingleAttestation(Box::new(
                                v.single_attestation(),
                            )));
                        }
                    }

                    if event_handler.has_attestation_subscribers() {
                        let current_fork = self
                            .spec
                            .fork_name_at_slot::<T::EthSpec>(v.attestation().data().slot);
                        if !current_fork.electra_enabled() {
                            event_handler.register(EventKind::Attestation(Box::new(
                                v.attestation().clone_as_attestation(),
                            )));
                        }
                    }
                }
                metrics::inc_counter(&metrics::UNAGGREGATED_ATTESTATION_PROCESSING_SUCCESSES);
            },
        )
    }

    /// Performs the same validation as `Self::verify_aggregated_attestation_for_gossip`, but for
    /// multiple attestations using batch BLS verification. Batch verification can provide
    /// significant CPU-time savings compared to individual verification.
    pub fn batch_verify_aggregated_attestations_for_gossip<'a, I>(
        &self,
        aggregates: I,
    ) -> Result<Vec<Result<VerifiedAggregatedAttestation<'a, T>, AttestationError>>, AttestationError>
    where
        I: Iterator<Item = &'a SignedAggregateAndProof<T::EthSpec>> + ExactSizeIterator,
    {
        batch_verify_aggregated_attestations(aggregates, self)
    }

    /// Accepts some `SignedAggregateAndProof` from the network and attempts to verify it,
    /// returning `Ok(_)` if it is valid to be (re)broadcast on the gossip network.
    pub fn verify_aggregated_attestation_for_gossip<'a>(
        &self,
        signed_aggregate: &'a SignedAggregateAndProof<T::EthSpec>,
    ) -> Result<VerifiedAggregatedAttestation<'a, T>, AttestationError> {
        metrics::inc_counter(&metrics::AGGREGATED_ATTESTATION_PROCESSING_REQUESTS);
        let _timer =
            metrics::start_timer(&metrics::AGGREGATED_ATTESTATION_GOSSIP_VERIFICATION_TIMES);

        VerifiedAggregatedAttestation::verify(signed_aggregate, self).inspect(|v| {
            // This method is called for API and gossip attestations, so this covers all aggregated attestation events
            if let Some(event_handler) = self.event_handler.as_ref()
                && event_handler.has_attestation_subscribers()
            {
                event_handler.register(EventKind::Attestation(Box::new(
                    v.attestation().clone_as_attestation(),
                )));
            }
            metrics::inc_counter(&metrics::AGGREGATED_ATTESTATION_PROCESSING_SUCCESSES);
        })
    }

    /// Accepts some `SyncCommitteeMessage` from the network and attempts to verify it, returning `Ok(_)` if
    /// it is valid to be (re)broadcast on the gossip network.
    pub fn verify_sync_committee_message_for_gossip(
        &self,
        sync_message: SyncCommitteeMessage,
        subnet_id: SyncSubnetId,
    ) -> Result<VerifiedSyncCommitteeMessage, SyncCommitteeError> {
        metrics::inc_counter(&metrics::SYNC_MESSAGE_PROCESSING_REQUESTS);
        let _timer = metrics::start_timer(&metrics::SYNC_MESSAGE_GOSSIP_VERIFICATION_TIMES);

        VerifiedSyncCommitteeMessage::verify(sync_message, subnet_id, self).inspect(|_| {
            metrics::inc_counter(&metrics::SYNC_MESSAGE_PROCESSING_SUCCESSES);
        })
    }

    /// Accepts some `SignedContributionAndProof` from the network and attempts to verify it,
    /// returning `Ok(_)` if it is valid to be (re)broadcast on the gossip network.
    pub fn verify_sync_contribution_for_gossip(
        &self,
        sync_contribution: SignedContributionAndProof<T::EthSpec>,
    ) -> Result<VerifiedSyncContribution<T>, SyncCommitteeError> {
        metrics::inc_counter(&metrics::SYNC_CONTRIBUTION_PROCESSING_REQUESTS);
        let _timer = metrics::start_timer(&metrics::SYNC_CONTRIBUTION_GOSSIP_VERIFICATION_TIMES);
        VerifiedSyncContribution::verify(sync_contribution, self).inspect(|v| {
            if let Some(event_handler) = self.event_handler.as_ref()
                && event_handler.has_contribution_subscribers()
            {
                event_handler.register(EventKind::ContributionAndProof(Box::new(
                    v.aggregate().clone(),
                )));
            }
            metrics::inc_counter(&metrics::SYNC_CONTRIBUTION_PROCESSING_SUCCESSES);
        })
    }

    /// Accepts some 'LightClientFinalityUpdate' from the network and attempts to verify it
    pub fn verify_finality_update_for_gossip(
        self: &Arc<Self>,
        light_client_finality_update: LightClientFinalityUpdate<T::EthSpec>,
        seen_timestamp: Duration,
    ) -> Result<VerifiedLightClientFinalityUpdate<T>, LightClientFinalityUpdateError> {
        VerifiedLightClientFinalityUpdate::verify(
            light_client_finality_update,
            self,
            seen_timestamp,
        )
        .inspect(|_| {
            metrics::inc_counter(&metrics::FINALITY_UPDATE_PROCESSING_SUCCESSES);
        })
    }

    #[instrument(skip_all, level = "trace")]
    pub fn verify_data_column_sidecar_for_gossip(
        self: &Arc<Self>,
        data_column_sidecar: Arc<DataColumnSidecar<T::EthSpec>>,
        subnet_id: DataColumnSubnetId,
    ) -> Result<GossipVerifiedDataColumn<T>, GossipDataColumnError> {
        metrics::inc_counter(&metrics::DATA_COLUMN_SIDECAR_PROCESSING_REQUESTS);
        let _timer = metrics::start_timer(&metrics::DATA_COLUMN_SIDECAR_GOSSIP_VERIFICATION_TIMES);
        GossipVerifiedDataColumn::new(data_column_sidecar, subnet_id, self).inspect(|_| {
            metrics::inc_counter(&metrics::DATA_COLUMN_SIDECAR_PROCESSING_SUCCESSES);
        })
    }

    #[instrument(skip_all, level = "trace")]
    pub fn verify_blob_sidecar_for_gossip(
        self: &Arc<Self>,
        blob_sidecar: Arc<BlobSidecar<T::EthSpec>>,
        subnet_id: u64,
    ) -> Result<GossipVerifiedBlob<T>, GossipBlobError> {
        metrics::inc_counter(&metrics::BLOBS_SIDECAR_PROCESSING_REQUESTS);
        let _timer = metrics::start_timer(&metrics::BLOBS_SIDECAR_GOSSIP_VERIFICATION_TIMES);
        GossipVerifiedBlob::new(blob_sidecar, subnet_id, self).inspect(|_| {
            metrics::inc_counter(&metrics::BLOBS_SIDECAR_PROCESSING_SUCCESSES);
        })
    }

    /// Accepts some 'LightClientOptimisticUpdate' from the network and attempts to verify it
    pub fn verify_optimistic_update_for_gossip(
        self: &Arc<Self>,
        light_client_optimistic_update: LightClientOptimisticUpdate<T::EthSpec>,
        seen_timestamp: Duration,
    ) -> Result<VerifiedLightClientOptimisticUpdate<T>, LightClientOptimisticUpdateError> {
        VerifiedLightClientOptimisticUpdate::verify(
            light_client_optimistic_update,
            self,
            seen_timestamp,
        )
        .inspect(|_| {
            metrics::inc_counter(&metrics::OPTIMISTIC_UPDATE_PROCESSING_SUCCESSES);
        })
    }

    /// Accepts some attestation-type object and attempts to verify it in the context of fork
    /// choice. If it is valid it is applied to `self.fork_choice`.
    ///
    /// Common items that implement `VerifiedAttestation`:
    ///
    /// - `VerifiedUnaggregatedAttestation`
    /// - `VerifiedAggregatedAttestation`
    pub fn apply_attestation_to_fork_choice(
        &self,
        verified: &impl VerifiedAttestation<T>,
    ) -> Result<(), Error> {
        self.canonical_head
            .fork_choice_write_lock()
            .on_attestation(
                self.slot()?,
                verified.indexed_attestation().to_ref(),
                AttestationFromBlock::False,
                &self.spec,
            )
            .map_err(Into::into)
    }

    /// Accepts a `VerifiedAttestation` and attempts to apply it to `self.op_pool`.
    ///
    /// The op pool is used by local block producers to pack blocks with operations.
    pub fn add_to_block_inclusion_pool<A>(
        &self,
        verified_attestation: A,
    ) -> Result<(), AttestationError>
    where
        A: VerifiedAttestation<T>,
    {
        let _timer = metrics::start_timer(&metrics::ATTESTATION_PROCESSING_APPLY_TO_OP_POOL);

        // If there's no eth1 chain then it's impossible to produce blocks and therefore
        // useless to put things in the op pool.
        let (attestation, attesting_indices) = verified_attestation.into_attestation_and_indices();
        self.op_pool
            .insert_attestation(attestation, attesting_indices)
            .map_err(Error::from)?;

        Ok(())
    }

    /// A convenience method for spawning a blocking task. It maps an `Option` and
    /// `tokio::JoinError` into a single `BeaconChainError`.
    pub(crate) async fn spawn_blocking_handle<F, R>(
        &self,
        task: F,
        name: &'static str,
    ) -> Result<R, Error>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let handle = self
            .task_executor
            .spawn_blocking_handle(task, name)
            .ok_or(Error::RuntimeShutdown)?;

        handle.await.map_err(Error::TokioJoin)
    }

    /// Accepts a `chain_segment` and filters out any uninteresting blocks (e.g., pre-finalization
    /// or already-known).
    ///
    /// This method is potentially long-running and should not run on the core executor.
    #[instrument(skip_all, level = "debug")]


    /// This function takes a configured weak subjectivity `Checkpoint` and the latest finalized `Checkpoint`.
    /// If the weak subjectivity checkpoint and finalized checkpoint share the same epoch, we compare
    /// roots. If we the weak subjectivity checkpoint is from an older epoch, we iterate back through
    /// roots in the canonical chain until we reach the finalized checkpoint from the correct epoch, and
    /// compare roots. This must called on startup and during verification of any block which causes a finality
    /// change affecting the weak subjectivity checkpoint.
    pub fn verify_weak_subjectivity_checkpoint(
        &self,
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
        // If epochs match, simply compare roots.
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

            // Iterate backwards through block roots from the given state. If first slot of the epoch is a skip-slot,
            // this will return the root of the closest prior non-skipped slot.
            match self.root_at_slot_from_state(slot, beacon_block_root, state)? {
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

    /// Called by the timer on every slot.
    ///
    /// Note: this function **MUST** be called from a non-async context since
    /// it contains a call to `fork_choice` which may eventually call
    /// `tokio::runtime::block_on` in certain cases.
    pub async fn per_slot_task(self: &Arc<Self>) {
        if let Some(slot) = self.slot_clock.now() {
            debug!(?slot, "Running beacon chain per slot tasks");

            // Always run the light-weight pruning tasks (these structures should be empty during
            // sync anyway).
            self.attestation_manager
                .naive_aggregation_pool
                .write()
                .prune(slot);
            self.block_times_cache.write().prune(slot);
            self.envelope_times_cache.write().prune(slot);
            self.gossip_verified_payload_bid_cache.prune(slot);
            self.gossip_verified_proposer_preferences_cache.prune(slot);

            // Don't run heavy-weight tasks during sync.
            if self.best_slot() + MAX_PER_SLOT_FORK_CHOICE_DISTANCE < slot {
                return;
            }

            // Run fork choice and signal to any waiting task that it has completed.
            self.recompute_head_at_current_slot().await;

            // Send the notification regardless of fork choice success, this is a "best effort"
            // notification and we don't want block production to hit the timeout in case of error.
            // Use a blocking task to avoid blocking the core executor whilst waiting for locks
            // in `ForkChoiceSignalTx`.
            let chain = self.clone();
            self.task_executor.clone().spawn_blocking(
                move || {
                    // Signal block proposal for the next slot (if it happens to be waiting).
                    if let Some(tx) = &chain.fork_choice_signal_tx
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

    /// This function provides safe and efficient multi-threaded access to the beacon proposer cache.
    ///
    /// The arguments are:
    ///
    /// - `shuffling_decision_block`: The block root of the decision block for the desired proposer
    ///   shuffling. This should be computed using one of the methods for computing proposer
    ///   shuffling decision roots, e.g. `BeaconState::proposer_shuffling_decision_root_at_epoch`.
    /// - `proposal_epoch`: The epoch at which the proposer shuffling is required.
    /// - `accessor`: A closure to run against the proposers for the selected epoch. Usually this
    ///   closure just grabs a single proposer, or takes the vec of proposers for the epoch.
    /// - `state_provider`: A closure to compute a state suitable for determining the shuffling.
    ///   This closure is evaluated lazily ONLY in the case that a cache miss occurs. It is
    ///   recommended for code that wants to keep track of cache misses to produce a log and/or
    ///   increment a metric inside this closure .
    ///
    /// Runs the `map_fn` with the committee cache for `shuffling_epoch` from the chain with head
    /// `head_block_root`. The `map_fn` will be supplied two values:
    ///
    /// - `&CommitteeCache`: the committee cache that serves the given parameters.
    /// - `Hash256`: the "shuffling decision root" which uniquely identifies the `CommitteeCache`.
    ///
    /// It's not necessary that `head_block_root` matches our current view of the chain, it can be
    /// any block that is:
    ///
    /// - Known to us.
    /// - The finalized block or a descendant of the finalized block.
    ///
    /// It would be quite common for attestation verification operations to use a `head_block_root`
    /// that differs from our view of the head.
    ///
    /// ## Important
    ///
    /// This function is **not** suitable for determining proposer duties (only attester duties).
    ///
    /// ## Notes
    ///
    /// This function exists in this odd "map" pattern because efficiently obtaining a committee
    /// can be complex. It might involve reading straight from the `beacon_chain.shuffling_cache`
    /// or it might involve reading it from a state from the DB. Due to the complexities of
    /// `RwLock`s on the shuffling cache, a simple `Cow` isn't suitable here.
    ///
    /// If the committee for `(head_block_root, shuffling_epoch)` isn't found in the
    /// `shuffling_cache`, we will read a state from disk and then update the `shuffling_cache`.
    pub fn with_committee_cache<F, R>(
        &self,
        head_block_root: Hash256,
        shuffling_epoch: Epoch,
        map_fn: F,
    ) -> Result<R, Error>
    where
        F: Fn(&CommitteeCache, Hash256) -> Result<R, Error>,
    {
        let head_block = self
            .canonical_head
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
            let _ =
                metrics::start_timer(&metrics::ATTESTATION_PROCESSING_SHUFFLING_CACHE_WAIT_TIMES);
            self.attestation_manager.shuffling_cache.write()
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
            //
            // This code is a little awkward because we need to ensure that the head we read and
            // the head we copy is identical. Taking one lock to read the head values and another
            // to copy the head is liable to race-conditions.
            let head_state_opt = self.with_head(|head| {
                if head.beacon_block_root == head_block_root {
                    Ok(Some((head.beacon_state.clone(), head.beacon_state_root())))
                } else {
                    Ok::<_, Error>(None)
                }
            })?;

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
                let (state_root, state) = self
                    .store
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
                partial_state_advance(&mut state, Some(state_root), target_slot, &self.spec)?;
            }
            metrics::stop_timer(state_skip_timer);

            let committee_building_timer =
                metrics::start_timer(&metrics::ATTESTATION_PROCESSING_COMMITTEE_BUILDING_TIMES);

            let relative_epoch = RelativeEpoch::from_epoch(state.current_epoch(), shuffling_epoch)
                .map_err(Error::IncorrectStateForAttestation)?;

            state.build_committee_cache(relative_epoch, &self.spec)?;

            let committee_cache = state.committee_cache(relative_epoch)?.clone();
            let shuffling_decision_block = shuffling_id.shuffling_decision_block;

            self.attestation_manager
                .shuffling_cache
                .write()
                .insert_committee_cache(shuffling_id, &committee_cache);

            metrics::stop_timer(committee_building_timer);

            sender.send(committee_cache.clone());

            map_fn(&committee_cache, shuffling_decision_block)
        }
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

    /// Gets the current `EnrForkId`.
    pub fn enr_fork_id(&self) -> EnrForkId {
        // If we are unable to read the slot clock we assume that it is prior to genesis and
        // therefore use the genesis slot.
        let slot = self.slot().unwrap_or(self.spec.genesis_slot);

        self.spec
            .enr_fork_id::<T::EthSpec>(slot, self.genesis_validators_root)
    }

    /// Returns the fork_digest corresponding to an epoch.
    /// See [`ChainSpec::compute_fork_digest`]
    pub fn compute_fork_digest(&self, epoch: Epoch) -> [u8; 4] {
        self.spec
            .compute_fork_digest(self.genesis_validators_root, epoch)
    }

    /// Calculates the `Duration` to the next fork digest (this could be either a regular or BPO
    /// hard fork) if it exists and returns it with its corresponding `Epoch`.
    pub fn duration_to_next_digest(&self) -> Option<(Epoch, Duration)> {
        // If we are unable to read the slot clock we assume that it is prior to genesis and
        // therefore use the genesis slot.
        let slot = self.slot().unwrap_or(self.spec.genesis_slot);
        let epoch = slot.epoch(T::EthSpec::slots_per_epoch());

        let next_digest_epoch = self.spec.next_digest_epoch(epoch)?;
        let next_digest_slot = next_digest_epoch.start_slot(T::EthSpec::slots_per_epoch());

        self.slot_clock
            .duration_to_slot(next_digest_slot)
            .map(|duration| (next_digest_epoch, duration))
    }

    /// This method serves to get a sense of the current chain health. It is used in block proposal
    /// to determine whether we should outsource payload production duties.
    ///
    /// Since we are likely calling this during the slot we are going to propose in, don't take into
    /// account the current slot when accounting for skips.
    pub fn is_healthy(&self, parent_root: &Hash256) -> Result<ChainHealth, Error> {
        let cached_head = self.canonical_head.cached_head();
        if let Some(head_hash) = cached_head.forkchoice_update_parameters().head_hash {
            if ExecutionBlockHash::zero() == head_hash {
                return Ok(ChainHealth::PreMerge);
            }
        } else {
            return Ok(ChainHealth::PreMerge);
        };

        // Check that the parent is NOT optimistic.
        if let Some(execution_status) = self
            .canonical_head
            .fork_choice_read_lock()
            .get_block_execution_status(parent_root)
            && execution_status.is_strictly_optimistic()
        {
            return Ok(ChainHealth::Optimistic);
        }

        if self.config.builder_fallback_disable_checks {
            return Ok(ChainHealth::Healthy);
        }

        let current_slot = self.slot()?;

        // Check slots at the head of the chain.
        let prev_slot = current_slot.saturating_sub(Slot::new(1));
        let head_skips = prev_slot.saturating_sub(cached_head.head_slot());
        let head_skips_check = head_skips.as_usize() <= self.config.builder_fallback_skips;

        // Check if finalization is advancing.
        let current_epoch = current_slot.epoch(T::EthSpec::slots_per_epoch());
        let epochs_since_finalization =
            current_epoch.saturating_sub(cached_head.finalized_checkpoint().epoch);
        let finalization_check = epochs_since_finalization.as_usize()
            <= self.config.builder_fallback_epochs_since_finalization;

        // Check skip slots in the last `SLOTS_PER_EPOCH`.
        let start_slot = current_slot.saturating_sub(T::EthSpec::slots_per_epoch());
        let mut epoch_skips = 0;
        for slot in start_slot.as_u64()..current_slot.as_u64() {
            if self
                .block_root_at_slot_skips_none(Slot::new(slot))?
                .is_none()
            {
                epoch_skips += 1;
            }
        }
        let epoch_skips_check = epoch_skips <= self.config.builder_fallback_skips_per_epoch;

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

    pub fn dump_as_dot<W: Write>(&self, output: &mut W) {
        let canonical_head_hash = self.canonical_head.cached_head().head_block_root();
        let mut visited: HashSet<Hash256> = HashSet::new();
        let mut finalized_blocks: HashSet<Hash256> = HashSet::new();
        let mut justified_blocks: HashSet<Hash256> = HashSet::new();

        let genesis_block_hash = Hash256::zero();
        writeln!(output, "digraph beacon {{").unwrap();
        writeln!(output, "\t_{:?}[label=\"zero\"];", genesis_block_hash).unwrap();

        // Canonical head needs to be processed first as otherwise finalized blocks aren't detected
        // properly.
        let heads = {
            let mut heads = self.heads();
            let canonical_head_index = heads
                .iter()
                .position(|(block_hash, _)| *block_hash == canonical_head_hash)
                .unwrap();
            let (canonical_head_hash, canonical_head_slot) =
                heads.swap_remove(canonical_head_index);
            heads.insert(0, (canonical_head_hash, canonical_head_slot));
            heads
        };

        for (head_hash, _head_slot) in heads {
            for maybe_pair in ParentRootBlockIterator::new(&*self.store, head_hash) {
                let (block_hash, signed_beacon_block) = maybe_pair.unwrap();
                if visited.contains(&block_hash) {
                    break;
                }
                visited.insert(block_hash);

                if signed_beacon_block.slot() % T::EthSpec::slots_per_epoch() == 0 {
                    let block = self.get_blinded_block(&block_hash).unwrap().unwrap();
                    // This branch is reached from the HTTP API. We assume the user wants
                    // to cache states so that future calls are faster.
                    let state = self
                        .get_state(&block.state_root(), Some(block.slot()), true)
                        .unwrap()
                        .unwrap();
                    finalized_blocks.insert(state.finalized_checkpoint().root);
                    justified_blocks.insert(state.current_justified_checkpoint().root);
                    justified_blocks.insert(state.previous_justified_checkpoint().root);
                }

                if block_hash == canonical_head_hash {
                    writeln!(
                        output,
                        "\t_{:?}[label=\"{} ({})\" shape=box3d];",
                        block_hash,
                        block_hash,
                        signed_beacon_block.slot()
                    )
                    .unwrap();
                } else if finalized_blocks.contains(&block_hash) {
                    writeln!(
                        output,
                        "\t_{:?}[label=\"{} ({})\" shape=Msquare];",
                        block_hash,
                        block_hash,
                        signed_beacon_block.slot()
                    )
                    .unwrap();
                } else if justified_blocks.contains(&block_hash) {
                    writeln!(
                        output,
                        "\t_{:?}[label=\"{} ({})\" shape=cds];",
                        block_hash,
                        block_hash,
                        signed_beacon_block.slot()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        output,
                        "\t_{:?}[label=\"{} ({})\" shape=box];",
                        block_hash,
                        block_hash,
                        signed_beacon_block.slot()
                    )
                    .unwrap();
                }
                writeln!(
                    output,
                    "\t_{:?} -> _{:?};",
                    block_hash,
                    signed_beacon_block.parent_root()
                )
                .unwrap();
            }
        }

        writeln!(output, "}}").unwrap();
    }

    /// Get a channel to request shutting down.
    pub fn shutdown_sender(&self) -> Sender<ShutdownReason> {
        self.shutdown_sender.clone()
    }

    // Used for debugging
    #[allow(dead_code)]
    pub fn dump_dot_file(&self, file_name: &str) {
        let mut file = std::fs::File::create(file_name).unwrap();
        self.dump_as_dot(&mut file);
    }

    /// Checks if attestations have been seen from the given `validator_index` at the
    /// given `epoch`.
    pub fn validator_seen_at_epoch(&self, validator_index: usize, epoch: Epoch) -> bool {
        // It's necessary to assign these checks to intermediate variables to avoid a deadlock.
        //
        // See: https://github.com/sigp/lighthouse/pull/2230#discussion_r620013993
        let gossip_attested = self
            .attestation_manager
            .observed_gossip_attesters
            .read()
            .index_seen_at_epoch(validator_index, epoch);
        let block_attested = self
            .attestation_manager
            .observed_block_attesters
            .read()
            .index_seen_at_epoch(validator_index, epoch);
        let aggregated = self
            .attestation_manager
            .observed_aggregators
            .read()
            .index_seen_at_epoch(validator_index, epoch);
        let produced_block = self
            .observed_block_producers
            .read()
            .index_seen_at_epoch(validator_index as u64, epoch);

        gossip_attested || block_attested || aggregated || produced_block
    }

    /// Gets the `LightClientBootstrap` object for a requested block root.
    ///
    /// Returns `None` when the state or block is not found in the database.
    #[allow(clippy::type_complexity)]
    pub fn get_light_client_bootstrap(
        &self,
        block_root: &Hash256,
    ) -> Result<Option<(LightClientBootstrap<T::EthSpec>, ForkName)>, Error> {
        let head_state = &self.head().snapshot.beacon_state;
        let finalized_period = head_state
            .finalized_checkpoint()
            .epoch
            .sync_committee_period(&self.spec)?;
        self.light_client_server_cache.get_light_client_bootstrap(
            &self.store,
            block_root,
            finalized_period,
            &self.spec,
        )
    }

    pub(crate) fn get_blobs_or_columns_store_op(
        &self,
        block_root: Hash256,
        block_slot: Slot,
        block_data: AvailableBlockData<T::EthSpec>,
    ) -> Option<StoreOp<'_, T::EthSpec>> {
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
                let columns_to_custody =
                    self.data_availability_manager
                        .custody_columns_for_epoch(Some(
                            block_slot.epoch(T::EthSpec::slots_per_epoch()),
                        ));
                // Supernodes need to persist all sampled custody columns
                if columns_to_custody.len() != self.spec.number_of_custody_groups as usize {
                    data_columns
                        .retain(|data_column| columns_to_custody.contains(data_column.index()));
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

    /// Retrieves block roots (in ascending slot order) within some slot range from fork choice.
    pub fn block_roots_from_fork_choice(
        &self,
        start_slot: u64,
        count: u64,
    ) -> Vec<(Hash256, Slot)> {
        let head_block_root = self.canonical_head.cached_head().head_block_root();
        let fork_choice_read_lock = self.canonical_head.fork_choice_read_lock();
        let block_roots_iter = fork_choice_read_lock
            .proto_array()
            .iter_block_roots(&head_block_root);
        let end_slot = start_slot.saturating_add(count);
        let mut roots = vec![];

        for (root, slot) in block_roots_iter {
            if slot < end_slot && slot >= start_slot {
                roots.push((root, slot));
            }
            if slot < start_slot {
                break;
            }
        }

        drop(fork_choice_read_lock);
        // return in ascending slot order
        roots.reverse();
        roots
    }
}

impl<T: BeaconChainTypes> Drop for BeaconChain<T> {
    fn drop(&mut self) {
        let drop = || -> Result<(), Error> {
            self.persist_fork_choice()?;
            self.persist_op_pool()?;
            self.persist_custody_context()
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
