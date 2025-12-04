use crate::StateId;
use crate::task_spawner::{Priority, TaskSpawner};
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::{
    GenericResponse, SyncCommitteeByValidatorIndices, SyncCommitteesQuery, SyncSubcommittee,
};
use std::sync::Arc;
use types::{BeaconStateError, EthSpec};
use warp::reply::Response;

pub fn get_beacon_state_sync_committees<T: BeaconChainTypes>(
    state_id: StateId,
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
    query: SyncCommitteesQuery,
) -> impl Future<Output = Response> {
    task_spawner.blocking_json_task(Priority::P1, move || {
        let (sync_committee, execution_optimistic, finalized) = state_id
            .map_state_and_execution_optimistic_and_finalized(
                &chain,
                |state, execution_optimistic, finalized| {
                    let current_epoch = state.current_epoch();
                    let epoch = query.epoch.unwrap_or(current_epoch);
                    Ok((
                        state
                            .get_built_sync_committee(epoch, &chain.spec)
                            .cloned()
                            .map_err(|e| match e {
                                BeaconStateError::SyncCommitteeNotKnown { .. } => {
                                    warp_utils::reject::custom_bad_request(format!(
                                        "state at epoch {} has no \
                                                     sync committee for epoch {}",
                                        current_epoch, epoch
                                    ))
                                }
                                BeaconStateError::IncorrectStateVariant => {
                                    warp_utils::reject::custom_bad_request(format!(
                                        "state at epoch {} is not activated for Altair",
                                        current_epoch,
                                    ))
                                }
                                e => warp_utils::reject::beacon_state_error(e),
                            })?,
                        execution_optimistic,
                        finalized,
                    ))
                },
            )?;

        let validators = chain
            .validator_indices(sync_committee.pubkeys.iter())
            .map_err(warp_utils::reject::unhandled_error)?;

        let validator_aggregates = validators
            .chunks_exact(T::EthSpec::sync_subcommittee_size())
            .map(|indices| SyncSubcommittee {
                indices: indices.to_vec(),
            })
            .collect();

        let response = SyncCommitteeByValidatorIndices {
            validators,
            validator_aggregates,
        };

        Ok(GenericResponse::from(response)
            .add_execution_optimistic_finalized(execution_optimistic, finalized))
    })
}
