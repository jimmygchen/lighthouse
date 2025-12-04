use crate::BlockId;
use crate::task_spawner::{Priority, TaskSpawner};
use beacon_chain::{BeaconChain, BeaconChainTypes, WhenSlotSkipped};
use eth2::types::{
    BlockHeaderAndSignature, BlockHeaderData, ExecutionOptimisticFinalizedResponse,
    GenericResponse, HeadersQuery,
};
use std::sync::Arc;
use warp::reply::Response;

pub fn get_beacon_headers<T: BeaconChainTypes>(
    query: HeadersQuery,
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
) -> impl Future<Output = Response> {
    task_spawner.blocking_json_task(Priority::P1, move || {
        let (root, block, execution_optimistic, finalized) = match (query.slot, query.parent_root) {
            // No query parameters, return the canonical head block.
            (None, None) => {
                let (cached_head, execution_status) = chain
                    .canonical_head
                    .head_and_execution_status()
                    .map_err(warp_utils::reject::unhandled_error)?;
                (
                    cached_head.head_block_root(),
                    cached_head.snapshot.beacon_block.clone_as_blinded(),
                    execution_status.is_optimistic_or_invalid(),
                    false,
                )
            }
            // Only the parent root parameter, do a forwards-iterator lookup.
            (None, Some(parent_root)) => {
                let (parent, execution_optimistic, _parent_finalized) =
                    BlockId::from_root(parent_root).blinded_block(&chain)?;
                let (root, _slot) = chain
                    .forwards_iter_block_roots(parent.slot())
                    .map_err(warp_utils::reject::unhandled_error)?
                    // Ignore any skip-slots immediately following the parent.
                    .find(|res| res.as_ref().is_ok_and(|(root, _)| *root != parent_root))
                    .transpose()
                    .map_err(warp_utils::reject::unhandled_error)?
                    .ok_or_else(|| {
                        warp_utils::reject::custom_not_found(format!(
                            "child of block with root {}",
                            parent_root
                        ))
                    })?;

                BlockId::from_root(root)
                    .blinded_block(&chain)
                    // Ignore this `execution_optimistic` since the first value has
                    // more information about the original request.
                    .map(|(block, _execution_optimistic, finalized)| {
                        (root, block, execution_optimistic, finalized)
                    })?
            }
            // Slot is supplied, search by slot and optionally filter by
            // parent root.
            (Some(slot), parent_root_opt) => {
                let (root, execution_optimistic, finalized) =
                    BlockId::from_slot(slot).root(&chain)?;
                // Ignore the second `execution_optimistic`, the first one is the
                // most relevant since it knows that we queried by slot.
                let (block, _execution_optimistic, _finalized) =
                    BlockId::from_root(root).blinded_block(&chain)?;

                // If the parent root was supplied, check that it matches the block
                // obtained via a slot lookup.
                if let Some(parent_root) = parent_root_opt
                    && block.parent_root() != parent_root
                {
                    return Err(warp_utils::reject::custom_not_found(format!(
                        "no canonical block at slot {} with parent root {}",
                        slot, parent_root
                    )));
                }

                (root, block, execution_optimistic, finalized)
            }
        };

        let data = BlockHeaderData {
            root,
            canonical: true,
            header: BlockHeaderAndSignature {
                message: block.message().block_header(),
                signature: block.signature().clone().into(),
            },
        };

        Ok(GenericResponse::from(vec![data])
            .add_execution_optimistic_finalized(execution_optimistic, finalized))
    })
}

pub fn get_beacon_headers_block_id<T: BeaconChainTypes>(
    block_id: BlockId,
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
) -> impl Future<Output = Response> {
    task_spawner.blocking_json_task(Priority::P1, move || {
        let (root, execution_optimistic, finalized) = block_id.root(&chain)?;
        // Ignore the second `execution_optimistic` since the first one has more
        // information about the original request.
        let (block, _execution_optimistic, _finalized) =
            BlockId::from_root(root).blinded_block(&chain)?;

        let canonical = chain
            .block_root_at_slot(block.slot(), WhenSlotSkipped::None)
            .map_err(warp_utils::reject::unhandled_error)?
            .is_some_and(|canonical| root == canonical);

        let data = BlockHeaderData {
            root,
            canonical,
            header: BlockHeaderAndSignature {
                message: block.message().block_header(),
                signature: block.signature().clone().into(),
            },
        };

        Ok(ExecutionOptimisticFinalizedResponse {
            execution_optimistic: Some(execution_optimistic),
            finalized: Some(finalized),
            data,
        })
    })
}
