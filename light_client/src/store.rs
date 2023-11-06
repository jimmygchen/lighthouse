use crate::types::LightClientHeader;
use std::sync::Arc;
use types::light_client_bootstrap::LightClientBootstrap;
use types::light_client_update::LightClientUpdate;
use types::{EthSpec, Hash256, SyncCommittee};

/// Initializes a new `LightClientStore` with a received `LightClientBootstrap` derived from a
/// given `trusted_block_root`.
///
/// https://github.com/ethereum/consensus-specs/blob/dev/specs/altair/light-client/sync-protocol.md#initialize_light_client_store
pub fn initialize_light_client_store<E: EthSpec>(
    _trusted_block_root: Hash256,
    bootstrap: LightClientBootstrap<E>,
) -> LightClientStore<E> {
    let LightClientBootstrap {
        header,
        current_sync_committee,
        current_sync_committee_branch: _current_sync_committee_branch,
    } = bootstrap;

    LightClientStore {
        finalized_header: header.clone().into(),
        current_sync_committee,
        next_sync_committee: None,
        best_valid_update: None,
        optimistic_header: header.into(),
        previous_max_active_participants: 0,
        current_max_active_participants: 0,
    }
}

/// Object to store the light client state.
///
/// https://github.com/ethereum/consensus-specs/blob/dev/specs/altair/light-client/sync-protocol.md#lightclientstore
pub struct LightClientStore<E: EthSpec> {
    /// Header that is finalized
    finalized_header: LightClientHeader,
    ///Sync committees corresponding to the finalized header
    current_sync_committee: Arc<SyncCommittee<E>>,
    next_sync_committee: Option<SyncCommittee<E>>,
    ///Best available header to switch finalized head to if we see nothing else
    best_valid_update: Option<LightClientUpdate<E>>,
    ///Most recent available reasonably-safe header
    optimistic_header: LightClientHeader,
    ///Max number of active participants in a sync committee (used to calculate safety threshold)
    previous_max_active_participants: u64,
    current_max_active_participants: u64,
}
