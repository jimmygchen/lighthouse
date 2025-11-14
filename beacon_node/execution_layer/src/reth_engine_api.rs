//! Direct in-process Reth Engine API - replaces HTTP JSON-RPC with direct function calls
//!
//! This module provides channel-based communication with a Reth execution engine running
//! in the same process, eliminating HTTP overhead.

use crate::engine_api::{
    BlockByNumberQuery, EngineCapabilities, Error as EngineApiError, Error, ExecutionBlock,
    ExecutionPayloadBodyV1, ForkchoiceUpdatedResponse, GetPayloadResponse, NewPayloadRequest,
    PayloadAttributes, PayloadId,
};
use crate::engines::ForkchoiceState;
use crate::json_structures::{BlobAndProofV1, BlobAndProofV2};
use crate::{ClientVersionV1, GetPayloadResponseElectra, GetPayloadResponseFulu};
use alloy_eips::eip7685::Requests;
use alloy_primitives::{B256, Bytes as AlloyBytes, U256};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, CancunPayloadFields, ExecutionPayloadEnvelopeV3,
    ExecutionPayloadEnvelopeV4, ExecutionPayloadEnvelopeV5, ExecutionPayloadSidecar,
    ExecutionPayloadV3, PraguePayloadFields,
};
use core::fmt;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, instrument, warn};
use types::{
    Blob, EthSpec, ExecutionBlockHash, ExecutionPayloadElectra, ExecutionPayloadFulu,
    ExecutionRequests, FixedVector, ForkName, Hash256, RequestType, VariableList,
    execution_requests::{ConsolidationRequests, DepositRequests, WithdrawalRequests},
};

// Reth imports
use reth_ethereum::pool::EthTransactionPool;
use reth_ethereum::pool::blobstore::DiskFileBlobStore;
use reth_ethereum::rpc::EngineApi;

use alloy_rpc_types_engine::payload::ExecutionData;
use eth2::types::BlobsBundle;
use kzg::{KzgCommitment, KzgProof};
use reth_ethereum::rpc::api::EngineApiServer;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_node_builder::EngineApiExt;
use reth_node_builder::rpc::BasicEngineApiBuilder;
use reth_node_core::args::DatadirArgs;
use reth_node_ethereum::{
    EthereumAddOns, EthereumEngineValidator, EthereumEngineValidatorBuilder, EthereumNode,
};
use ssz::Decode;
use task_executor::TaskExecutor;

/// Direct Reth Engine API handler - communicates with Reth in-process
/// Stores Reth's EngineApi and converts between Lighthouse and Reth types
pub struct RethEngineApi {
    /// Reth's engine API handle - this is the key integration point!
    reth_handle: RethHandle,
}

// Type aliases to avoid repetition in the complex generic parameters
type EthereumProvider = reth_provider::providers::BlockchainProvider<
    reth_node_types::NodeTypesWithDBAdapter<EthereumNode, Arc<reth_db::DatabaseEnv>>,
>;

// Type alias for the concrete Reth Engine Api type
type RethHandle = EngineApi<
    EthereumProvider,
    EthEngineTypes,
    EthTransactionPool<EthereumProvider, DiskFileBlobStore>,
    EthereumEngineValidator,
    reth_chainspec::ChainSpec,
>;

impl RethEngineApi {
    /// Create a new RethEngineApi by launching Reth with configuration
    pub fn new(config: RethConfig, executor: TaskExecutor) -> Result<Self, EngineApiError> {
        // Launch Reth with the provided configuration
        let reth_handle =
            launch_reth_and_get_handle_with_config(config, executor).map_err(|e| {
                error!("Failed to launch Reth: {}", e);
                EngineApiError::IsSyncing
            })?;

        info!("Successfully launched Reth and obtained EngineApi handle");

        Ok(Self { reth_handle })
    }

    /// Check if the engine is online and synced
    pub async fn upcheck(&self) -> Result<(), EngineApiError> {
        // TODO: Query Reth's engine status directly
        debug!("upcheck() called - returning Ok (TODO: implement proper health check)");
        Err(EngineApiError::IsSyncing) // return IsSyncing until we have engine API interactions implemented
    }

