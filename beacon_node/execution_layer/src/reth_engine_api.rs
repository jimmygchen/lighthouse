//! Direct in-process Reth Engine API - replaces HTTP JSON-RPC with direct function calls
//!
//! This module provides channel-based communication with a Reth execution engine running
//! in the same process, eliminating HTTP overhead.

use crate::engine_api::{
    BlockByNumberQuery, EngineCapabilities, Error as EngineApiError, ExecutionBlock,
    ExecutionPayloadBodyV1, ForkchoiceUpdatedResponse, GetPayloadResponse, NewPayloadRequest,
    PayloadAttributes, PayloadId,
};
use crate::engines::ForkchoiceState;
use crate::json_structures::{BlobAndProofV1, BlobAndProofV2};
use crate::ClientVersionV1;
use std::time::Duration;
use tracing::info;
use types::{EthSpec, ExecutionBlockHash, ForkName, Hash256};

// Reth imports
use reth_engine_primitives::ConsensusEngineHandle;
use tokio::sync::mpsc::unbounded_channel;

// Use Reth's built-in Ethereum engine types - already properly implemented!
use reth_ethereum_engine_primitives::EthEngineTypes;

/// Direct Reth Engine API handler - communicates with Reth in-process
/// Stores Reth's ConsensusEngineHandle and converts between Lighthouse and Reth types
pub struct RethEngineApi {
    /// Reth's consensus engine handle - this is the key integration point!
    reth_handle: ConsensusEngineHandle<EthEngineTypes>,
}

impl RethEngineApi {
    /// Create a new RethEngineApi by launching Reth and getting its ConsensusEngineHandle
    pub fn new() -> Result<Self, EngineApiError> {
        // Launch Reth and get the ConsensusEngineHandle
        let reth_handle = launch_reth_and_get_handle()
            .map_err(|e| {
                eprintln!("Failed to launch Reth: {}", e);
                EngineApiError::IsSyncing
            })?;

        info!("Successfully launched Reth and obtained ConsensusEngineHandle");

        Ok(Self { reth_handle })
    }

    /// Create RethEngineApi with stub for testing/POC
    #[allow(dead_code)]
    pub fn new_stub() -> Result<Self, EngineApiError> {
        // Create channel - this is what Reth uses internally!
        let (to_engine, from_consensus) = unbounded_channel();

        // Wrap in ConsensusEngineHandle - this is Reth's handle type
        let reth_handle = ConsensusEngineHandle::new(to_engine);

        // Spawn task that acts like Reth's engine (processes messages from the channel)
        spawn_stub_reth_engine_handler(from_consensus);

        Ok(Self { reth_handle })
    }

    /// Check if the engine is online and synced
    pub async fn upcheck(&self) -> Result<(), EngineApiError> {
        // TODO: Query Reth's engine status directly
        println!("RethEngineApi::upcheck() called - TODO: implement");
        Ok(())
    }

    /// Update fork choice and optionally request payload building
    pub async fn forkchoice_updated(
        &self,
        forkchoice_state: ForkchoiceState,
        payload_attributes: Option<PayloadAttributes>,
    ) -> Result<ForkchoiceUpdatedResponse, EngineApiError> {
        // Convert Lighthouse ForkchoiceState → Reth ForkchoiceState
        let reth_forkchoice = convert_lighthouse_to_reth_forkchoice(forkchoice_state);

        // Convert Lighthouse PayloadAttributes → Reth PayloadAttributes
        let reth_payload_attrs = payload_attributes.map(convert_lighthouse_to_reth_payload_attrs);

        // Call Reth's ConsensusEngineHandle directly!
        let reth_response = self
            .reth_handle
            .fork_choice_updated(
                reth_forkchoice,
                reth_payload_attrs,
                reth_payload_primitives::EngineApiMessageVersion::V3,
            )
            .await
            .map_err(|_| EngineApiError::IsSyncing)?;

        // Convert Reth response → Lighthouse response
        convert_reth_to_lighthouse_forkchoice_response(reth_response)
    }

