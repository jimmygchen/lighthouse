use crate::config::LightClientConfig;
use crate::data_provider::{LightClientDataProvider, LightClientDataRestProvider};
use crate::store::{initialize_light_client_store, LightClientStore};
use environment::RuntimeContext;
use eth2::{BeaconNodeHttpClient, Timeouts};
use execution_layer::ExecutionLayer;
use slog::info;
use slot_clock::{SlotClock, SystemTimeSlotClock};
use std::marker::PhantomData;
use std::time::Duration;
use types::EthSpec;

const DEFAULT_BEACON_API_TIMEOUT: Duration = Duration::from_secs(2);

pub trait LightClientTypes {
    type SlotClock: SlotClock;
    type EthSpec: EthSpec;
    type DataProvider: LightClientDataProvider<Self::EthSpec>;
}

/// An empty struct used to "witness" all the `LightClientTypes` traits. It has no user-facing
/// functionality and only exists to satisfy the type system.
pub struct Witness<TSlotClock, TEthSpec, TDataProvider>(
    PhantomData<(TSlotClock, TEthSpec, TDataProvider)>,
);

impl<S: SlotClock, E: EthSpec, D: LightClientDataProvider<E>> LightClientTypes
    for Witness<S, E, D>
{
    type SlotClock = S;
    type EthSpec = E;
    type DataProvider = D;
}

pub struct LightClient<T: LightClientTypes> {
    context: RuntimeContext<T::EthSpec>,
    slot_clock: T::SlotClock,
    /// In memory storage for the light client state.
    store: LightClientStore<T::EthSpec>,
    /// Provider to fetch light client data from.
    data_provider: T::DataProvider,
    /// Interfaces with the execution client.
    execution_layer: ExecutionLayer<T::EthSpec>,
}

impl<T: LightClientTypes> LightClient<T> {
    pub async fn new(
        context: RuntimeContext<T::EthSpec>,
        config: LightClientConfig,
        slot_clock: T::SlotClock,
        data_provider: T::DataProvider,
    ) -> Result<Self, String> {
        let bootstrap = data_provider
            .get_light_client_bootstrap()
            .await
            .map_err(|e| format!("Error fetching LightClientBootstrap: {e:?}"))?;

        let store = initialize_light_client_store(config.checkpoint_root, bootstrap);

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
            slot_clock,
            store,
            data_provider,
            execution_layer,
        })
    }

    pub async fn start_service(&mut self) -> Result<(), String> {
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
        let _genesis_validators_root = genesis_state.genesis_validators_root();

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

        let light_client = LightClient::new(context, config, slot_clock, data_provider).await?;

        Ok(Self(light_client))
    }

    pub async fn start_service(&mut self) -> Result<(), String> {
        self.0.start_service().await
    }
}
