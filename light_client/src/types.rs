use types::BeaconBlockHeader;

pub struct LightClientHeader {
    beacon: BeaconBlockHeader,
}

impl From<BeaconBlockHeader> for LightClientHeader {
    fn from(beacon: BeaconBlockHeader) -> Self {
        LightClientHeader { beacon }
    }
}

impl LightClientHeader {
    /// https://github.com/ethereum/consensus-specs/blob/dev/specs/altair/light-client/sync-protocol.md#is_valid_light_client_header
    pub fn is_valid_light_client_header(&self) -> bool {
        true
    }
}