    /// Get engine capabilities
    pub async fn get_engine_capabilities(
        &self,
        _age_limit: Option<Duration>,
    ) -> Result<EngineCapabilities, EngineApiError> {
        // TODO: Query Reth's engine capabilities dynamically
        // For now, return full capabilities for latest Ethereum spec
        Ok(EngineCapabilities {
            new_payload_v1: true,
            new_payload_v2: true,
            new_payload_v3: true,
            new_payload_v4: true,
            forkchoice_updated_v1: true,
            forkchoice_updated_v2: true,
            forkchoice_updated_v3: true,
            get_payload_bodies_by_hash_v1: true,
            get_payload_bodies_by_range_v1: true,
            get_payload_v1: true,
            get_payload_v2: true,
            get_payload_v3: true,
            get_payload_v4: true,
            get_payload_v5: true,
            get_client_version_v1: true,
            get_blobs_v1: true,
            get_blobs_v2: true,
        })
    }

    /// Get engine version
    pub async fn get_engine_version(
        &self,
        _age_limit: Option<Duration>,
    ) -> Result<Vec<ClientVersionV1>, EngineApiError> {
        // TODO: Query Reth's version info
        println!("RethEngineApi::get_engine_version() called - TODO: implement");
        Ok(vec![])
    }

    /// Clear capabilities cache
    pub async fn clear_exchange_capabilties_cache(&self) {
        // TODO: Clear cache if we implement caching
        println!("RethEngineApi::clear_exchange_capabilties_cache() called - TODO: implement");
    }

    /// Clear version cache
    pub async fn clear_engine_version_cache(&self) {
        // TODO: Clear cache if we implement caching
        println!("RethEngineApi::clear_engine_version_cache() called - TODO: implement");
    }

    /// Submit a new payload for execution
    pub async fn new_payload<E: EthSpec>(
        &self,
        _new_payload_request: NewPayloadRequest<'_, E>,
    ) -> Result<crate::engine_api::PayloadStatusV1, EngineApiError> {
        // TODO: Implement new_payload by calling Reth's engine
        // Would use self.reth_handle to send the payload
        println!("RethEngineApi::new_payload() called - TODO: implement");
        Err(EngineApiError::IsSyncing)
    }

    /// Get a payload by ID for block production
    pub async fn get_payload<E: EthSpec>(
        &self,
        _fork_name: ForkName,
        _payload_id: PayloadId,
    ) -> Result<GetPayloadResponse<E>, EngineApiError> {
        // TODO: Get payload from Reth engine
        println!("RethEngineApi::get_payload() called - TODO: implement");
        Err(EngineApiError::PayloadIdUnavailable)
    }

    /// Get execution block by hash
    pub async fn get_block_by_hash(
        &self,
        _block_hash: ExecutionBlockHash,
    ) -> Result<Option<ExecutionBlock>, EngineApiError> {
        // TODO: Query Reth for block by hash
        println!("RethEngineApi::get_block_by_hash() called - TODO: implement");
        Ok(None)
    }

    /// Get execution block by number
    pub async fn get_block_by_number(
        &self,
        _query: BlockByNumberQuery<'_>,
    ) -> Result<Option<ExecutionBlock>, EngineApiError> {
        // TODO: Query Reth for block by number
        println!("RethEngineApi::get_block_by_number() called - TODO: implement");
        Ok(None)
    }

    /// Get payload bodies by block hashes
    pub async fn get_payload_bodies_by_hash_v1<E: EthSpec>(
        &self,
        _block_hashes: Vec<ExecutionBlockHash>,
    ) -> Result<Vec<Option<ExecutionPayloadBodyV1<E>>>, EngineApiError> {
        // TODO: Query Reth for payload bodies
        println!("RethEngineApi::get_payload_bodies_by_hash_v1() called - TODO: implement");
        Ok(vec![])
    }