    /// Update fork choice and optionally request payload building
    pub async fn forkchoice_updated(
        &self,
        forkchoice_state: ForkchoiceState,
        maybe_payload_attributes: Option<PayloadAttributes>,
    ) -> Result<ForkchoiceUpdatedResponse, EngineApiError> {
        let engine_capabilities = self.get_engine_capabilities()?;

        // Convert Lighthouse ForkchoiceState → Reth ForkchoiceState
        let reth_forkchoice_state = convert_lighthouse_to_reth_forkchoice(forkchoice_state);

        // Match on the original Lighthouse PayloadAttributes to determine which version to use
        let result = if let Some(payload_attributes) = maybe_payload_attributes.as_ref() {
            // Convert to Alloy types
            let reth_payload_attrs = Some(convert_lighthouse_to_reth_payload_attrs(
                payload_attributes.clone(),
            ));

            match payload_attributes {
                PayloadAttributes::V1(_) | PayloadAttributes::V2(_) => {
                    if engine_capabilities.forkchoice_updated_v2 {
                        self.reth_handle
                            .fork_choice_updated_v2(reth_forkchoice_state, reth_payload_attrs)
                            .await
                            .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
                    } else if engine_capabilities.forkchoice_updated_v1 {
                        self.reth_handle
                            .fork_choice_updated_v1(reth_forkchoice_state, reth_payload_attrs)
                            .await
                            .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
                    } else {
                        Err(EngineApiError::RequiredMethodUnsupported(
                            "engine_forkchoiceUpdated",
                        ))
                    }
                }
                PayloadAttributes::V3(_) => {
                    if engine_capabilities.forkchoice_updated_v3 {
                        self.reth_handle
                            .fork_choice_updated_v3(reth_forkchoice_state, reth_payload_attrs)
                            .await
                            .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
                    } else {
                        Err(EngineApiError::RequiredMethodUnsupported(
                            "engine_forkchoiceUpdatedV3",
                        ))
                    }
                }
            }
        } else if engine_capabilities.forkchoice_updated_v3 {
            self.reth_handle
                .fork_choice_updated_v3(reth_forkchoice_state, None)
                .await
                .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
        } else if engine_capabilities.forkchoice_updated_v2 {
            self.reth_handle
                .fork_choice_updated_v2(reth_forkchoice_state, None)
                .await
                .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
        } else if engine_capabilities.forkchoice_updated_v1 {
            self.reth_handle
                .fork_choice_updated_v1(reth_forkchoice_state, None)
                .await
                .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
        } else {
            Err(EngineApiError::RequiredMethodUnsupported(
                "engine_forkchoiceUpdated",
            ))
        };

        // Convert Reth response → Lighthouse response
        result.and_then(reth_to_lighthouse_forkchoice_response)
    }

    /// Get engine capabilities
    pub fn get_engine_capabilities(&self) -> Result<EngineCapabilities, EngineApiError> {
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
        debug!("get_engine_version() called - returning empty vec (TODO: implement)");
        Ok(vec![])
    }

    /// Clear capabilities cache
    pub async fn clear_exchange_capabilties_cache(&self) {
        // TODO: Clear cache if we implement caching
        debug!("clear_exchange_capabilties_cache() called (no-op)");
    }

    /// Clear version cache
    pub async fn clear_engine_version_cache(&self) {
        // TODO: Clear cache if we implement caching
        debug!("clear_engine_version_cache() called (no-op)");
    }

    /// Submit a new payload for execution
    pub async fn new_payload<E: EthSpec>(
        &self,
        new_payload_request: NewPayloadRequest<'_, E>,
    ) -> Result<crate::engine_api::PayloadStatusV1, EngineApiError> {
        let engine_capabilities = self.get_engine_capabilities()?;

        // Match on request variant and convert to Alloy types per-variant
        let execution_payload = match new_payload_request {
            NewPayloadRequest::Electra(payload_request) => {
                if !engine_capabilities.new_payload_v4 {
                    return Err(EngineApiError::RequiredMethodUnsupported(
                        "engine_newPayloadV4",
                    ));
                }

                // Convert payload to Alloy format
                convert_lighthouse_to_reth_payload(NewPayloadRequest::Electra(payload_request))
                    .map_err(|e| {
                        error!("Failed to convert Electra payload: {}", e);
                        EngineApiError::PayloadIdUnavailable
                    })?
            }
            NewPayloadRequest::Fulu(payload_request) => {
                if !engine_capabilities.new_payload_v4 {
                    return Err(EngineApiError::RequiredMethodUnsupported(
                        "engine_newPayloadV4",
                    ));
                }

                convert_lighthouse_to_reth_payload(NewPayloadRequest::Fulu(payload_request))
                    .map_err(|e| {
                        error!("Failed to convert Fulu payload: {}", e);
                        EngineApiError::PayloadIdUnavailable
                    })?
            }
            NewPayloadRequest::Bellatrix(_)
            | NewPayloadRequest::Capella(_)
            | NewPayloadRequest::Deneb(_)
            | NewPayloadRequest::Gloas(_) => {
                return Err(EngineApiError::UnsupportedForkVariant(
                    "Unsupported fork for Reth Engine API integration".to_string(),
                ));
            }
        };

        let reth_response = self
            .spawn_reth_task(move |reth_handle| async move {
                reth_handle
                    .new_payload_v4(execution_payload)
                    .await
                    .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
            })
            .await?;

        reth_to_lighthouse_payload_status(reth_response)
    }

