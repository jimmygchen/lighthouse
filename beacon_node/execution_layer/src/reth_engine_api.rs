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
use tracing::{error, info};
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
    /// Create a new RethEngineApi by launching Reth with configuration
    pub fn new(config: RethConfig) -> Result<Self, EngineApiError> {
        // Launch Reth with the provided configuration
        let reth_handle = launch_reth_and_get_handle_with_config(config)
            .map_err(|e| {
                eprintln!("Failed to launch Reth: {}", e);
                EngineApiError::IsSyncing
            })?;

        info!("Successfully launched Reth and obtained ConsensusEngineHandle");

        Ok(Self { reth_handle })
    }

    /// Create a new RethEngineApi with default configuration
    #[allow(dead_code)]
    pub fn new_default() -> Result<Self, EngineApiError> {
        Self::new(RethConfig::default())
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
        new_payload_request: NewPayloadRequest<'_, E>,
    ) -> Result<crate::engine_api::PayloadStatusV1, EngineApiError> {
        info!(
            block_number = new_payload_request.block_number(),
            block_hash = ?new_payload_request.block_hash(),
            "RethEngineApi::new_payload() called"
        );

        // Convert Lighthouse ExecutionPayload to Alloy ExecutionPayload
        let alloy_payload = convert_lighthouse_to_alloy_payload(new_payload_request)
            .map_err(|e| {
                error!("Failed to convert payload: {}", e);
                EngineApiError::PayloadIdUnavailable
            })?;

        // Call Reth's new_payload via ConsensusEngineHandle
        // TODO: Complete type conversion - ExecutionData is just field mapping from ExecutionPayload
        // The alloy_payload needs to be converted to Reth's ExecutionData type
        // which for EthEngineTypes should match the ExecutionPayload structure
        info!("new_payload called - TODO: complete ExecutionData type conversion");

        // Temporary error until type conversion is completed
        Err(EngineApiError::PayloadIdUnavailable)
    }

    /// Get a payload by ID for block production
    pub async fn get_payload<E: EthSpec>(
        &self,
        fork_name: ForkName,
        payload_id: PayloadId,
    ) -> Result<GetPayloadResponse<E>, EngineApiError> {
        use tracing::warn;

        warn!(
            fork = ?fork_name,
            payload_id = ?payload_id,
            "RethEngineApi::get_payload() called - implementation needed"
        );

        // TODO: Full implementation
        // get_payload requires access to Reth's payload builder, not just the consensus engine.
        // This typically involves:
        // 1. Converting the payload_id to Reth's format
        // 2. Calling Reth's payload builder API (separate from ConsensusEngineHandle)
        // 3. Converting the returned ExecutionPayload back to Lighthouse format
        //
        // For now, return an error to indicate this is not yet implemented
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
    lh: PayloadAttributes,
) -> alloy_rpc_types_engine::PayloadAttributes {
    use alloy_primitives::{Address as AlloyAddress, B256};
    use alloy_rpc_types_engine::PayloadAttributes as RethPayloadAttributes;

    match lh {
        PayloadAttributes::V1(attrs) => {
            RethPayloadAttributes {
                timestamp: attrs.timestamp,
                prev_randao: B256::from_slice(attrs.prev_randao.as_ref()),
                suggested_fee_recipient: AlloyAddress::from(attrs.suggested_fee_recipient.0),
                withdrawals: None,
                parent_beacon_block_root: None,
            }
        }
        PayloadAttributes::V2(attrs) => {
            RethPayloadAttributes {
                timestamp: attrs.timestamp,
                prev_randao: B256::from_slice(attrs.prev_randao.as_ref()),
                suggested_fee_recipient: AlloyAddress::from(attrs.suggested_fee_recipient.0),
                withdrawals: Some(attrs.withdrawals.into_iter().map(convert_withdrawal).collect()),
                parent_beacon_block_root: None,
            }
        }
        PayloadAttributes::V3(attrs) => {
            RethPayloadAttributes {
                timestamp: attrs.timestamp,
                prev_randao: B256::from_slice(attrs.prev_randao.as_ref()),
                suggested_fee_recipient: AlloyAddress::from(attrs.suggested_fee_recipient.0),
                withdrawals: Some(attrs.withdrawals.into_iter().map(convert_withdrawal).collect()),
                parent_beacon_block_root: Some(B256::from_slice(attrs.parent_beacon_block_root.as_ref())),
            }
        }
    }
}

