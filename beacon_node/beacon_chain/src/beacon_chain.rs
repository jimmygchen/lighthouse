use crate::BeaconForkChoiceStore;
use crate::attestation_manager::AttestationManager;
use crate::block_importer::BlockImporter;
use crate::block_production::BlockProducer;
use crate::block_verification_types::RangeSyncBlock;
pub use crate::canonical_head::CanonicalHead;
use crate::chain_config::ChainConfig;
use crate::data_availability_manager::DataAvailabilityManager;
use crate::errors::BeaconChainError as Error;
use crate::execution_manager::ExecutionManager;
use crate::migrate::BackgroundMigrator;
use crate::operations_manager::OperationsManager;
use crate::sync_committee_manager::SyncCommitteeManager;
use crate::validator_query_service::ValidatorQueryService;
use fork_choice::ForkChoice;
use kzg::Kzg;
use slot_clock::SlotClock;
use std::sync::Arc;
use std::time::Duration;
use store::{Error as DBError, HotColdDB};
use task_executor::TaskExecutor;
use types::*;

pub type ForkChoiceError = fork_choice::Error<crate::ForkChoiceStoreError>;

/// Alias to appease clippy.
pub(crate) type HashBlockTuple<E> = (Hash256, RangeSyncBlock<E>);

// These keys are all zero because they get stored in different columns, see `DBColumn` type.
pub const BEACON_CHAIN_DB_KEY: Hash256 = Hash256::ZERO;
pub const OP_POOL_DB_KEY: Hash256 = Hash256::ZERO;
pub const FORK_CHOICE_DB_KEY: Hash256 = Hash256::ZERO;

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
/// The top-level container for all beacon-chain subsystems.
///
/// Holds shared state (store, slot clock, spec, etc.) and the various manager
/// components that implement beacon-chain logic. Previously named `BeaconChain`;
/// the alias above keeps external crates compiling while the rename propagates.
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
    /// Manages attestation pools, observation tracking, and shuffling caches.
    pub attestation_manager: Arc<AttestationManager<T::EthSpec>>,
    /// Manages voluntary exits, proposer/attester slashings, and BLS-to-execution changes.
    pub operations: Arc<OperationsManager<T::EthSpec>>,
    /// Manages sync committee message and contribution verification, and the
    /// sync aggregation pool.
    pub sync_committee_manager: Arc<SyncCommitteeManager<T::EthSpec>>,
    /// Stores information about the canonical head and finalized/justified checkpoints of the
    /// chain. Also contains the fork choice struct, for computing the canonical head.
    pub canonical_head: Arc<CanonicalHead<T>>,
    /// The root of the genesis block.
    pub genesis_block_root: Hash256,
    /// The root of the genesis state.
    pub genesis_state_root: Hash256,
    /// The root of the list of genesis validators, used during syncing.
    pub genesis_validators_root: Hash256,
    /// The genesis time of this `BeaconChain` (seconds since UNIX epoch).
    pub genesis_time: u64,
    /// Handles validator public key and index lookups.
    pub validator_query: ValidatorQueryService<T>,
    /// The slot at which blocks are downloaded back to.
    pub genesis_backfill_slot: Slot,
    /// The KZG trusted setup used by this chain.
    ///
    /// Kept as a top-level field because KZG is a process-wide singleton used by
    /// many subsystems beyond data availability (block verification, blob
    /// verification, data-column verification, fetch-blobs, historical column
    /// backfill). No single owning component exists; if needed elsewhere, prefer
    /// cloning this `Arc`.
    pub kzg: Arc<Kzg>,
    /// Component managing data availability: DA boundary calculations, custody info,
    /// and blob/column retrieval.
    pub data_availability_manager: Arc<DataAvailabilityManager<T>>,
    /// Component managing execution layer integration, proposer cache, and
    /// fork choice signalling.
    pub execution_manager: Arc<ExecutionManager<T>>,
    /// Component handling block, blob, and data-column import.
    pub block_importer: Arc<BlockImporter<T>>,
    /// Component handling block production.
    pub block_producer: Arc<BlockProducer<T>>,
}

impl FinalizationAndCanonicity {
    pub fn is_finalized(self) -> bool {
        self.slot_is_finalized && self.canonical
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
