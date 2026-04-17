use std::sync::Arc;

#[cfg(test)]
use mockall::automock;
use task_executor::TaskExecutor;
use types::{Hash256, SignedExecutionPayloadEnvelope, Slot};

use crate::{BeaconChainError, BeaconChainTypes, BeaconSystem};

/// An adapter to the `BeaconSystem` functionalities to remove `BeaconSystem` from direct dependency to enable testing envelope streamer logic.
pub(crate) struct EnvelopeStreamerBeaconAdapter<T: BeaconChainTypes> {
    chain: Arc<BeaconSystem<T>>,
}

#[cfg_attr(test, automock, allow(dead_code))]
impl<T: BeaconChainTypes> EnvelopeStreamerBeaconAdapter<T> {
    pub(crate) fn new(chain: Arc<BeaconSystem<T>>) -> Self {
        Self { chain }
    }

    pub(crate) fn executor(&self) -> &TaskExecutor {
        &self.chain.task_executor
    }

    pub(crate) fn get_payload_envelope(
        &self,
        root: &Hash256,
    ) -> Result<Option<SignedExecutionPayloadEnvelope<T::EthSpec>>, store::Error> {
        self.chain.store.get_payload_envelope(root)
    }

    pub(crate) fn get_split_slot(&self) -> Slot {
        self.chain.store.get_split_info().slot
    }

    pub(crate) fn block_has_canonical_payload(
        &self,
        root: &Hash256,
    ) -> Result<bool, BeaconChainError> {
        self.chain.canonical_head.block_has_canonical_payload(root)
    }
}