/// Convert Lighthouse Withdrawal → Reth/Alloy Withdrawal
fn convert_withdrawal(lh: types::Withdrawal) -> alloy_eips::eip4895::Withdrawal {
    use alloy_primitives::Address as AlloyAddress;

    alloy_eips::eip4895::Withdrawal {
        index: lh.index,
        validator_index: lh.validator_index,
        address: AlloyAddress::from(lh.address.0),
        amount: lh.amount,
    }
}

/// Convert Lighthouse ExecutionPayload → Alloy ExecutionPayload for new_payload
fn convert_lighthouse_to_alloy_payload<E: EthSpec>(
    request: NewPayloadRequest<'_, E>,
) -> Result<alloy_rpc_types_engine::ExecutionPayload, String> {
    use alloy_primitives::{Address as AlloyAddress, Bloom, Bytes, B256, U256};
    use alloy_rpc_types_engine::{
        ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
    };

    match request {
        NewPayloadRequest::Bellatrix(payload_request) => {
            let payload = payload_request.execution_payload;

            let alloy_payload = ExecutionPayloadV1 {
                parent_hash: B256::from_slice(payload.parent_hash.0.as_ref()),
                fee_recipient: AlloyAddress::from(payload.fee_recipient.0),
                state_root: B256::from_slice(payload.state_root.as_ref()),
                receipts_root: B256::from_slice(payload.receipts_root.as_ref()),
                logs_bloom: Bloom::from_slice(payload.logs_bloom.as_ref()),
                prev_randao: B256::from_slice(payload.prev_randao.as_ref()),
                block_number: payload.block_number,
                gas_limit: payload.gas_limit,
                gas_used: payload.gas_used,
                timestamp: payload.timestamp,
                extra_data: Bytes::copy_from_slice(payload.extra_data.as_ref()),
                base_fee_per_gas: U256::from_be_bytes::<32>(payload.base_fee_per_gas.to_be_bytes::<32>()),
                block_hash: B256::from_slice(payload.block_hash.0.as_ref()),
                transactions: payload
                    .transactions
                    .iter()
                    .map(|tx| Bytes::copy_from_slice(tx.as_ref()))
                    .collect(),
            };

            Ok(alloy_rpc_types_engine::ExecutionPayload::V1(alloy_payload))
        }
        NewPayloadRequest::Capella(payload_request) => {
            let payload = payload_request.execution_payload;

            let alloy_payload = ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::from_slice(payload.parent_hash.0.as_ref()),
                    fee_recipient: AlloyAddress::from(payload.fee_recipient.0),
                    state_root: B256::from_slice(payload.state_root.as_ref()),
                    receipts_root: B256::from_slice(payload.receipts_root.as_ref()),
                    logs_bloom: Bloom::from_slice(payload.logs_bloom.as_ref()),
                    prev_randao: B256::from_slice(payload.prev_randao.as_ref()),
                    block_number: payload.block_number,
                    gas_limit: payload.gas_limit,
                    gas_used: payload.gas_used,
                    timestamp: payload.timestamp,
                    extra_data: Bytes::copy_from_slice(payload.extra_data.as_ref()),
                    base_fee_per_gas: U256::from_be_bytes::<32>(payload.base_fee_per_gas.to_be_bytes::<32>()),
                    block_hash: B256::from_slice(payload.block_hash.0.as_ref()),
                    transactions: payload
                        .transactions
                        .iter()
                        .map(|tx| Bytes::copy_from_slice(tx.as_ref()))
                        .collect(),
                },
                withdrawals: payload
                    .withdrawals
                    .iter()
                    .map(|w| convert_withdrawal(w.clone()))
                    .collect(),
            };

            Ok(alloy_rpc_types_engine::ExecutionPayload::V2(alloy_payload))
        }
        NewPayloadRequest::Deneb(payload_request) => {
            let payload = payload_request.execution_payload;

            let alloy_payload = ExecutionPayloadV3 {
                payload_inner: ExecutionPayloadV2 {
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: B256::from_slice(payload.parent_hash.0.as_ref()),
                        fee_recipient: AlloyAddress::from(payload.fee_recipient.0),
                        state_root: B256::from_slice(payload.state_root.as_ref()),
                        receipts_root: B256::from_slice(payload.receipts_root.as_ref()),
                        logs_bloom: Bloom::from_slice(payload.logs_bloom.as_ref()),
                        prev_randao: B256::from_slice(payload.prev_randao.as_ref()),
                        block_number: payload.block_number,
                        gas_limit: payload.gas_limit,
                        gas_used: payload.gas_used,
                        timestamp: payload.timestamp,
                        extra_data: Bytes::copy_from_slice(payload.extra_data.as_ref()),
                        base_fee_per_gas: U256::from_be_bytes::<32>(payload.base_fee_per_gas.to_be_bytes::<32>()),
                        block_hash: B256::from_slice(payload.block_hash.0.as_ref()),
                        transactions: payload
                            .transactions
                            .iter()
                            .map(|tx| Bytes::copy_from_slice(tx.as_ref()))
                            .collect(),
                    },
                    withdrawals: payload
                        .withdrawals
                        .iter()
                        .map(|w| convert_withdrawal(w.clone()))
                        .collect(),
                },
                blob_gas_used: payload.blob_gas_used,
                excess_blob_gas: payload.excess_blob_gas,
            };

            Ok(alloy_rpc_types_engine::ExecutionPayload::V3(alloy_payload))
        }
        // Electra, Fulu, and Gloas - not yet implemented due to missing Alloy types
        // TODO: Implement once ExecutionPayloadV4 and EIP-7685 types are available in alloy
        NewPayloadRequest::Electra(_)
        | NewPayloadRequest::Fulu(_)
        | NewPayloadRequest::Gloas(_) => {
            Err("Electra/Fulu/Gloas fork conversions not yet implemented - missing Alloy types".to_string())
        }
    }
}

