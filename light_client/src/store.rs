use safe_arith::ArithError;
use std::sync::Arc;
use tree_hash::TreeHash;
use types::light_client_bootstrap::LightClientBootstrap;
use types::light_client_update::LightClientUpdate;
use types::{ChainSpec, EthSpec, ForkVersionedResponse, Hash256, LightClientHeader, SyncCommittee};

const CURRENT_SYNC_COMMITTEE_INDEX: u64 = 54;

#[derive(Debug)]
pub enum StoreError {
    InvalidLightClientHeader,
    TrustedBlockRootMismatch,
    BadMerkleProof,
}

/// Initializes a new `LightClientStore` with a received `LightClientBootstrap` derived from a
/// given `trusted_block_root`.
///
/// https://github.com/ethereum/consensus-specs/blob/dev/specs/altair/light-client/sync-protocol.md#initialize_light_client_store
pub fn initialize_light_client_store<E: EthSpec>(
    trusted_block_root: Hash256,
    bootstrap: ForkVersionedResponse<LightClientBootstrap<E>>,
) -> Result<LightClientStore<E>, StoreError> {
    let LightClientBootstrap {
        header,
        current_sync_committee,
        current_sync_committee_branch: _current_sync_committee_branch,
    } = bootstrap.data;

    let lc_header: LightClientHeader = header.into();
    if !lc_header.is_valid_light_client_header() {
        return Err(StoreError::InvalidLightClientHeader);
    }

    if lc_header.beacon.tree_hash_root() != trusted_block_root {
        return Err(StoreError::TrustedBlockRootMismatch);
    }

    Ok(LightClientStore {
        finalized_header: lc_header.clone(),
        current_sync_committee,
        next_sync_committee: Arc::new(SyncCommittee::temporary()),
        best_valid_update: None,
        optimistic_header: lc_header,
        previous_max_active_participants: 0,
        current_max_active_participants: 0,
    })
}

/// Object to store the light client state.
///
/// https://github.com/ethereum/consensus-specs/blob/dev/specs/altair/light-client/sync-protocol.md#lightclientstore
pub struct LightClientStore<E: EthSpec> {
    /// Header that is finalized
    pub finalized_header: LightClientHeader,
    ///Sync committees corresponding to the finalized header
    pub current_sync_committee: Arc<SyncCommittee<E>>,
    pub next_sync_committee: Arc<SyncCommittee<E>>,
    ///Best available header to switch finalized head to if we see nothing else
    pub best_valid_update: Option<LightClientUpdate<E>>,
    ///Most recent available reasonably-safe header
    pub optimistic_header: LightClientHeader,
    ///Max number of active participants in a sync committee (used to calculate safety threshold)
    pub previous_max_active_participants: u64,
    pub current_max_active_participants: u64,
}

impl<E: EthSpec> LightClientStore<E> {
    pub fn finalized_period(&self, spec: &ChainSpec) -> Result<u64, ArithError> {
        self.finalized_header
            .beacon
            .slot
            .epoch(E::slots_per_epoch())
            .sync_committee_period(spec)
    }

    pub fn optimistic_period(&self, spec: &ChainSpec) -> Result<u64, ArithError> {
        self.optimistic_header
            .beacon
            .slot
            .epoch(E::slots_per_epoch())
            .sync_committee_period(spec)
    }

    pub fn is_next_sync_committee_known(&self) -> bool {
        *self.next_sync_committee == SyncCommittee::temporary()
    }
}
