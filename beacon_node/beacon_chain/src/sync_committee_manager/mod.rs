#[cfg(test)]
mod tests;

use crate::errors::BeaconChainError as Error;
use crate::naive_aggregation_pool::{
    Error as NaiveAggregationError, NaiveAggregationPool, SyncContributionAggregateMap,
};
use crate::observed_aggregates::ObservedSyncContributions;
use crate::observed_attesters::{ObservedSyncAggregators, ObservedSyncContributors};
use crate::sync_committee_verification::{
    Error as SyncCommitteeError, VerifiedSyncCommitteeMessage, VerifiedSyncContribution,
};
use crate::{BeaconChainTypes, metrics};
use operation_pool::OperationPool;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, trace};
use types::{
    BeaconState, BeaconStateError, ChainSpec, Epoch, EthSpec, SyncCommitteeContribution,
    SyncContributionData, SyncDuty, SyncSubnetId,
};

/// Manages sync committee message and contribution verification, and the
/// sync aggregation pool.
///
/// Generic over `E: EthSpec` rather than `T: BeaconChainTypes` so it can be
/// constructed and tested without a full `BeaconChain`.
///
/// State is passed as method parameters -- this component never fetches head
/// state, slot clock values, or similar chain-level context on its own.
pub struct SyncCommitteeManager<E: EthSpec> {
    spec: Arc<ChainSpec>,
    op_pool: Arc<OperationPool<E>>,
    pub(crate) naive_sync_aggregation_pool:
        RwLock<NaiveAggregationPool<SyncContributionAggregateMap<E>>>,
    pub(crate) observed_sync_contributions: RwLock<ObservedSyncContributions<E>>,
    pub(crate) observed_sync_contributors: RwLock<ObservedSyncContributors<E>>,
    pub(crate) observed_sync_aggregators: RwLock<ObservedSyncAggregators<E>>,
}

impl<E: EthSpec> SyncCommitteeManager<E> {
    /// Create a new `SyncCommitteeManager`.
    pub fn new(spec: Arc<ChainSpec>, op_pool: Arc<OperationPool<E>>) -> Self {
        Self {
            spec,
            op_pool,
            naive_sync_aggregation_pool: <_>::default(),
            observed_sync_contributions: <_>::default(),
            observed_sync_contributors: <_>::default(),
            observed_sync_aggregators: <_>::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Naive sync aggregation pool
    // -----------------------------------------------------------------------

    /// Add a verified sync committee message to the naive aggregation pool.
    ///
    /// The naive aggregation pool is used by local validators to produce
    /// `SignedContributionAndProof`.
    ///
    /// If the sync message is too old (low slot) to be included in the pool
    /// it is simply dropped and no error is returned.
    pub fn add_to_naive_sync_aggregation_pool(
        &self,
        verified_sync_committee_message: VerifiedSyncCommitteeMessage,
    ) -> Result<VerifiedSyncCommitteeMessage, SyncCommitteeError> {
        let sync_message = verified_sync_committee_message.sync_message();
        let positions_by_subnet_id: &HashMap<SyncSubnetId, Vec<usize>> =
            verified_sync_committee_message.subnet_positions();
        for (subnet_id, positions) in positions_by_subnet_id.iter() {
            for position in positions {
                let _timer =
                    metrics::start_timer(&metrics::SYNC_CONTRIBUTION_PROCESSING_APPLY_TO_AGG_POOL);
                let contribution = SyncCommitteeContribution::from_message(
                    sync_message,
                    subnet_id.into(),
                    *position,
                )?;

                match self
                    .naive_sync_aggregation_pool
                    .write()
                    .insert(&contribution)
                {
                    Ok(outcome) => trace!(
                        ?outcome,
                        index = sync_message.validator_index,
                        slot = sync_message.slot.as_u64(),
                        "Stored unaggregated sync committee message"
                    ),
                    Err(NaiveAggregationError::SlotTooLow {
                        slot,
                        lowest_permissible_slot,
                    }) => {
                        trace!(
                            lowest_permissible_slot = lowest_permissible_slot.as_u64(),
                            slot = slot.as_u64(),
                            "Refused to store unaggregated sync committee message"
                        );
                    }
                    Err(e) => {
                        error!(
                            error = ?e,
                            index = sync_message.validator_index,
                            slot = sync_message.slot.as_u64(),
                            "Failed to store unaggregated sync committee message"
                        );
                        return Err(Error::from(e).into());
                    }
                };
            }
        }
        Ok(verified_sync_committee_message)
    }

    /// Return an aggregated `SyncCommitteeContribution` matching the given
    /// `SyncContributionData`, if one exists in the pool.
    pub fn get_aggregated_sync_committee_contribution(
        &self,
        sync_contribution_data: &SyncContributionData,
    ) -> Option<SyncCommitteeContribution<E>> {
        self.naive_sync_aggregation_pool
            .read()
            .get(sync_contribution_data)
    }

    // -----------------------------------------------------------------------
    // Block inclusion pool (op pool)
    // -----------------------------------------------------------------------

    /// Add a verified sync contribution to the op pool for block inclusion.
    ///
    /// The op pool is used by local block producers to pack blocks with
    /// operations.
    pub fn add_contribution_to_block_inclusion_pool<T: BeaconChainTypes<EthSpec = E>>(
        &self,
        contribution: VerifiedSyncContribution<T>,
    ) -> Result<(), SyncCommitteeError> {
        let _timer = metrics::start_timer(&metrics::SYNC_CONTRIBUTION_PROCESSING_APPLY_TO_OP_POOL);

        self.op_pool
            .insert_sync_contribution(contribution.contribution())
            .map_err(Error::from)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Sync committee duties
    // -----------------------------------------------------------------------

    /// Compute sync committee duties for the given epoch and validator indices
    /// from the provided head state.
    pub fn sync_committee_duties(
        &self,
        epoch: Epoch,
        validator_indices: &[u64],
        head_state: &BeaconState<E>,
    ) -> Result<Vec<Result<Option<SyncDuty>, BeaconStateError>>, Error> {
        head_state
            .get_sync_committee_duties(epoch, validator_indices, &self.spec)
            .map_err(Error::SyncDutiesError)
    }
}