    /// Get a payload by ID for block production
    pub async fn get_payload<E: EthSpec>(
        &self,
        fork_name: ForkName,
        payload_id: PayloadId,
    ) -> Result<GetPayloadResponse<E>, EngineApiError> {
        let engine_capabilities = self.get_engine_capabilities()?;
        let alloy_payload_id = alloy_rpc_types_engine::PayloadId::new(payload_id);
        match fork_name {
            ForkName::Fulu if engine_capabilities.get_payload_v5 => {
                let ExecutionPayloadEnvelopeV5 {
                    execution_payload,
                    block_value,
                    blobs_bundle,
                    should_override_builder,
                    execution_requests,
                } = self
                    .spawn_reth_task(move |reth_handle| async move {
                        reth_handle
                            .get_payload_v5(alloy_payload_id)
                            .await
                            .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
                    })
                    .await?;

                convert_get_payload_v5_response(
                    execution_payload,
                    block_value,
                    blobs_bundle,
                    should_override_builder,
                    execution_requests,
                )?
            }
            ForkName::Electra if engine_capabilities.get_payload_v4 => {
                let envelope_v4 = self
                    .spawn_reth_task(move |reth_handle| async move {
                        reth_handle
                            .get_payload_v4(alloy_payload_id)
                            .await
                            .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
                    })
                    .await?;

                let ExecutionPayloadEnvelopeV4 {
                    envelope_inner: envelope_v3,
                    execution_requests,
                } = envelope_v4;
                let ExecutionPayloadEnvelopeV3 {
                    execution_payload,
                    block_value,
                    blobs_bundle,
                    should_override_builder,
                } = envelope_v3;

                convert_get_payload_v4_response(
                    execution_requests,
                    execution_payload,
                    block_value,
                    blobs_bundle,
                    should_override_builder,
                )?
            }
            _ => Err(Error::UnsupportedForkVariant(format!(
                "called get_payload with {}",
                fork_name
            ))),
        }
    }

    /// Get execution block by hash
    pub async fn get_block_by_hash(
        &self,
        block_hash: ExecutionBlockHash,
    ) -> Result<Option<ExecutionBlock>, EngineApiError> {
        // TODO: Query Reth for block by hash
        debug!(block_hash = ?block_hash, "get_block_by_hash() called - returning None (TODO: implement)");
        Ok(None)
    }

    /// Get execution block by number
    pub async fn get_block_by_number(
        &self,
        query: BlockByNumberQuery<'_>,
    ) -> Result<Option<ExecutionBlock>, EngineApiError> {
        // TODO: Query Reth for block by number
        debug!(query = ?query, "get_block_by_number() called - returning None (TODO: implement)");
        Ok(None)
    }

    /// Get payload bodies by block hashes
    pub async fn get_payload_bodies_by_hash_v1<E: EthSpec>(
        &self,
        block_hashes: Vec<ExecutionBlockHash>,
    ) -> Result<Vec<Option<ExecutionPayloadBodyV1<E>>>, EngineApiError> {
        // TODO: Query Reth for payload bodies
        debug!(
            count = block_hashes.len(),
            "get_payload_bodies_by_hash_v1() called - returning empty vec (TODO: implement)"
        );
        Ok(vec![])
    }

    /// Get payload bodies by block range
    pub async fn get_payload_bodies_by_range_v1<E: EthSpec>(
        &self,
        start: u64,
        count: u64,
    ) -> Result<Vec<Option<ExecutionPayloadBodyV1<E>>>, EngineApiError> {
        // TODO: Query Reth for payload bodies by range
        debug!(
            start = start,
            count = count,
            "get_payload_bodies_by_range_v1() called - returning empty vec (TODO: implement)"
        );
        Ok(vec![])
    }

