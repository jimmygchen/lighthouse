use async_trait::async_trait;
use eth2::BeaconNodeHttpClient;
use types::light_client_bootstrap::LightClientBootstrap;
use types::light_client_update::LightClientUpdate;
use types::{
    EthSpec, ForkVersionedResponse, Hash256, LightClientFinalityUpdate, LightClientOptimisticUpdate,
};

pub const MAX_REQUEST_LIGHT_CLIENT_UPDATES: u64 = 128;

#[async_trait]
pub trait LightClientDataProvider<E: EthSpec>: Send + Sync {
    async fn get_light_client_bootstrap(
        &self,
        checkpoint_root: Hash256,
    ) -> Result<ForkVersionedResponse<LightClientBootstrap<E>>, DataProviderError>;

    async fn get_light_client_updates(
        &self,
        start_period: u64,
        count: u64,
    ) -> Result<Vec<ForkVersionedResponse<LightClientUpdate<E>>>, DataProviderError>;

    async fn get_light_client_finality_update(
        &self,
    ) -> Result<ForkVersionedResponse<LightClientFinalityUpdate<E>>, DataProviderError>;

    async fn get_light_client_optimistic_update(
        &self,
    ) -> Result<ForkVersionedResponse<LightClientOptimisticUpdate<E>>, DataProviderError>;
}

pub struct LightClientDataRestProvider {
    beacon_node: BeaconNodeHttpClient,
}

#[derive(Debug)]
pub enum DataProviderError {
    BeaconApiError(eth2::Error),
}

impl LightClientDataRestProvider {
    pub(crate) fn new(beacon_node: BeaconNodeHttpClient) -> Self {
        LightClientDataRestProvider { beacon_node }
    }
}

#[async_trait]
impl<E: EthSpec> LightClientDataProvider<E> for LightClientDataRestProvider {
    async fn get_light_client_bootstrap(
        &self,
        checkpoint_root: Hash256,
    ) -> Result<ForkVersionedResponse<LightClientBootstrap<E>>, DataProviderError> {
        self.beacon_node
            .get_light_client_bootstrap(checkpoint_root)
            .await
            .map_err(DataProviderError::BeaconApiError)
    }

    async fn get_light_client_updates(
        &self,
        start_period: u64,
        count: u64,
    ) -> Result<Vec<ForkVersionedResponse<LightClientUpdate<E>>>, DataProviderError> {
        self.beacon_node
            .get_light_client_updates(start_period, count)
            .await
            .map_err(DataProviderError::BeaconApiError)
    }

    async fn get_light_client_finality_update(
        &self,
    ) -> Result<ForkVersionedResponse<LightClientFinalityUpdate<E>>, DataProviderError> {
        self.beacon_node
            .get_light_client_finality_update()
            .await
            .map_err(DataProviderError::BeaconApiError)
    }

    async fn get_light_client_optimistic_update(
        &self,
    ) -> Result<ForkVersionedResponse<LightClientOptimisticUpdate<E>>, DataProviderError> {
        self.beacon_node
            .get_light_client_optimistic_update()
            .await
            .map_err(DataProviderError::BeaconApiError)
    }
}
