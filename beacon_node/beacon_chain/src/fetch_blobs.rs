use crate::{BeaconChain, BeaconChainError, BeaconChainTypes, BlockError, ExecutionPayloadError};
use slog::{debug, error, warn};
use state_processing::per_block_processing::deneb::kzg_commitment_to_versioned_hash;
use std::sync::Arc;
use types::blob_sidecar::FixedBlobSidecarList;
use types::{
    BlobSidecar, DataColumnSidecar, DataColumnSidecarVec, EthSpec, FullPayload, Hash256,
    SignedBeaconBlock,
};

pub enum BlobsOrDataColumns<E: EthSpec> {
    Blobs(Vec<Arc<BlobSidecar<E>>>),
    DataColumns(DataColumnSidecarVec<E>),
}

pub async fn fetch_blobs_and_publish<T: BeaconChainTypes>(
    chain: Arc<BeaconChain<T>>,
    block_root: Hash256,
    block: Arc<SignedBeaconBlock<T::EthSpec, FullPayload<T::EthSpec>>>,
    publish_fn: impl FnOnce(BlobsOrDataColumns<T::EthSpec>) + Send + 'static,
) -> Result<(), BlockError<T::EthSpec>> {
    let versioned_hashes =
        if let Ok(kzg_commitments) = block.message().body().blob_kzg_commitments() {
            kzg_commitments
                .iter()
                .map(kzg_commitment_to_versioned_hash)
                .collect()
        } else {
            vec![]
        };
    let num_blobs = versioned_hashes.len();

    if versioned_hashes.is_empty() {
        debug!(chain.log, "Blobs from EL - none required");
        return Ok(());
    }

    let execution_layer = chain
        .execution_layer
        .as_ref()
        .ok_or(BeaconChainError::ExecutionLayerMissing)?;

    debug!(
        chain.log,
        "Blobs from EL - start request";
        "num_blobs" => num_blobs,
    );
    let response = execution_layer
        .get_blobs(versioned_hashes)
        .await
        .map_err(|e| BlockError::ExecutionPayloadError(ExecutionPayloadError::RequestFailed(e)))?;
    let num_fetched_blobs = response.iter().filter(|b| b.is_some()).count();
    let mut all_blobs_fetched = false;
    if num_fetched_blobs == 0 {
        debug!(chain.log, "Blobs from EL - response with none");
        return Ok(());
    } else if num_fetched_blobs < num_blobs {
        // TODO(das) partial blobs response isn't useful for PeerDAS, do we even try to process them?
        debug!(
            chain.log,
            "Blobs from EL - response with some";
            "fetched" => num_fetched_blobs,
            "total" => num_blobs,
        );
    } else {
        all_blobs_fetched = true;
        debug!(
            chain.log,
            "Blobs from EL - response with all";
            "num_blobs" => num_blobs
        );
    }

    let (signed_block_header, kzg_commitments_proof) =
        block.signed_block_header_and_kzg_commitments_proof()?;

    let mut fixed_blob_sidecar_list = FixedBlobSidecarList::default();
    for (i, blob_and_proof) in response
        .into_iter()
        .enumerate()
        .filter_map(|(i, opt_blob)| Some((i, opt_blob?)))
    {
        match BlobSidecar::new_efficiently(
            i,
            blob_and_proof.blob,
            &block,
            signed_block_header.clone(),
            &kzg_commitments_proof,
            blob_and_proof.proof,
        ) {
            Ok(blob) => {
                if let Some(blob_mut) = fixed_blob_sidecar_list.get_mut(i) {
                    *blob_mut = Some(Arc::new(blob));
                } else {
                    error!(
                        chain.log,
                        "Blobs from EL - out of bounds";
                        "i" => i
                    );
                }
            }
            Err(e) => {
                warn!(
                    chain.log,
                    "Blobs from EL - error";
                    "error" => ?e
                );
            }
        }
    }

    // Spawn an async task here for long computation tasks, so it doesn't block processing, and it
    // allows blobs / data columns to propagate without waiting for processing.
    //
    // An `mpsc::Sender` is then used to send the produced data columns to the `beacon_chain` for it
    // to be persisted, **after** the block is made attestable.
    //
    // The reason for doing this is to make the block available and attestable as soon as possible,
    // while maintaining the invariant that block and data columns are persisted atomically.
    let (data_columns_sender, data_columns_receiver) = tokio::sync::mpsc::channel(1);
    let peer_das_enabled = chain.spec.is_peer_das_enabled_for_epoch(block.epoch());

    if peer_das_enabled && all_blobs_fetched {
        let logger = chain.log.clone();
        let block_cloned = block.clone();
        let kzg = chain.kzg.clone().expect("KZG not initialized");
        let spec = chain.spec.clone();
        // TODO(das) is it possible to avoid the blob clone?
        let blobs = fixed_blob_sidecar_list
            .iter()
            .filter_map(|b| b.clone().map(|b| b.blob.clone()))
            .collect::<Vec<_>>()
            .into();
        let is_supernode =
            chain.data_availability_checker.get_custody_columns_count() == spec.number_of_columns;

        chain
            .task_executor
            .spawn_handle(
                async move {
                    let data_columns_result = DataColumnSidecar::build_sidecars(
                        &blobs,
                        &block_cloned,
                        &kzg,
                        &spec,
                    );

                    let data_columns = match data_columns_result {
                        Ok(d) => d,
                        Err(e) => {
                            error!(logger, "Failed to build data column sidecars from blobs"; "error" => ?e);
                            return;
                        }
                    };

                    if let Err(e) = data_columns_sender.try_send(data_columns.clone()) {
                        error!(logger, "Failed to send computed data columns"; "error" => ?e);
                    };

                    if is_supernode {
                        publish_fn(BlobsOrDataColumns::DataColumns(data_columns));
                    }
                },
                "compute_data_columns",
            )
            .ok_or(BeaconChainError::RuntimeShutdown)?;
    } else {
        let blobs = fixed_blob_sidecar_list
            .clone()
            .into_iter()
            .flat_map(|b| b.clone())
            .collect::<Vec<_>>();
        publish_fn(BlobsOrDataColumns::Blobs(blobs));
    };

    debug!(
        chain.log,
        "Blobs from EL - start processing";
        "num_blobs" => num_blobs,
    );

    chain
        .process_engine_blobs(
            block.slot(),
            block_root,
            fixed_blob_sidecar_list.clone(),
            Some(data_columns_receiver),
        )
        .await
        .map(|_| debug!(chain.log, "Blobs from EL - processed"))
        .map_err(|e| {
            warn!(chain.log, "Blobs from EL - error"; "error" => ?e);
            e
        })?;

    Ok(())
}
