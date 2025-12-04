use crate::StateId;
use crate::task_spawner::{Priority, TaskSpawner};
use crate::version::{
    ResponseIncludesVersion, add_consensus_version_header, add_ssz_content_type_header,
    execution_optimistic_finalized_beacon_response, inconsistent_fork_rejection,
};
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::{Accept, EndpointVersion, ForkChoice, ForkChoiceExtraData, ForkChoiceNode};
use ssz::Encode;
use std::sync::Arc;
use tracing::debug;
use warp::Reply;
use warp::hyper::Body;
use warp::reply::Response;

pub fn get_debug_fork_choice<T: BeaconChainTypes>(
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
) -> impl Future<Output = Response> {
    task_spawner.blocking_json_task(Priority::P1, move || {
        let beacon_fork_choice = chain.canonical_head.fork_choice_read_lock();

        let proto_array = beacon_fork_choice.proto_array().core_proto_array();

        let fork_choice_nodes = proto_array
            .nodes
            .iter()
            .map(|node| {
                let execution_status = if node.execution_status.is_execution_enabled() {
                    Some(node.execution_status.to_string())
                } else {
                    None
                };

                ForkChoiceNode {
                    slot: node.slot,
                    block_root: node.root,
                    parent_root: node
                        .parent
                        .and_then(|index| proto_array.nodes.get(index))
                        .map(|parent| parent.root),
                    justified_epoch: node.justified_checkpoint.epoch,
                    finalized_epoch: node.finalized_checkpoint.epoch,
                    weight: node.weight,
                    validity: execution_status,
                    execution_block_hash: node
                        .execution_status
                        .block_hash()
                        .map(|block_hash| block_hash.into_root()),
                    extra_data: ForkChoiceExtraData {
                        target_root: node.target_root,
                        justified_root: node.justified_checkpoint.root,
                        finalized_root: node.finalized_checkpoint.root,
                        unrealized_justified_root: node
                            .unrealized_justified_checkpoint
                            .map(|checkpoint| checkpoint.root),
                        unrealized_finalized_root: node
                            .unrealized_finalized_checkpoint
                            .map(|checkpoint| checkpoint.root),
                        unrealized_justified_epoch: node
                            .unrealized_justified_checkpoint
                            .map(|checkpoint| checkpoint.epoch),
                        unrealized_finalized_epoch: node
                            .unrealized_finalized_checkpoint
                            .map(|checkpoint| checkpoint.epoch),
                        execution_status: node.execution_status.to_string(),
                        best_child: node
                            .best_child
                            .and_then(|index| proto_array.nodes.get(index))
                            .map(|child| child.root),
                        best_descendant: node
                            .best_descendant
                            .and_then(|index| proto_array.nodes.get(index))
                            .map(|descendant| descendant.root),
                    },
                }
            })
            .collect::<Vec<_>>();
        Ok(ForkChoice {
            justified_checkpoint: beacon_fork_choice.justified_checkpoint(),
            finalized_checkpoint: beacon_fork_choice.finalized_checkpoint(),
            fork_choice_nodes,
        })
    })
}

pub fn get_debug_beacon_states<T: BeaconChainTypes>(
    _endpoint_version: EndpointVersion,
    state_id: StateId,
    accept_header: Option<Accept>,
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
) -> impl Future<Output = Response> {
    task_spawner.blocking_response_task(Priority::P1, move || match accept_header {
        Some(Accept::Ssz) => {
            // We can ignore the optimistic status for the "fork" since it's a
            // specification constant that doesn't change across competing heads of the
            // beacon chain.
            let t = std::time::Instant::now();
            let (state, _execution_optimistic, _finalized) = state_id.state(&chain)?;
            let fork_name = state
                .fork_name(&chain.spec)
                .map_err(inconsistent_fork_rejection)?;
            let timer =
                crate::metrics::start_timer(&crate::metrics::HTTP_API_STATE_SSZ_ENCODE_TIMES);
            let response_bytes = state.as_ssz_bytes();
            drop(timer);
            debug!(
                total_time_ms = t.elapsed().as_millis(),
                target_slot = %state.slot(),
                "HTTP state load"
            );

            warp::http::Response::builder()
                .status(200)
                .body(response_bytes.into())
                .map(|res: warp::http::Response<Body>| add_ssz_content_type_header(res))
                .map(|resp: warp::reply::Response| add_consensus_version_header(resp, fork_name))
                .map_err(|e| {
                    warp_utils::reject::custom_server_error(format!(
                        "failed to create response: {}",
                        e
                    ))
                })
        }
        _ => state_id.map_state_and_execution_optimistic_and_finalized(
            &chain,
            |state, execution_optimistic, finalized| {
                let fork_name = state
                    .fork_name(&chain.spec)
                    .map_err(inconsistent_fork_rejection)?;
                let res = execution_optimistic_finalized_beacon_response(
                    ResponseIncludesVersion::Yes(fork_name),
                    execution_optimistic,
                    finalized,
                    &state,
                )?;
                Ok(add_consensus_version_header(
                    warp::reply::json(&res).into_response(),
                    fork_name,
                ))
            },
        ),
    })
}
