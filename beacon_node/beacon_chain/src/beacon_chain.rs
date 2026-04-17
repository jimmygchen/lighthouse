use crate::attestation_manager::AttestationManager;
use crate::attestation_verification::{
    Error as AttestationError, VerifiedAggregatedAttestation, VerifiedAttestation,
    VerifiedUnaggregatedAttestation, batch_verify_aggregated_attestations,
    batch_verify_unaggregated_attestations,
};
use crate::beacon_block_streamer::{BeaconBlockStreamer, CheckCaches};
use crate::beacon_proposer_cache::BeaconProposerCache;
use crate::blob_verification::{GossipBlobError, GossipVerifiedBlob};
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
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::prelude::*;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;
use store::iter::ParentRootBlockIterator;
use store::{
    BlobSidecarListFromRoot, DBColumn, DatabaseBlock, Error as DBError, HotColdDB, HotStateSummary,
    KeyValueStore, KeyValueStoreOp, StoreItem, StoreOp,
};
use task_executor::{RayonPoolType, ShutdownReason, TaskExecutor};
use tokio_stream::Stream;
use tracing::{debug, error, info, info_span, instrument, trace, warn};
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

    // -----------------------------------------------------------------------
    // State query methods: delegated to `state_query` free functions.
    // See `state_query.rs` for implementations and `impl BeaconChain<T>`
    // thin delegations.
    // -----------------------------------------------------------------------

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
        } else if let Some(block) = self.store.get_blinded_block(&block_root)? {
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

    /// Return the status of a block as it progresses through the various caches.
    pub fn get_block_process_status(&self, block_root: &Hash256) -> BlockProcessStatus<T::EthSpec> {
        self.data_availability_checker
            .get_cached_block(block_root)
            .unwrap_or(BlockProcessStatus::Unknown)
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

    /// Returns the state at the given root, if any.
    pub fn get_state(
        &self,
        state_root: &Hash256,
        slot: Option<Slot>,
        update_cache: bool,
    ) -> Result<Option<BeaconState<T::EthSpec>>, Error> {
        Ok(self.store.get_state(state_root, slot, update_cache)?)
    }

    /// Return the sync committee for `slot + 1` from the canonical chain.
    ///
    /// Delegates to `SyncCommitteeManager::sync_committee_at_next_slot`, providing
    /// the head state and a state-loader closure.
    pub fn sync_committee_at_next_slot(
        &self,
        slot: Slot,
    ) -> Result<Arc<SyncCommittee<T::EthSpec>>, Error> {
        let head_state = &self.head_snapshot().beacon_state;
        self.sync_committee_manager
            .sync_committee_at_next_slot(slot, head_state, |load_slot| {
                self.state_at_slot(load_slot, StateSkipConfig::WithoutStateRoots)
            })
    }

    /// Return the sync committee at `epoch` from the canonical chain.
    ///
    /// Delegates to `SyncCommitteeManager::sync_committee_at_epoch`, providing
    /// the head state and a state-loader closure.
    pub fn sync_committee_at_epoch(
        &self,
        epoch: Epoch,
    ) -> Result<Arc<SyncCommittee<T::EthSpec>>, Error> {
        let head_state = &self.head_snapshot().beacon_state;
        self.sync_committee_manager
            .sync_committee_at_epoch(epoch, head_state, |load_slot| {
                self.state_at_slot(load_slot, StateSkipConfig::WithoutStateRoots)
            })
    }

    /// Load a state suitable for determining the sync committee for the given period.
    ///
    /// **WARNING**: the state returned will have dummy state roots. It should only be used
    /// for its sync committees (determining duties, etc).
    pub fn state_for_sync_committee_period(
        &self,
        sync_committee_period: u64,
    ) -> Result<BeaconState<T::EthSpec>, Error> {
        let load_slot = self
            .sync_committee_manager
            .slot_for_sync_committee_period(sync_committee_period)?;
        self.state_at_slot(load_slot, StateSkipConfig::WithoutStateRoots)
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

    /// Return the attestation duties for the given `validator_indices` at `epoch`.
    ///
    /// Delegates to `AttestationManager::validator_attestation_duties`.
    pub fn validator_attestation_duties(
        &self,
        validator_indices: &[u64],
        epoch: Epoch,
        head_block_root: Hash256,
    ) -> Result<(Vec<Option<AttestationDuty>>, Hash256, ExecutionStatus), Error> {
        self.attestation_manager.validator_attestation_duties(
            validator_indices,
            epoch,
            head_block_root,
            &self.canonical_head,
            &self.store,
            &self.spec,
        )
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

    /// Produce an unaggregated `Attestation` that is valid for the given `slot` and `index`.
    ///
    /// Delegates to `AttestationManager::produce_unaggregated_attestation`.
    #[instrument(name = "lh_produce_unaggregated_attestation", skip_all, fields(%request_slot, %request_index), level = "debug")]
    pub fn produce_unaggregated_attestation(
        &self,
        request_slot: Slot,
        request_index: CommitteeIndex,
    ) -> Result<Attestation<T::EthSpec>, Error> {
        self.attestation_manager.produce_unaggregated_attestation(
            request_slot,
            request_index,
            &self.canonical_head,
            &self.store,
            &self.spec,
        )
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
        let ctx = crate::attestation_verification::AttestationVerificationContext::from_chain(self);
        batch_verify_unaggregated_attestations(attestations, &ctx)
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

        let ctx = crate::attestation_verification::AttestationVerificationContext::from_chain(self);
        VerifiedUnaggregatedAttestation::verify(unaggregated_attestation, subnet_id, &ctx).inspect(
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
        let ctx = crate::attestation_verification::AttestationVerificationContext::from_chain(self);
        batch_verify_aggregated_attestations(aggregates, &ctx)
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

        let ctx = crate::attestation_verification::AttestationVerificationContext::from_chain(self);
        VerifiedAggregatedAttestation::verify(signed_aggregate, &ctx).inspect(|v| {
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
                self.slot_clock.now().ok_or(Error::UnableToReadSlot)?,
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

    /// Provides access to the committee cache via the attestation manager's shuffling cache
    /// and the store for state loading on cache miss.
    ///
    /// Delegates to the `with_committee_cache` free function in `attestation_manager`.
    pub fn with_committee_cache<F, R>(
        &self,
        head_block_root: Hash256,
        shuffling_epoch: Epoch,
        map_fn: F,
    ) -> Result<R, Error>
    where
        F: Fn(&CommitteeCache, Hash256) -> Result<R, Error>,
    {
        crate::attestation_manager::with_committee_cache(
            head_block_root,
            shuffling_epoch,
            &self.canonical_head,
            &self.attestation_manager,
            &self.store,
            &self.spec,
            map_fn,
        )
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

        let current_slot = self.slot_clock.now().ok_or(Error::UnableToReadSlot)?;

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
                .block_root_at_slot(Slot::new(slot), WhenSlotSkipped::None)?
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
                    let block = self.store.get_blinded_block(&block_hash).unwrap().unwrap();
                    // This branch is reached from the HTTP API. We assume the user wants
                    // to cache states so that future calls are faster.
                    let state = self
                        .store
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

    // Used for debugging
    #[allow(dead_code)]
    pub fn dump_dot_file(&self, file_name: &str) {
        let mut file = std::fs::File::create(file_name).unwrap();
        self.dump_as_dot(&mut file);
    }

    /// Checks if attestations have been seen from the given `validator_index` at the
    /// given `epoch`.
    pub fn validator_seen_at_epoch(&self, validator_index: usize, epoch: Epoch) -> bool {
        let attested_or_aggregated = self
            .attestation_manager
            .validator_seen_at_epoch(validator_index, epoch);
        let produced_block = self
            .observed_block_producers
            .read()
            .index_seen_at_epoch(validator_index as u64, epoch);
        attested_or_aggregated || produced_block
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