    /// Get blobs by versioned hashes (v1)
    pub async fn get_blobs_v1<E: EthSpec>(
        &self,
        versioned_hashes: Vec<Hash256>,
    ) -> Result<Vec<Option<BlobAndProofV1<E>>>, EngineApiError> {
        let versioned_hashes = versioned_hashes.into_iter().map(to_alloy_b256).collect();

        let resp = self
            .spawn_reth_task(|reth_handle| async move {
                reth_handle
                    .get_blobs_v1(versioned_hashes)
                    .await
                    .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
            })
            .await?
            .into_iter()
            .map(|blob_and_proof_opt| {
                blob_and_proof_opt
                    .map(|blob_and_proof| {
                        blob_and_proof
                            .blob
                            .0
                            .to_vec()
                            .try_into()
                            .map(|blob| BlobAndProofV1::<E> {
                                blob,
                                proof: blob_and_proof.proof.0.into(),
                            })
                            .map_err(|e| {
                                EngineApiError::BadResponse(format!("Invalid blob: {e:?}"))
                            })
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(resp)
    }

    /// Get blobs by versioned hashes (v2)
    pub async fn get_blobs_v2<E: EthSpec>(
        &self,
        versioned_hashes: Vec<Hash256>,
    ) -> Result<Option<Vec<BlobAndProofV2<E>>>, EngineApiError> {
        let versioned_hashes = versioned_hashes.into_iter().map(to_alloy_b256).collect();

        let resp = self
            .spawn_reth_task(|reth_handle| async move {
                reth_handle
                    .get_blobs_v2(versioned_hashes)
                    .await
                    .map_err(|e| EngineApiError::EngineApiError(format!("{e:?}")))
            })
            .await?
            .map(|blobs_and_proofs| {
                blobs_and_proofs
                    .into_iter()
                    .map(|blob_and_proof| {
                        let blob = blob_and_proof.blob.0.to_vec().try_into().map_err(|e| {
                            EngineApiError::BadResponse(format!("Invalid blob: {e:?}"))
                        })?;
                        let proofs = blob_and_proof
                            .proofs
                            .into_iter()
                            .map(|bytes| bytes.0.into())
                            .collect::<Vec<_>>()
                            .try_into()
                            .map_err(|e| {
                                EngineApiError::BadResponse(format!("Invalid proofs: {e:?}"))
                            })?;
                        Ok(BlobAndProofV2::<E> { blob, proofs })
                    })
                    .collect::<Result<Vec<_>, EngineApiError>>()
            })
            .transpose()?;

        Ok(resp)
    }

    // Spawn the rpc request in a separate task to avoid polluting
    // this future with non-Sync jsonrpsee types
    async fn spawn_reth_task<F, Fut, T>(&self, f: F) -> Result<T, EngineApiError>
    where
        F: FnOnce(RethHandle) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, EngineApiError>> + Send,
        T: Send + 'static,
    {
        let reth_handle = self.reth_handle.clone();
        tokio::task::spawn(async move { f(reth_handle).await })
            .await
            .map_err(EngineApiError::TokioJoin)
            .flatten()
    }
}

impl fmt::Display for RethEngineApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

fn to_alloy_b256(hash: Hash256) -> B256 {
    B256::from_slice(hash.0.as_ref())
}

/// Convert Lighthouse PayloadAttributes → Reth PayloadAttributes
fn convert_lighthouse_to_reth_payload_attrs(
    lh: PayloadAttributes,
) -> alloy_rpc_types_engine::PayloadAttributes {
    use alloy_primitives::{Address as AlloyAddress, B256};
    use alloy_rpc_types_engine::PayloadAttributes as RethPayloadAttributes;

    match lh {
        PayloadAttributes::V1(attrs) => RethPayloadAttributes {
            timestamp: attrs.timestamp,
            prev_randao: B256::from_slice(attrs.prev_randao.as_ref()),
            suggested_fee_recipient: AlloyAddress::from(attrs.suggested_fee_recipient.0),
            withdrawals: None,
            parent_beacon_block_root: None,
        },
        PayloadAttributes::V2(attrs) => RethPayloadAttributes {
            timestamp: attrs.timestamp,
            prev_randao: B256::from_slice(attrs.prev_randao.as_ref()),
            suggested_fee_recipient: AlloyAddress::from(attrs.suggested_fee_recipient.0),
            withdrawals: Some(
                attrs
                    .withdrawals
                    .into_iter()
                    .map(convert_withdrawal)
                    .collect(),
            ),
            parent_beacon_block_root: None,
        },
        PayloadAttributes::V3(attrs) => RethPayloadAttributes {
            timestamp: attrs.timestamp,
            prev_randao: B256::from_slice(attrs.prev_randao.as_ref()),
            suggested_fee_recipient: AlloyAddress::from(attrs.suggested_fee_recipient.0),
            withdrawals: Some(
                attrs
                    .withdrawals
                    .into_iter()
                    .map(convert_withdrawal)
                    .collect(),
            ),
            parent_beacon_block_root: Some(B256::from_slice(
                attrs.parent_beacon_block_root.as_ref(),
            )),
        },
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
#[instrument(level = "debug", skip_all)]
fn convert_lighthouse_to_reth_payload<E: EthSpec>(
    request: NewPayloadRequest<'_, E>,
) -> Result<ExecutionData, String> {
    use alloy_primitives::{Address as AlloyAddress, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};

    let res = match request {
        NewPayloadRequest::Electra(payload_request) => {
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
                        base_fee_per_gas: U256::from_be_bytes::<32>(
                            payload.base_fee_per_gas.to_be_bytes::<32>(),
                        ),
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
                        .cloned()
                        .map(convert_withdrawal)
                        .collect(),
                },
                blob_gas_used: payload.blob_gas_used,
                excess_blob_gas: payload.excess_blob_gas,
            };

            let parent_beacon_block_root =
                B256::from_slice(payload_request.parent_beacon_block_root.as_ref());
            let versioned_hashes = payload_request
                .versioned_hashes
                .iter()
                .map(|hash| B256::from_slice(hash.as_ref()))
                .collect();
            let requests = payload_request
                .execution_requests
                .get_execution_requests_list();
            let execution_payload_sidecar = ExecutionPayloadSidecar::v4(
                CancunPayloadFields::new(parent_beacon_block_root, versioned_hashes),
                PraguePayloadFields::new(Requests::new(requests)),
            );

            Ok((
                alloy_rpc_types_engine::ExecutionPayload::V3(alloy_payload),
                execution_payload_sidecar,
            ))
        }
        NewPayloadRequest::Fulu(payload_request) => {
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
                        base_fee_per_gas: U256::from_be_bytes::<32>(
                            payload.base_fee_per_gas.to_be_bytes::<32>(),
                        ),
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
                        .cloned()
                        .map(convert_withdrawal)
                        .collect(),
                },
                blob_gas_used: payload.blob_gas_used,
                excess_blob_gas: payload.excess_blob_gas,
            };

            let parent_beacon_block_root =
                B256::from_slice(payload_request.parent_beacon_block_root.as_ref());
            let versioned_hashes = payload_request
                .versioned_hashes
                .iter()
                .map(|hash| B256::from_slice(hash.as_ref()))
                .collect();
            let requests = payload_request
                .execution_requests
                .get_execution_requests_list();
            let execution_payload_sidecar = ExecutionPayloadSidecar::v4(
                CancunPayloadFields::new(parent_beacon_block_root, versioned_hashes),
                PraguePayloadFields::new(Requests::new(requests)),
            );

            Ok((
                alloy_rpc_types_engine::ExecutionPayload::V3(alloy_payload),
                execution_payload_sidecar,
            ))
        }
        NewPayloadRequest::Bellatrix(_)
        | NewPayloadRequest::Capella(_)
        | NewPayloadRequest::Deneb(_)
        | NewPayloadRequest::Gloas(_) => Err("Fork not yet implemented".to_string()),
    };

    res.map(|(payload, sidecar)| ExecutionData::new(payload, sidecar))
}

/// Convert Alloy PayloadStatus → Lighthouse PayloadStatusV1
fn reth_to_lighthouse_payload_status(
    alloy_status: alloy_rpc_types_engine::PayloadStatus,
) -> Result<crate::engine_api::PayloadStatusV1, EngineApiError> {
    use crate::engine_api::{PayloadStatusV1, PayloadStatusV1Status};
    use alloy_rpc_types_engine::PayloadStatusEnum;

    Ok(PayloadStatusV1 {
        status: match alloy_status.status {
            PayloadStatusEnum::Valid => PayloadStatusV1Status::Valid,
            PayloadStatusEnum::Invalid {
                validation_error: _,
            } => PayloadStatusV1Status::Invalid,
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
fn reth_to_lighthouse_forkchoice_response(
    reth: alloy_rpc_types_engine::ForkchoiceUpdated,
) -> Result<ForkchoiceUpdatedResponse, EngineApiError> {
    use crate::engine_api::{PayloadStatusV1, PayloadStatusV1Status};
    use alloy_rpc_types_engine::PayloadStatusEnum;

    let payload_status = PayloadStatusV1 {
        status: match reth.payload_status.status {
            PayloadStatusEnum::Valid => PayloadStatusV1Status::Valid,
            PayloadStatusEnum::Invalid {
                validation_error: _,
            } => PayloadStatusV1Status::Invalid,
            PayloadStatusEnum::Syncing => PayloadStatusV1Status::Syncing,
            PayloadStatusEnum::Accepted => PayloadStatusV1Status::Accepted,
        },
        latest_valid_hash: reth
            .payload_status
            .latest_valid_hash
            .map(|h| ExecutionBlockHash::from(Hash256::from_slice(h.as_slice()))),
        validation_error: match reth.payload_status.status {
            PayloadStatusEnum::Invalid { validation_error } => Some(validation_error),
            _ => None,
        },
    };

    Ok(ForkchoiceUpdatedResponse {
        payload_status,
        payload_id: reth.payload_id.map(|id| id.0.into()),
    })
}

#[instrument(level = "debug", skip_all)]
fn convert_get_payload_v4_response<E: EthSpec>(
    execution_requests: Requests,
    execution_payload: ExecutionPayloadV3,
    block_value: U256,
    blobs_bundle: BlobsBundleV1,
    should_override_builder: bool,
) -> Result<Result<GetPayloadResponse<E>, Error>, Error> {
    let execution_payload = reth_to_lighthouse_execution_payload_electra(execution_payload)?;
    let requests = reth_lighthouse_execution_requests(execution_requests.to_vec())?;
    let blobs_bundle = reth_to_lighthouse_blobs_bundle_v1(blobs_bundle)?;

    Ok(Ok(GetPayloadResponse::Electra(GetPayloadResponseElectra {
        execution_payload,
        block_value,
        blobs_bundle,
        should_override_builder,
        requests,
    })))
}

#[instrument(level = "debug", skip_all)]
fn convert_get_payload_v5_response<E: EthSpec>(
    execution_payload: ExecutionPayloadV3,
    block_value: U256,
    blobs_bundle: BlobsBundleV2,
    should_override_builder: bool,
    execution_requests: Requests,
) -> Result<Result<GetPayloadResponse<E>, Error>, Error> {
    let execution_payload = reth_to_lighthouse_execution_payload_fulu(execution_payload)?;
    let requests = reth_lighthouse_execution_requests(execution_requests.to_vec())?;
    let blobs_bundle = reth_to_lighthouse_blobs_bundle_v2(blobs_bundle)?;

    Ok(Ok(GetPayloadResponse::Fulu(GetPayloadResponseFulu {
        execution_payload,
        block_value,
        blobs_bundle,
        should_override_builder,
        requests,
    })))
}

fn reth_to_lighthouse_execution_payload_fulu<E: EthSpec>(
    reth_execution_payload_v3: ExecutionPayloadV3,
) -> Result<ExecutionPayloadFulu<E>, EngineApiError> {
    let inner = &reth_execution_payload_v3.payload_inner.payload_inner;

    Ok(ExecutionPayloadFulu {
        parent_hash: Hash256::from_slice(inner.parent_hash.as_slice()).into(),
        fee_recipient: inner.fee_recipient,
        state_root: inner.state_root,
        receipts_root: inner.receipts_root,
        logs_bloom: FixedVector::new(inner.logs_bloom.as_slice().to_vec())
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid logs_bloom: {e:?}")))?,
        prev_randao: inner.prev_randao,
        block_number: inner.block_number,
        gas_limit: inner.gas_limit,
        gas_used: inner.gas_used,
        timestamp: inner.timestamp,
        extra_data: VariableList::new(inner.extra_data.to_vec())
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid extra_data: {e:?}")))?,
        base_fee_per_gas: inner.base_fee_per_gas,
        block_hash: Hash256::from_slice(inner.block_hash.as_slice()).into(),
        transactions: VariableList::new(
            inner
                .transactions
                .iter()
                .map(|tx| VariableList::new(tx.to_vec()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| EngineApiError::BadResponse(format!("Invalid transaction: {e:?}")))?,
        )
        .map_err(|e| EngineApiError::BadResponse(format!("Invalid transactions: {e:?}")))?,
        withdrawals: VariableList::new(
            reth_execution_payload_v3
                .payload_inner
                .withdrawals
                .iter()
                .map(|w| types::Withdrawal {
                    index: w.index,
                    validator_index: w.validator_index,
                    address: w.address,
                    amount: w.amount,
                })
                .collect(),
        )
        .map_err(|e| EngineApiError::BadResponse(format!("Invalid withdrawals: {e:?}")))?,
        blob_gas_used: reth_execution_payload_v3.blob_gas_used,
        excess_blob_gas: reth_execution_payload_v3.excess_blob_gas,
    })
}

fn reth_to_lighthouse_execution_payload_electra<E: EthSpec>(
    reth_execution_payload_v3: ExecutionPayloadV3,
) -> Result<ExecutionPayloadElectra<E>, EngineApiError> {
    let inner = &reth_execution_payload_v3.payload_inner.payload_inner;

    Ok(ExecutionPayloadElectra {
        parent_hash: Hash256::from_slice(inner.parent_hash.as_slice()).into(),
        fee_recipient: inner.fee_recipient,
        state_root: inner.state_root,
        receipts_root: inner.receipts_root,
        logs_bloom: FixedVector::new(inner.logs_bloom.as_slice().to_vec())
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid logs_bloom: {e:?}")))?,
        prev_randao: inner.prev_randao,
        block_number: inner.block_number,
        gas_limit: inner.gas_limit,
        gas_used: inner.gas_used,
        timestamp: inner.timestamp,
        extra_data: VariableList::new(inner.extra_data.to_vec())
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid extra_data: {e:?}")))?,
        base_fee_per_gas: inner.base_fee_per_gas,
        block_hash: Hash256::from_slice(inner.block_hash.as_slice()).into(),
        transactions: VariableList::new(
            inner
                .transactions
                .iter()
                .map(|tx| VariableList::new(tx.to_vec()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| EngineApiError::BadResponse(format!("Invalid transaction: {e:?}")))?,
        )
        .map_err(|e| EngineApiError::BadResponse(format!("Invalid transactions: {e:?}")))?,
        withdrawals: VariableList::new(
            reth_execution_payload_v3
                .payload_inner
                .withdrawals
                .iter()
                .map(|w| types::Withdrawal {
                    index: w.index,
                    validator_index: w.validator_index,
                    address: w.address,
                    amount: w.amount,
                })
                .collect(),
        )
        .map_err(|e| EngineApiError::BadResponse(format!("Invalid withdrawals: {e:?}")))?,
        blob_gas_used: reth_execution_payload_v3.blob_gas_used,
        excess_blob_gas: reth_execution_payload_v3.excess_blob_gas,
    })
}

fn reth_to_lighthouse_blobs_bundle_v1<E: EthSpec>(
    reth_blobs_bundle: alloy_rpc_types_engine::BlobsBundleV1,
) -> Result<BlobsBundle<E>, EngineApiError> {
    let alloy_rpc_types_engine::BlobsBundleV1 {
        commitments,
        proofs,
        blobs,
    } = reth_blobs_bundle;

    Ok(BlobsBundle {
        commitments: commitments
            .into_iter()
            .map(|v| KzgCommitment(v.0))
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid commitments: {e:?}")))?,
        proofs: proofs
            .into_iter()
            .map(|v| KzgProof(v.0))
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid proofs: {e:?}")))?,
        blobs: blobs
            .into_iter()
            .map(|v| {
                Vec::from(v.0)
                    .try_into()
                    .map_err(|e| EngineApiError::BadResponse(format!("Invalid blob: {e:?}")))
            })
            .collect::<Result<Vec<Blob<E>>, _>>()?
            .try_into()
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid blobs: {e:?}")))?,
    })
}

fn reth_to_lighthouse_blobs_bundle_v2<E: EthSpec>(
    reth_blobs_bundle: alloy_rpc_types_engine::BlobsBundleV2,
) -> Result<BlobsBundle<E>, EngineApiError> {
    let alloy_rpc_types_engine::BlobsBundleV2 {
        commitments,
        proofs,
        blobs,
    } = reth_blobs_bundle;

    Ok(BlobsBundle {
        commitments: commitments
            .into_iter()
            .map(|v| KzgCommitment(v.0))
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid commitments: {e:?}")))?,
        proofs: proofs
            .into_iter()
            .map(|v| KzgProof(v.0))
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid proofs: {e:?}")))?,
        blobs: blobs
            .into_iter()
            .map(|v| {
                Vec::from(v.0)
                    .try_into()
                    .map_err(|e| EngineApiError::BadResponse(format!("Invalid blob: {e:?}")))
            })
            .collect::<Result<Vec<Blob<E>>, _>>()?
            .try_into()
            .map_err(|e| EngineApiError::BadResponse(format!("Invalid blobs: {e:?}")))?,
    })
}

/// Convert Alloy execution requests directly to Lighthouse ExecutionRequests
/// This is much faster than converting to hex strings and back
fn reth_lighthouse_execution_requests<E: EthSpec>(
    alloy_requests: Vec<AlloyBytes>,
) -> Result<ExecutionRequests<E>, EngineApiError> {
    let mut requests = ExecutionRequests::default();
    let mut prev_prefix: Option<RequestType> = None;

    for (i, request) in alloy_requests.into_iter().enumerate() {
        let request_bytes = request.as_ref();

        // The first byte is the request_type, remaining bytes are request_data
        let Some((prefix_byte, request_data)) = request_bytes.split_first() else {
            return Err(EngineApiError::BadResponse(format!(
                "Empty request at index {i}"
            )));
        };

        if request_data.is_empty() {
            return Err(EngineApiError::BadResponse(format!(
                "Empty request data at index {i}"
            )));
        }

        // Validate ordering (must be ascending)
        let current_prefix = RequestType::from_u8(*prefix_byte).ok_or_else(|| {
            EngineApiError::BadResponse(format!("Invalid request prefix: {prefix_byte}"))
        })?;

        if let Some(prev) = prev_prefix
            && prev.to_u8() >= current_prefix.to_u8()
        {
            return Err(EngineApiError::BadResponse(
                "Requests not in ascending order".to_string(),
            ));
        }
        prev_prefix = Some(current_prefix);

        // Decode SSZ based on request type
        match current_prefix {
            RequestType::Deposit => {
                requests.deposits = DepositRequests::<E>::from_ssz_bytes(request_data)
                    .map_err(|e| EngineApiError::BadResponse(format!("Invalid deposits: {e:?}")))?;
            }
            RequestType::Withdrawal => {
                requests.withdrawals = WithdrawalRequests::<E>::from_ssz_bytes(request_data)
                    .map_err(|e| {
                        EngineApiError::BadResponse(format!("Invalid withdrawals: {e:?}"))
                    })?;
            }
            RequestType::Consolidation => {
                requests.consolidations = ConsolidationRequests::<E>::from_ssz_bytes(request_data)
                    .map_err(|e| {
                        EngineApiError::BadResponse(format!("Invalid consolidations: {e:?}"))
                    })?;
            }
        }
    }

    Ok(requests)
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
    pub chain_spec: Arc<reth_chainspec::ChainSpec>,
    /// Path to JWT secret file for Engine API authentication
    pub jwt_path: std::path::PathBuf,
}

/// Launch Reth and return the Reth Engine API handle
///
/// This initializes Reth's full node with a persistent database and returns
/// the handle we can use to send Engine API messages to it.
///
/// The function accepts a RethConfig to specify:
/// - Data directory path for persistent storage
/// - Chain spec (mainnet, sepolia, holesky, gnosis, etc.)
/// - JWT secret path
fn launch_reth_and_get_handle_with_config(
    config: RethConfig,
    executor: TaskExecutor,
) -> Result<RethHandle, String> {
    use reth_db::{ClientVersion, init_db, mdbx::DatabaseArguments};
    use reth_ethereum::{
        node::{builder::NodeBuilder, core::node_config::NodeConfig, node::EthereumNode},
        tasks::TaskManager,
    };
    use std::sync::Arc;

    // Enable Reth logging - set environment variable if not already set
    // This allows us to see Reth's internal logs for debugging
    // Run Lighthouse with: RUST_LOG=reth=debug,lighthouse=info lighthouse beacon_node
    if std::env::var("RUST_LOG").is_err() {
        // SAFETY: Setting RUST_LOG before any logging initialization is safe
        // as we're the only thread accessing it at this point during startup
        unsafe {
            std::env::set_var("RUST_LOG", "reth=debug,reth_db=debug,reth_node=info,info");
        }
    }

    warn!("Reth logging enabled via RUST_LOG environment variable");

    info!(
        datadir = %config.datadir.display(),
        chain = %config.chain_spec.chain.to_string(),
        "Launching Reth execution engine in-process"
    );

    // Create data directory if it doesn't exist
    std::fs::create_dir_all(&config.datadir)
        .map_err(|e| format!("Failed to create data directory: {}", e))?;
    let db_path = config.datadir.join("db");
    let sf_path = config.datadir.join("static_files");

    let db = Arc::new(
        init_db(&db_path, DatabaseArguments::new(ClientVersion::default()))
            .map_err(|e| format!("Failed to initialize database: {}", e))?,
    );

    let node_config = NodeConfig::new(config.chain_spec)
        .with_datadir_args(DatadirArgs {
            datadir: config.datadir.clone().into(),
            static_files_path: Some(sf_path),
        })
        .with_rpc(reth_node_core::args::RpcServerArgs {
            auth_jwtsecret: Some(config.jwt_path.clone()),
            ..<_>::default()
        });

    // Channel to extract the EngineApi or error
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let error_tx = handle_tx.clone();

    info!("Spawning Reth launch task on executor");

    // Spawn a thread that queues the async task on executor and exits immediately
    // This avoids deadlock:
    // - This thread exits quickly after queuing the task
    // - Main thread can block on recv_timeout
    // - Executor's worker threads run the spawned async task
    let _join_handle = std::thread::spawn(move || {
        executor.spawn_without_exit(
            async move {
                debug!("Launching Reth node");

                let engine_api = EngineApiExt::new(
                    BasicEngineApiBuilder::<EthereumEngineValidatorBuilder>::default(),
                    move |api| {
                        info!("Reth node started, extracting engine api handle");
                        let _ = handle_tx.send(Ok(api));
                        debug!("Extracted engine api handle");
                    },
                );

                let tasks = TaskManager::current();
                let node_builder = NodeBuilder::new(node_config)
                    .with_database(db)
                    .with_launch_context(tasks.executor())
                    .with_types::<EthereumNode>()
                    .with_components(EthereumNode::components())
                    .with_add_ons(EthereumAddOns::default().with_engine_api(engine_api));

                match node_builder.launch().await {
                    Ok(handle) => {
                        info!("Reth execution engine launched successfully");
                        // Keep node running infinitely (beacon chain will shut us down)
                        let _ = handle.wait_for_node_exit().await;
                        info!("Reth node exited");
                    }
                    Err(e) => {
                        error!("Reth launch failed: {}", e);
                        // Send error through channel so we don't timeout
                        let _ = error_tx.send(Err(format!("Reth launch failed: {}", e)));
                    }
                }
            },
            "launch_reth",
        );
    });

    info!("Waiting for Reth to initialize (30s timeout)");

    handle_rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|e| format!("Timeout waiting for Reth to launch: {}", e))?
        .map_err(|e| format!("Reth launch error: {}", e))
}
