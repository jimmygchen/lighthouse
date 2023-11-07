use crate::config::LightClientConfig;
use crate::data_provider::{LightClientDataProvider, LightClientDataRestProvider};
use crate::light_client_sync_service::LightClientSyncService;
use crate::store::{initialize_light_client_store, LightClientStore};
use environment::RuntimeContext;
use eth2::{BeaconNodeHttpClient, Timeouts};
use execution_layer::ExecutionLayer;
use parking_lot::RwLock;
use slog::info;
use slot_clock::{SlotClock, SystemTimeSlotClock};
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;
use types::{EthSpec, Hash256};

const DEFAULT_BEACON_API_TIMEOUT: Duration = Duration::from_secs(2);

pub trait LightClientTypes: Send + Sync + 'static {
    type SlotClock: SlotClock;
    type EthSpec: EthSpec;
    type DataProvider: LightClientDataProvider<Self::EthSpec>;
}

/// An empty struct used to "witness" all the `LightClientTypes` traits. It has no user-facing
/// functionality and only exists to satisfy the type system.
pub struct Witness<TSlotClock, TEthSpec, TDataProvider>(
    PhantomData<(TSlotClock, TEthSpec, TDataProvider)>,
);

impl<TSlotClock, TEthSpec, TDataProvider> LightClientTypes
    for Witness<TSlotClock, TEthSpec, TDataProvider>
where
    TSlotClock: SlotClock + 'static,
    TDataProvider: LightClientDataProvider<TEthSpec> + 'static,
    TEthSpec: EthSpec + 'static,
{
    type SlotClock = TSlotClock;
    type EthSpec = TEthSpec;
    type DataProvider = TDataProvider;
}

pub struct LightClient<T: LightClientTypes> {
    context: RuntimeContext<T::EthSpec>,
    slot_clock: Arc<T::SlotClock>,
    /// In memory storage for the light client state.
    store: Arc<RwLock<LightClientStore<T::EthSpec>>>,
    /// Provider to fetch light client data from.
    data_provider: Arc<T::DataProvider>,
    /// Interfaces with the execution client.
    execution_layer: ExecutionLayer<T::EthSpec>,
    genesis_validators_root: Hash256,
}

impl<T: LightClientTypes> LightClient<T> {
    pub async fn new(
        context: RuntimeContext<T::EthSpec>,
        config: LightClientConfig,
        slot_clock: T::SlotClock,
        data_provider: T::DataProvider,
        genesis_validators_root: Hash256,
    ) -> Result<Self, String> {
        let bootstrap = data_provider
            .get_light_client_bootstrap(config.checkpoint_root)
            .await
            .map_err(|e| format!("Error fetching LightClientBootstrap: {e:?}"))?;

        let store = initialize_light_client_store(config.checkpoint_root, bootstrap)
            .map_err(|e| format!("Error initializing LightClientStore: {e:?}"))?;

        let execution_layer = {
            let context = context.service_context("exec".into());
            ExecutionLayer::from_config(
                config.execution_layer,
                context.executor.clone(),
                context.log().clone(),
            )
            .map_err(|e| format!("unable to start execution layer endpoints: {:?}", e))?
        };

        Ok(Self {
            context,
            slot_clock: Arc::new(slot_clock),
            store: Arc::new(RwLock::new(store)),
            data_provider: Arc::new(data_provider),
            execution_layer,
            genesis_validators_root,
        })
    }

    pub fn start_service(&mut self) -> Result<(), String> {
        let service = LightClientSyncService::<T>::new(
            self.store.clone(),
            self.data_provider.clone(),
            self.slot_clock.clone(),
            self.genesis_validators_root,
            self.context.log().clone(),
            self.context.eth2_config.spec.clone(),
        );

        let executor = self.context.executor.clone();
        executor.spawn(
            async move { service.start().await },
            "light_client_sync_service",
        );

        Ok(())
    }
}

/// A type-alias to the tighten the definition of a production-intended `LightClient`.
pub type ProductionClient<E> =
    LightClient<Witness<SystemTimeSlotClock, E, LightClientDataRestProvider>>;

pub struct ProductionLightClient<E: EthSpec>(ProductionClient<E>);

impl<E: EthSpec> ProductionLightClient<E> {
    pub async fn new(
        context: RuntimeContext<E>,
        config: LightClientConfig,
    ) -> Result<Self, String> {
        let log = context.log().clone();

        info!(
            log,
            "Starting light client";
            "beacon_node" => format!("{:?}", &config.beacon_node),
        );

        let genesis_state = context
            .eth2_network_config
            .as_ref()
            .ok_or("Context is missing eth2 network config")?
            .genesis_state::<E>(
                config.genesis_state_url.as_deref(),
                config.genesis_state_url_timeout,
                &log,
            )
            .await?
            .ok_or_else(|| "The genesis state for this network is not known".to_string())?;

        let genesis_time = genesis_state.genesis_time();
        let genesis_validators_root = genesis_state.genesis_validators_root();

        info!(log, "Genesis state found"; "root" => genesis_state.canonical_root().to_string());

        let slot_clock = SystemTimeSlotClock::new(
            context.eth2_config.spec.genesis_slot,
            Duration::from_secs(genesis_time),
            Duration::from_secs(context.eth2_config.spec.seconds_per_slot),
        );

        let data_provider = if let Some(beacon_node_url) = config.beacon_node.clone() {
            let beacon_node = BeaconNodeHttpClient::new(
                beacon_node_url,
                Timeouts::set_all(DEFAULT_BEACON_API_TIMEOUT),
            );
            LightClientDataRestProvider::new(beacon_node)
        } else {
            return Err("Beacon node URL is missing".to_string());
        };

        let light_client = LightClient::new(
            context,
            config,
            slot_clock,
            data_provider,
            genesis_validators_root,
        )
        .await?;

        Ok(Self(light_client))
    }

    pub fn start_service(&mut self) -> Result<(), String> {
        self.0.start_service()
    }
}
