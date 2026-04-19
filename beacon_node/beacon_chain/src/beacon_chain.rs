use crate::BeaconForkChoiceStore;
use crate::attestation_manager::AttestationManager;
use crate::block_importer::BlockImporter;
use crate::block_production::BlockProducer;
pub use crate::canonical_head::CanonicalHead;
use crate::canonical_head::ForkChoiceError;
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
use std::sync::Arc;
use store::{Error as DBError, HotColdDB};
use task_executor::TaskExecutor;
use types::*;

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