/// Convert Alloy PayloadStatus → Lighthouse PayloadStatusV1
fn convert_alloy_to_lighthouse_payload_status(
    alloy_status: alloy_rpc_types_engine::PayloadStatus,
) -> Result<crate::engine_api::PayloadStatusV1, EngineApiError> {
    use crate::engine_api::{PayloadStatusV1, PayloadStatusV1Status};
    use alloy_rpc_types_engine::PayloadStatusEnum;

    Ok(PayloadStatusV1 {
        status: match alloy_status.status {
            PayloadStatusEnum::Valid => PayloadStatusV1Status::Valid,
            PayloadStatusEnum::Invalid { validation_error: _ } => PayloadStatusV1Status::Invalid,
            PayloadStatusEnum::Syncing => PayloadStatusV1Status::Syncing,
            PayloadStatusEnum::Accepted => PayloadStatusV1Status::Accepted,
        },
        latest_valid_hash: alloy_status
            .latest_valid_hash
            .map(|h| ExecutionBlockHash::from(Hash256::from_slice(h.as_slice()))),
        validation_error: match alloy_status.status {
            PayloadStatusEnum::Invalid { validation_error } => Some(validation_error),
            _ => None,
        },
    })
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
                    payload_attrs: _,
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

/// Configuration for launching Reth
#[derive(Debug, Clone)]
pub struct RethConfig {
    /// Data directory for Reth database
    pub datadir: std::path::PathBuf,
    /// Chain specification (mainnet, sepolia, holesky, etc.)
    pub chain_spec: std::sync::Arc<reth_chainspec::ChainSpec>,
}

impl Default for RethConfig {
    fn default() -> Self {
        use reth_ethereum::chainspec::MAINNET;
        Self {
            datadir: std::path::PathBuf::from("/tmp/reth-dev"),
            chain_spec: MAINNET.clone(),
        }
    }
}

/// Launch Reth and return the ConsensusEngineHandle
///
/// This initializes Reth's full node with a persistent database and returns
/// the handle we can use to send Engine API messages to it.
///
/// The function accepts a RethConfig to specify:
/// - Data directory path for persistent storage
/// - Chain spec (mainnet, sepolia, holesky, gnosis, etc.)
fn launch_reth_and_get_handle_with_config(
    config: RethConfig,
) -> Result<ConsensusEngineHandle<EthEngineTypes>, String> {
    use reth_ethereum::{
        node::{builder::NodeBuilder, node::EthereumNode, core::node_config::NodeConfig},
        tasks::TaskManager,
    };
    use reth_db::{mdbx::DatabaseArguments, ClientVersion, DatabaseEnv};
    use std::sync::Arc;

    info!(
        datadir = %config.datadir.display(),
        chain = %config.chain_spec.chain.to_string(),
        "Launching Reth execution engine in-process with persistent database"
    );

    // Create data directory if it doesn't exist
    std::fs::create_dir_all(&config.datadir)
        .map_err(|e| format!("Failed to create data directory: {}", e))?;

    info!("Created data directory, initializing task manager");

    // Create task manager for Reth
    let tasks = TaskManager::current();

    info!("Task manager created, opening database");

    // Open persistent database
    let db_path = config.datadir.join("db");
    info!(db_path = %db_path.display(), "Attempting to open Reth database");

    let db = Arc::new(
        DatabaseEnv::open(
            &db_path,
            reth_db::DatabaseEnvKind::RW,
            DatabaseArguments::new(ClientVersion::default()),
        )
        .map_err(|e| format!("Failed to open database: {}", e))?
    );

    info!(db_path = %db_path.display(), "Opened persistent Reth database");

    // Create node config with persistent database
    info!("Creating node config");
    let node_config = NodeConfig::new(config.chain_spec);

    // Channel to extract the ConsensusEngineHandle or error
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let error_tx = handle_tx.clone();

    info!("Spawning Reth node launch task");

    // Launch Reth in background task with persistent database
    tokio::spawn(async move {
        info!("Starting Reth node launch...");
        info!("Building Reth node with NodeBuilder");

        match NodeBuilder::new(node_config)
            .with_database(db)
            .with_launch_context(tasks.executor())
            .node(EthereumNode::default())
            .on_node_started(move |full_node| {
                info!("Reth node started, extracting consensus engine handle");
                // Extract the consensus engine handle from the node
                let handle = full_node.add_ons_handle.consensus_engine_handle().clone();
                info!("Successfully extracted consensus engine handle");
                let _ = handle_tx.send(Ok(handle));
                Ok(())
            })
            .launch()
            .await
        {
            Ok(handle) => {
                info!("Reth execution engine launched successfully with persistent database");
                // Keep node running
                info!("Waiting for node exit...");
                let _ = handle.wait_for_node_exit().await;
            }
            Err(e) => {
                error!("Failed to launch Reth: {}", e);
                // Send error through channel so we don't timeout
                let _ = error_tx.send(Err(format!("Reth launch failed: {}", e)));
            }
        }
    });

    // Wait for handle with longer timeout (Reth initialization can take time)
    info!("Waiting for Reth to initialize...");
    handle_rx.recv_timeout(Duration::from_secs(120))
        .map_err(|e| format!("Timeout waiting for Reth to launch: {}", e))?
        .map_err(|e| format!("Reth launch error: {}", e))
}