    /// Get payload bodies by block range
    pub async fn get_payload_bodies_by_range_v1<E: EthSpec>(
        &self,
        _start: u64,
        _count: u64,
    ) -> Result<Vec<Option<ExecutionPayloadBodyV1<E>>>, EngineApiError> {
        // TODO: Query Reth for payload bodies by range
        println!("RethEngineApi::get_payload_bodies_by_range_v1() called - TODO: implement");
        Ok(vec![])
    }

    /// Get blobs by versioned hashes (v1)
    pub async fn get_blobs_v1<E: EthSpec>(
        &self,
        _versioned_hashes: Vec<Hash256>,
    ) -> Result<Vec<Option<BlobAndProofV1<E>>>, EngineApiError> {
        // TODO: Query Reth for blobs
        println!("RethEngineApi::get_blobs_v1() called - TODO: implement");
        Ok(vec![])
    }

    /// Get blobs by versioned hashes (v2)
    pub async fn get_blobs_v2<E: EthSpec>(
        &self,
        _versioned_hashes: Vec<Hash256>,
    ) -> Result<Option<Vec<BlobAndProofV2<E>>>, EngineApiError> {
        // TODO: Query Reth for blobs
        println!("RethEngineApi::get_blobs_v2() called - TODO: implement");
        Ok(None)
    }
}

impl std::fmt::Display for RethEngineApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RethEngineApi (in-process)")
    }
}

// ============================================================================
// Type Conversion Functions
// ============================================================================

/// Convert Lighthouse ForkchoiceState → Reth ForkchoiceState
fn convert_lighthouse_to_reth_forkchoice(
    lh: ForkchoiceState,
) -> alloy_rpc_types_engine::ForkchoiceState {
    use alloy_primitives::B256;
    alloy_rpc_types_engine::ForkchoiceState {
        head_block_hash: B256::from_slice(lh.head_block_hash.0.as_ref()),
        safe_block_hash: B256::from_slice(lh.safe_block_hash.0.as_ref()),
        finalized_block_hash: B256::from_slice(lh.finalized_block_hash.0.as_ref()),
    }
}

/// Convert Lighthouse PayloadAttributes → Reth PayloadAttributes
fn convert_lighthouse_to_reth_payload_attrs(
    _lh: PayloadAttributes,
) -> alloy_rpc_types_engine::PayloadAttributes {
    // TODO: Implement full conversion
    // For now, return a minimal stub
    todo!("Payload attributes conversion not yet implemented")
}

/// Convert Reth ForkchoiceUpdated response → Lighthouse response
fn convert_reth_to_lighthouse_forkchoice_response(
    reth: alloy_rpc_types_engine::ForkchoiceUpdated,
) -> Result<ForkchoiceUpdatedResponse, EngineApiError> {
    use crate::engine_api::{PayloadStatusV1, PayloadStatusV1Status};
    use alloy_rpc_types_engine::PayloadStatusEnum;

    let payload_status = PayloadStatusV1 {
        status: match reth.payload_status.status {
            PayloadStatusEnum::Valid => PayloadStatusV1Status::Valid,
            PayloadStatusEnum::Invalid { validation_error: _ } => PayloadStatusV1Status::Invalid,
            PayloadStatusEnum::Syncing => PayloadStatusV1Status::Syncing,
            PayloadStatusEnum::Accepted => PayloadStatusV1Status::Accepted,
        },
        latest_valid_hash: reth
            .payload_status
            .latest_valid_hash
            .map(|h| ExecutionBlockHash::from(Hash256::from_slice(h.as_slice()))),
        validation_error: match reth.payload_status.status {
            PayloadStatusEnum::Invalid { validation_error } => Some(validation_error),
            _ => None
        },
    };

    Ok(ForkchoiceUpdatedResponse {
        payload_status,
        payload_id: reth.payload_id.map(|id| id.0.into()),
    })
}

// ============================================================================
// Stub Reth Engine Handler
// ============================================================================

