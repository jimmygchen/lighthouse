use async_trait::async_trait;
use eth2::BeaconNodeHttpClient;
use types::light_client_bootstrap::LightClientBootstrap;
use types::EthSpec;

#[async_trait]
pub trait LightClientDataProvider<E: EthSpec> {
    async fn get_light_client_bootstrap(&self) -> Result<LightClientBootstrap<E>, Error>;
}

pub struct LightClientDataRestProvider {
    beacon_node: BeaconNodeHttpClient,
}

#[derive(Debug)]
pub enum Error {}

impl LightClientDataRestProvider {
    pub(crate) fn new(beacon_node: BeaconNodeHttpClient) -> Self {
        LightClientDataRestProvider { beacon_node }
    }
}

#[async_trait]
impl<E: EthSpec> LightClientDataProvider<E> for LightClientDataRestProvider {
    async fn get_light_client_bootstrap(&self) -> Result<LightClientBootstrap<E>, Error> {
        todo!()
    }
}
