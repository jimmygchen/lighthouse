use crate::StateId;
use crate::task_spawner::{Priority, TaskSpawner};
use crate::validator::pubkey_to_validator_index;
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::{
    ExecutionOptimisticFinalizedResponse, ValidatorData, ValidatorId, ValidatorStatus,
};
use std::sync::Arc;
use warp::reply::Response;

pub fn get_beacon_state_validators_id<T: BeaconChainTypes>(
    state_id: StateId,
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
    validator_id: ValidatorId,
) -> impl Future<Output = Response> {
    // Prioritise requests for validators at the head. These should be fast to service
    // and could be required by the validator client.
    let priority = if let StateId(eth2::types::StateId::Head) = state_id {
        Priority::P0
    } else {
        Priority::P1
    };
    task_spawner.blocking_json_task(priority, move || {
        let (data, execution_optimistic, finalized) = state_id
            .map_state_and_execution_optimistic_and_finalized(
                &chain,
                |state, execution_optimistic, finalized| {
                    let index_opt = match &validator_id {
                        ValidatorId::PublicKey(pubkey) => {
                            pubkey_to_validator_index(&chain, state, pubkey).map_err(|e| {
                                warp_utils::reject::custom_not_found(format!(
                                    "unable to access pubkey cache: {e:?}",
                                ))
                            })?
                        }
                        ValidatorId::Index(index) => Some(*index as usize),
                    };

                    Ok((
                        index_opt
                            .and_then(|index| {
                                let validator = state.validators().get(index)?;
                                let balance = *state.balances().get(index)?;
                                let epoch = state.current_epoch();
                                let far_future_epoch = chain.spec.far_future_epoch;

                                Some(ValidatorData {
                                    index: index as u64,
                                    balance,
                                    status: ValidatorStatus::from_validator(
                                        validator,
                                        epoch,
                                        far_future_epoch,
                                    ),
                                    validator: validator.clone(),
                                })
                            })
                            .ok_or_else(|| {
                                warp_utils::reject::custom_not_found(format!(
                                    "unknown validator: {}",
                                    validator_id
                                ))
                            })?,
                        execution_optimistic,
                        finalized,
                    ))
                },
            )?;

        Ok(ExecutionOptimisticFinalizedResponse {
            data,
            execution_optimistic: Some(execution_optimistic),
            finalized: Some(finalized),
        })
    })
}