/// Spawn a stub handler that processes Reth's BeaconEngineMessage
///
/// This receives messages via the channel that ConsensusEngineHandle sends to
/// In a real implementation, this would be Reth's EngineService
fn spawn_stub_reth_engine_handler(
    mut from_consensus: tokio::sync::mpsc::UnboundedReceiver<
        reth_engine_primitives::BeaconEngineMessage<EthEngineTypes>,
    >,
) {
    tokio::spawn(async move {
        use reth_engine_primitives::BeaconEngineMessage;

        println!("Stub Reth engine handler started - processing BeaconEngineMessages...");

        while let Some(message) = from_consensus.recv().await {
            match message {
                BeaconEngineMessage::NewPayload { payload, tx } => {
                    println!("[Reth Engine] Received NewPayload via ConsensusEngineHandle");

                    // In real implementation, this would call Reth's execution engine
                    // For now, return a stub response
                    use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
                    let status = PayloadStatus::new(PayloadStatusEnum::Valid, Some(payload.block_hash()));
                    let _ = tx.send(Ok(status));
                }

                BeaconEngineMessage::ForkchoiceUpdated {
                    state,
                    payload_attrs,
                    tx,
                    version: _,
                } => {
                    println!(
                        "[Reth Engine] Received ForkchoiceUpdated via ConsensusEngineHandle - head: {:?}",
                        state.head_block_hash
                    );

                    // In real implementation, this would call Reth's execution engine
                    // For now, return a stub response
                    use reth_engine_primitives::OnForkChoiceUpdated;
                    use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum as AlloyStatus};
                    let status = PayloadStatus::new(AlloyStatus::Valid, Some(state.head_block_hash));
                    let response = OnForkChoiceUpdated::valid(status);
                    let _ = tx.send(Ok(response));
                }
            }
        }

        println!("Stub Reth engine handler shut down");
    });
}

// ============================================================================
// Reth Launch Function
// ============================================================================

/// Launch Reth and return the ConsensusEngineHandle
///
/// This initializes Reth's full node and returns the handle we can use to send
/// Engine API messages to it.
///
/// TODO: Configuration should come from Lighthouse config:
/// - Data directory path
/// - Chain spec (mainnet, sepolia, etc.)
/// - Database backend choice
fn launch_reth_and_get_handle() -> Result<ConsensusEngineHandle<EthEngineTypes>, String> {
    use reth_ethereum::{
        node::{builder::{NodeBuilder, NodeHandle}, node::EthereumNode, core::node_config::NodeConfig},
        chainspec::MAINNET,
        tasks::TaskManager,
    };

    info!("Launching Reth execution engine in-process...");

    // Create task manager for Reth
    let tasks = TaskManager::current();

    // Create node config for mainnet development
    // TODO: Production setup needs:
    // 1. Proper data directory from Lighthouse config (not hardcoded)
    // 2. Persistent database instead of testing_node
    // 3. Database initialization and migration handling
    // 4. Proper chain spec configuration from Lighthouse
    let config = NodeConfig::new(MAINNET.clone())
        .dev();

    // Channel to extract the ConsensusEngineHandle
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();

    // Launch Reth in background task
    tokio::spawn(async move {
        match NodeBuilder::new(config)
            .testing_node(tasks.executor())
            .node(EthereumNode::default())
            .on_node_started(move |full_node| {
                // Extract the consensus engine handle from the node
                let handle = full_node.add_ons_handle.consensus_engine_handle().clone();
                let _ = handle_tx.send(handle);
                Ok(())
            })
            .launch_with_debug_capabilities()
            .await
        {
            Ok(handle) => {
                info!("Reth execution engine launched successfully");
                // Keep node running
                let _ = handle.wait_for_node_exit().await;
            }
            Err(e) => {
                eprintln!("Failed to launch Reth: {}", e);
            }
        }
    });

    // Wait for handle with timeout
    handle_rx.recv_timeout(Duration::from_secs(30))
        .map_err(|_| "Timeout waiting for Reth to launch".to_string())
        .map(|h| h.clone())
}
