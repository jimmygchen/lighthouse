use crate::BlockId;
use crate::ExecutionOptimistic;
use crate::sync_committee_rewards::get_state_before_applying_block;
use beacon_chain::{BeaconChainTypes, BeaconSystem};
use eth2::types::StandardBlockReward;
use std::sync::Arc;
use warp_utils::reject::unhandled_error;
/// The difference between block_rewards and beacon_block_rewards is the later returns block
/// reward format that satisfies beacon-api specs
pub fn compute_beacon_block_rewards<T: BeaconChainTypes>(
    chain: Arc<BeaconSystem<T>>,
    block_id: BlockId,
) -> Result<(StandardBlockReward, ExecutionOptimistic, bool), warp::Rejection> {
    let (block, execution_optimistic, finalized) = block_id.blinded_block(&chain)?;

    let block_ref = block.message();

    let mut state = get_state_before_applying_block(chain.clone(), &block)?;

    let rewards = beacon_chain::beacon_block_reward::compute_beacon_block_reward(
        block_ref,
        &mut state,
        &chain.store,
        &chain.canonical_head,
        &chain.spec,
    )
    .map_err(unhandled_error)?;

    Ok((rewards, execution_optimistic, finalized))
}
