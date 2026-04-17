use std::{sync::Arc, time::Duration};

use proto_array::ProposerHeadError;
use slot_clock::SlotClock;
use tracing::{debug, debug_span, error, info, instrument, warn};
use types::{BeaconState, Hash256, Slot, StatePayloadStatus};

use crate::{
    BeaconChain, BeaconChainTypes, BlockProductionError, StateSkipConfig,
    attestation_manager::AttestationManager, block_times_cache::BlockTimesCache,
    canonical_head::CanonicalHead, execution_manager::ExecutionManager,
    fork_choice_signal::ForkChoiceWaitResult, metrics,
};

mod gloas;

/// Context struct for block production free functions.
///
/// Holds references to the `BeaconChain` fields that block production depends on.
/// Public async entry points (`produce_block_with_verification`, `produce_block_on_state`)
/// remain as `impl BeaconChain<T>` methods that construct this context internally.
pub(crate) struct BlockProductionContext<'a, T: BeaconChainTypes> {
    pub canonical_head: &'a CanonicalHead<T>,
    pub store: &'a crate::BeaconStore<T>,
    pub attestation_manager: &'a AttestationManager<T::EthSpec>,
    pub execution_manager: &'a Arc<ExecutionManager<T>>,
    pub execution_layer: Option<&'a execution_layer::ExecutionLayer<T::EthSpec>>,
    pub op_pool: &'a Arc<operation_pool::OperationPool<T::EthSpec>>,
    pub spec: &'a types::ChainSpec,
    pub slot_clock: &'a T::SlotClock,
    pub config: &'a crate::ChainConfig,
    pub block_times_cache: &'a Arc<parking_lot::RwLock<BlockTimesCache>>,
    pub beacon_proposer_cache:
        &'a Arc<parking_lot::Mutex<crate::beacon_proposer_cache::BeaconProposerCache>>,
    pub genesis_block_root: Hash256,
}

/// Construct a `BlockProductionContext` from a `BeaconChain` reference.
///
/// Module-private helper for `impl BeaconChain<T>` methods. External callers should
/// construct the context from individual component refs.
fn block_production_context_from_chain<T: BeaconChainTypes>(
    chain: &Arc<BeaconChain<T>>,
) -> BlockProductionContext<'_, T> {
    BlockProductionContext {
        canonical_head: &chain.canonical_head,
        store: &chain.store,
        attestation_manager: &chain.attestation_manager,
        execution_manager: &chain.execution_manager,
        execution_layer: chain.execution_layer.as_ref(),
        op_pool: &chain.op_pool,
        spec: &chain.spec,
        slot_clock: &chain.slot_clock,
        config: &chain.config,
        block_times_cache: &chain.block_times_cache,
        beacon_proposer_cache: &chain.beacon_proposer_cache,
        genesis_block_root: chain.genesis_block_root,
    }
}

/// Check if the block with `block_root` was observed after the attestation deadline of `slot`.
pub(crate) fn block_observed_after_attestation_deadline<T: BeaconChainTypes>(
    ctx: &BlockProductionContext<'_, T>,
    block_root: Hash256,
    slot: Slot,
) -> bool {
    let block_delays = ctx.block_times_cache.read().get_block_delays(
        block_root,
        ctx.slot_clock
            .start_of(slot)
            .unwrap_or_else(|| Duration::from_secs(0)),
    );
    block_delays
        .observed
        .is_some_and(|delay| delay >= ctx.spec.get_unaggregated_attestation_due())
}

impl<T: BeaconChainTypes> BeaconChain<T> {
    /// Load a beacon state from the database for block production. This is a long-running process
    /// that should not be performed in an `async` context.
    #[instrument(skip_all, level = "debug")]
    pub(crate) fn load_state_for_block_production(
        self: &Arc<Self>,
        slot: Slot,
    ) -> Result<(BeaconState<T::EthSpec>, Option<Hash256>), BlockProductionError> {
        let ctx = block_production_context_from_chain(self);

        let fork_choice_timer = metrics::start_timer(&metrics::BLOCK_PRODUCTION_FORK_CHOICE_TIMES);
        self.wait_for_fork_choice_before_block_production(slot)?;
        drop(fork_choice_timer);

        let state_load_timer = metrics::start_timer(&metrics::BLOCK_PRODUCTION_STATE_LOAD_TIMES);

        // Atomically read some values from the head whilst avoiding holding cached head `Arc` any
        // longer than necessary.
        let (head_slot, head_block_root, head_state_root) = {
            let head = ctx.canonical_head.cached_head();
            (
                head.head_slot(),
                head.head_block_root(),
                head.head_state_root(),
            )
        };
        let (state, state_root_opt) = if head_slot < slot {
            // Attempt an aggressive re-org if configured and the conditions are right.
            // TODO(gloas): re-enable reorgs
            let gloas_enabled = ctx
                .spec
                .fork_name_at_slot::<T::EthSpec>(slot)
                .gloas_enabled();
            if !gloas_enabled
                && let Some((re_org_state, re_org_state_root)) =
                    get_state_for_re_org(&ctx, slot, head_slot, head_block_root)
            {
                info!(
                    %slot,
                    head_to_reorg = %head_block_root,
                    "Proposing block to re-org current head"
                );
                (re_org_state, Some(re_org_state_root))
            } else {
                // Fetch the head state advanced through to `slot`, which should be present in the
                // state cache thanks to the state advance timer.
                // TODO(gloas): need to fix this once fork choice understands payloads
                // for now we just use the existence of the head's payload envelope to determine
                // whether we should build atop it
                let (payload_status, parent_state_root) = if gloas_enabled
                    && let Ok(Some(envelope)) = ctx.store.get_payload_envelope(&head_block_root)
                {
                    debug!(
                        %slot,
                        parent_state_root = ?envelope.message.state_root,
                        parent_block_root = ?head_block_root,
                        "Building Gloas block on full state"
                    );
                    (StatePayloadStatus::Full, envelope.message.state_root)
                } else {
                    (StatePayloadStatus::Pending, head_state_root)
                };
                let (state_root, state) = ctx
                    .store
                    .get_advanced_hot_state(
                        head_block_root,
                        payload_status,
                        slot,
                        parent_state_root,
                    )
                    .map_err(BlockProductionError::FailedToLoadState)?
                    .ok_or(BlockProductionError::UnableToProduceAtSlot(slot))?;
                (state, Some(state_root))
            }
        } else {
            warn!(
                message = "this block is more likely to be orphaned",
                %slot,
                "Producing block that conflicts with head"
            );
            let state = crate::state_query::state_at_slot(
                &self.store,
                &self.canonical_head,
                &self.spec,
                slot - 1,
                StateSkipConfig::WithStateRoots,
            )
            .map_err(|_| BlockProductionError::UnableToProduceAtSlot(slot))?;

            (state, None)
        };

        drop(state_load_timer);

        Ok((state, state_root_opt))
    }

    /// If configured, wait for the fork choice run at the start of the slot to complete.
    #[instrument(level = "debug", skip_all)]
    fn wait_for_fork_choice_before_block_production(
        self: &Arc<Self>,
        slot: Slot,
    ) -> Result<(), BlockProductionError> {
        if let Some(rx) = &self.fork_choice_signal_rx {
            let current_slot = crate::state_query::current_slot(&self.slot_clock)
                .map_err(|_| BlockProductionError::UnableToReadSlot)?;

            let timeout = Duration::from_millis(self.config.fork_choice_before_proposal_timeout_ms);

            if slot == current_slot || slot == current_slot + 1 {
                match rx.wait_for_fork_choice(slot, timeout) {
                    ForkChoiceWaitResult::Success(fc_slot) => {
                        debug!(
                            %slot,
                            fork_choice_slot = %fc_slot,
                            "Fork choice successfully updated before block production"
                        );
                    }
                    ForkChoiceWaitResult::Behind(fc_slot) => {
                        warn!(
                            fork_choice_slot = %fc_slot,
                            %slot,
                            message = "this block may be orphaned",
                            "Fork choice notifier out of sync with block production"
                        );
                    }
                    ForkChoiceWaitResult::TimeOut => {
                        warn!(
                            message = "this block may be orphaned",
                            "Timed out waiting for fork choice before proposal"
                        );
                    }
                }
            } else {
                error!(
                    %slot,
                    %current_slot,
                    message = "check clock sync, this block may be orphaned",
                    "Producing block at incorrect slot"
                );
            }
        }
        Ok(())
    }
}

/// Fetch the beacon state to use for producing a block if a 1-slot proposer re-org is viable.
///
/// This function will return `None` if proposer re-orgs are disabled.
#[instrument(skip_all, level = "debug")]
fn get_state_for_re_org<T: BeaconChainTypes>(
    ctx: &BlockProductionContext<'_, T>,
    slot: Slot,
    head_slot: Slot,
    canonical_head_root: Hash256,
) -> Option<(BeaconState<T::EthSpec>, Hash256)> {
    let re_org_head_threshold = ctx.config.re_org_head_threshold?;
    let re_org_parent_threshold = ctx.config.re_org_parent_threshold?;

    if ctx.spec.proposer_score_boost.is_none() {
        warn!(
            reason = "this network does not have proposer boost enabled",
            "Ignoring proposer re-org configuration"
        );
        return None;
    }

    let slot_delay = ctx
        .slot_clock
        .seconds_from_current_slot_start()
        .or_else(|| {
            warn!(error = "unable to read slot clock", "Not attempting re-org");
            None
        })?;

    // Attempt a proposer re-org if:
    //
    // 1. It seems we have time to propagate and still receive the proposer boost.
    // 2. The current head block was seen late.
    // 3. The `get_proposer_head` conditions from fork choice pass.
    let proposing_on_time = slot_delay < ctx.config.re_org_cutoff(ctx.spec.get_slot_duration());
    if !proposing_on_time {
        debug!(reason = "not proposing on time", "Not attempting re-org");
        return None;
    }

    let head_late = block_observed_after_attestation_deadline(ctx, canonical_head_root, head_slot);
    if !head_late {
        debug!(reason = "head not late", "Not attempting re-org");
        return None;
    }

    // Is the current head weak and appropriate for re-orging?
    let proposer_head_timer =
        metrics::start_timer(&metrics::BLOCK_PRODUCTION_GET_PROPOSER_HEAD_TIMES);
    let proposer_head = ctx
        .canonical_head
        .fork_choice_read_lock()
        .get_proposer_head(
            slot,
            canonical_head_root,
            re_org_head_threshold,
            re_org_parent_threshold,
            &ctx.config.re_org_disallowed_offsets,
            ctx.config.re_org_max_epochs_since_finalization,
        )
        .map_err(|e| match e {
            ProposerHeadError::DoNotReOrg(reason) => {
                debug!(
                    %reason,
                    "Not attempting re-org"
                );
            }
            ProposerHeadError::Error(e) => {
                warn!(
                    error = ?e,
                    "Not attempting re-org"
                );
            }
        })
        .ok()?;
    drop(proposer_head_timer);
    let re_org_parent_block = proposer_head.parent_node.root();

    let (state_root, state) = ctx
        .store
        .get_advanced_hot_state_from_cache(re_org_parent_block, StatePayloadStatus::Pending, slot)
        .or_else(|| {
            warn!(reason = "no state in cache", "Not attempting re-org");
            None
        })?;

    info!(
        weak_head = ?canonical_head_root,
        parent = ?re_org_parent_block,
        head_weight = proposer_head.head_node.weight(),
        threshold_weight = proposer_head.re_org_head_weight_threshold,
        "Attempting re-org due to weak head"
    );

    Some((state, state_root))
}

// Additional imports for methods moved from beacon_chain.rs
use crate::beacon_chain::{
    BeaconBlockResponse, BeaconBlockResponseWrapper, PartialBeaconBlock, PrePayloadAttributes,
    ProduceBlockVerification, shuffling_is_compatible_with_fork_choice,
};
use crate::errors::BeaconChainError as Error;
use crate::execution_payload::get_execution_payload;
use crate::graffiti_calculator::GraffitiSettings;
use crate::{BeaconChainError, CachedHead};
use bls::Signature;
use execution_layer::{BlockProposalContents, BlockProposalContentsType, BuilderParams};
use fixed_bytes::FixedBytesExtended;
use fork_choice::ForkchoiceUpdateParameters;
use operation_pool::CompactAttestationRef;
use proto_array::DoNotReOrg;
use ssz::Encode;
use state_processing::{
    BlockSignatureStrategy, ConsensusContext, VerifyBlockRoot, VerifyOperation,
    common::get_attesting_indices_from_state,
    epoch_cache::initialize_epoch_cache,
    per_block_processing,
    per_block_processing::{
        VerifySignatures, get_expected_withdrawals, verify_attestation_for_block_inclusion,
    },
    state_advance::{complete_state_advance, partial_state_advance},
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::marker::PhantomData;
use tracing::trace;
use types::execution::BlockProductionVersion;
use types::*;

impl<T: BeaconChainTypes> BeaconChain<T> {
    pub async fn produce_block_with_verification(
        self: &Arc<Self>,
        randao_reveal: Signature,
        slot: Slot,
        graffiti_settings: GraffitiSettings,
        verification: ProduceBlockVerification,
        builder_boost_factor: Option<u64>,
        block_production_version: BlockProductionVersion,
    ) -> Result<BeaconBlockResponseWrapper<T::EthSpec>, BlockProductionError> {
        metrics::inc_counter(&metrics::BLOCK_PRODUCTION_REQUESTS);
        let _complete_timer = metrics::start_timer(&metrics::BLOCK_PRODUCTION_TIMES);
        // Part 1/2 (blocking)
        //
        // Load the parent state from disk.
        let chain = self.clone();
        let (state, state_root_opt) = self
            .task_executor
            .spawn_blocking_handle(
                move || chain.load_state_for_block_production(slot),
                "load_state_for_block_production",
            )
            .ok_or(BlockProductionError::ShuttingDown)?
            .await
            .map_err(BlockProductionError::TokioJoin)??;

        // Part 2/2 (async, with some blocking components)
        //
        // Produce the block upon the state
        self.produce_block_on_state(
            state,
            state_root_opt,
            slot,
            randao_reveal,
            graffiti_settings,
            verification,
            builder_boost_factor,
            block_production_version,
        )
        .await
    }

    /// Get the proposer index and `prev_randao` value for a proposal at slot `proposal_slot`.
    ///
    /// Delegates to the free function [`get_pre_payload_attributes`].
    pub fn get_pre_payload_attributes(
        self: &Arc<Self>,
        proposal_slot: Slot,
        proposer_head: Hash256,
        cached_head: &CachedHead<T::EthSpec>,
    ) -> Result<Option<PrePayloadAttributes>, Error> {
        let ctx = block_production_context_from_chain(self);
        get_pre_payload_attributes(&ctx, proposal_slot, proposer_head, cached_head)
    }

    /// Delegates to the free function [`get_expected_withdrawals_for_proposal`].
    pub fn get_expected_withdrawals(
        self: &Arc<Self>,
        forkchoice_update_params: &ForkchoiceUpdateParameters,
        proposal_slot: Slot,
    ) -> Result<Withdrawals<T::EthSpec>, Error> {
        let ctx = block_production_context_from_chain(self);
        get_expected_withdrawals_for_proposal(&ctx, forkchoice_update_params, proposal_slot)
    }

    /// Determine whether a fork choice update to the execution layer should be overridden.
    ///
    /// Delegates to the free function [`overridden_forkchoice_update_params`].
    pub fn overridden_forkchoice_update_params(
        self: &Arc<Self>,
        canonical_forkchoice_params: ForkchoiceUpdateParameters,
    ) -> Result<ForkchoiceUpdateParameters, Error> {
        let ctx = block_production_context_from_chain(self);
        overridden_forkchoice_update_params_fn(&ctx, canonical_forkchoice_params)
    }

    /// Delegates to the free function [`overridden_forkchoice_update_params_or_failure_reason`].
    pub fn overridden_forkchoice_update_params_or_failure_reason(
        self: &Arc<Self>,
        canonical_forkchoice_params: &ForkchoiceUpdateParameters,
    ) -> Result<ForkchoiceUpdateParameters, Box<ProposerHeadError<Error>>> {
        let ctx = block_production_context_from_chain(self);
        overridden_forkchoice_update_params_or_failure_reason_fn(&ctx, canonical_forkchoice_params)
    }

    /// Check if the block with `block_root` was observed after the attestation deadline of `slot`.
    ///
    /// Delegates to the free function [`block_observed_after_attestation_deadline`].
    pub(crate) fn block_observed_after_attestation_deadline(
        self: &Arc<Self>,
        block_root: Hash256,
        slot: Slot,
    ) -> bool {
        let ctx = block_production_context_from_chain(self);
        block_observed_after_attestation_deadline(&ctx, block_root, slot)
    }

    /// Produce a block for some `slot` upon the given `state`.
    ///
    /// Typically the `self.produce_block()` function should be used, instead of calling this
    /// function directly. This function is useful for purposefully creating forks or blocks at
    /// non-current slots.
    ///
    /// If required, the given state will be advanced to the given `produce_at_slot`, then a block
    /// will be produced at that slot height.
    ///
    /// The provided `state_root_opt` should only ever be set to `Some` if the contained value is
    /// equal to the root of `state`. Providing this value will serve as an optimization to avoid
    /// performing a tree hash in some scenarios.
    #[allow(clippy::too_many_arguments)]
    #[instrument(level = "debug", skip_all)]
    pub async fn produce_block_on_state(
        self: &Arc<Self>,
        state: BeaconState<T::EthSpec>,
        state_root_opt: Option<Hash256>,
        produce_at_slot: Slot,
        randao_reveal: Signature,
        graffiti_settings: GraffitiSettings,
        verification: ProduceBlockVerification,
        builder_boost_factor: Option<u64>,
        block_production_version: BlockProductionVersion,
    ) -> Result<BeaconBlockResponseWrapper<T::EthSpec>, BlockProductionError> {
        // Part 1/3 (blocking)
        //
        // Perform the state advance and block-packing functions.
        let chain = self.clone();
        let graffiti = self
            .graffiti_calculator
            .get_graffiti(graffiti_settings)
            .await;
        let mut partial_beacon_block = self
            .task_executor
            .spawn_blocking_handle(
                move || {
                    let ctx = block_production_context_from_chain(&chain);
                    produce_partial_beacon_block(
                        &ctx,
                        &chain,
                        state,
                        state_root_opt,
                        produce_at_slot,
                        randao_reveal,
                        graffiti,
                        builder_boost_factor,
                        block_production_version,
                    )
                },
                "produce_partial_beacon_block",
            )
            .ok_or(BlockProductionError::ShuttingDown)?
            .await
            .map_err(BlockProductionError::TokioJoin)??;
        // Part 2/3 (async)
        //
        // Wait for the execution layer to return an execution payload (if one is required).
        let prepare_payload_handle = partial_beacon_block.prepare_payload_handle.take();
        let block_contents_type_option =
            if let Some(prepare_payload_handle) = prepare_payload_handle {
                Some(
                    prepare_payload_handle
                        .await
                        .map_err(BlockProductionError::TokioJoin)?
                        .ok_or(BlockProductionError::ShuttingDown)??,
                )
            } else {
                None
            };
        // Part 3/3 (blocking)
        if let Some(block_contents_type) = block_contents_type_option {
            match block_contents_type {
                BlockProposalContentsType::Full(block_contents) => {
                    let chain = self.clone();
                    let beacon_block_response = self
                        .task_executor
                        .spawn_blocking_handle(
                            move || {
                                let ctx = block_production_context_from_chain(&chain);
                                complete_partial_beacon_block(
                                    &ctx,
                                    &chain,
                                    partial_beacon_block,
                                    Some(block_contents),
                                    verification,
                                )
                            },
                            "complete_partial_beacon_block",
                        )
                        .ok_or(BlockProductionError::ShuttingDown)?
                        .await
                        .map_err(BlockProductionError::TokioJoin)??;

                    Ok(BeaconBlockResponseWrapper::Full(beacon_block_response))
                }
                BlockProposalContentsType::Blinded(block_contents) => {
                    let chain = self.clone();
                    let beacon_block_response = self
                        .task_executor
                        .spawn_blocking_handle(
                            move || {
                                let ctx = block_production_context_from_chain(&chain);
                                complete_partial_beacon_block(
                                    &ctx,
                                    &chain,
                                    partial_beacon_block,
                                    Some(block_contents),
                                    verification,
                                )
                            },
                            "complete_partial_beacon_block",
                        )
                        .ok_or(BlockProductionError::ShuttingDown)?
                        .await
                        .map_err(BlockProductionError::TokioJoin)??;

                    Ok(BeaconBlockResponseWrapper::Blinded(beacon_block_response))
                }
            }
        } else {
            let chain = self.clone();
            let beacon_block_response = self
                .task_executor
                .spawn_blocking_handle(
                    move || {
                        let ctx = block_production_context_from_chain(&chain);
                        complete_partial_beacon_block(
                            &ctx,
                            &chain,
                            partial_beacon_block,
                            None,
                            verification,
                        )
                    },
                    "complete_partial_beacon_block",
                )
                .ok_or(BlockProductionError::ShuttingDown)?
                .await
                .map_err(BlockProductionError::TokioJoin)??;

            Ok(BeaconBlockResponseWrapper::Full(beacon_block_response))
        }
    }
}

/// Get the proposer index and `prev_randao` value for a proposal at slot `proposal_slot`.
///
/// The `proposer_head` may be the head block of `cached_head` or its parent. An error will
/// be returned for any other value.
pub(crate) fn get_pre_payload_attributes<T: BeaconChainTypes>(
    ctx: &BlockProductionContext<'_, T>,
    proposal_slot: Slot,
    proposer_head: Hash256,
    cached_head: &CachedHead<T::EthSpec>,
) -> Result<Option<PrePayloadAttributes>, Error> {
    let proposal_epoch = proposal_slot.epoch(T::EthSpec::slots_per_epoch());

    let head_block_root = cached_head.head_block_root();
    let head_parent_block_root = cached_head.parent_block_root();

    // The proposer head must be equal to the canonical head or its parent.
    if proposer_head != head_block_root && proposer_head != head_parent_block_root {
        warn!(
            block_root = ?proposer_head,
            head_block_root = ?head_block_root,
            "Unable to compute payload attributes"
        );
        return Ok(None);
    }

    // Compute the proposer index.
    let head_epoch = cached_head.head_slot().epoch(T::EthSpec::slots_per_epoch());
    let shuffling_decision_root = cached_head
        .snapshot
        .beacon_state
        .proposer_shuffling_decision_root_at_epoch(proposal_epoch, proposer_head, ctx.spec)?;

    let Some(proposer_index) = ctx
        .execution_manager
        .with_proposer_cache(
            shuffling_decision_root,
            proposal_epoch,
            |proposers| {
                proposers
                    .get_slot::<T::EthSpec>(proposal_slot)
                    .map(|p| p.index as u64)
            },
            || {
                if head_epoch + ctx.config.sync_tolerance_epochs < proposal_epoch {
                    warn!(
                        msg = "this is a non-critical issue that can happen on unhealthy nodes or \
                               networks",
                        %proposal_epoch,
                        %head_epoch,
                        "Skipping proposer preparation"
                    );

                    // Don't skip the head forward too many epochs. This avoids burdening an
                    // unhealthy node.
                    //
                    // Although this node might miss out on preparing for a proposal, they should
                    // still be able to propose. This will prioritise beacon chain health over
                    // efficient packing of execution blocks.
                    Err(Error::SkipProposerPreparation)
                } else {
                    debug!(
                        ?shuffling_decision_root,
                        epoch = %proposal_epoch,
                        "Proposer shuffling cache miss for proposer prep"
                    );
                    let head = ctx.canonical_head.cached_head();
                    Ok((head.head_state_root(), head.snapshot.beacon_state.clone()))
                }
            },
        )
        .map_or_else(
            |e| {
                match e {
                    Error::ProposerCacheIncorrectState { .. } => {
                        warn!("Head changed during proposer preparation");
                        Ok(None)
                    }
                    Error::SkipProposerPreparation => {
                        // Warning logged for this above.
                        Ok(None)
                    }
                    e => Err(e),
                }
            },
            |value| Ok(Some(value)),
        )?
    else {
        return Ok(None);
    };

    // TODO(gloas) not sure what to do here see this issue
    // https://github.com/sigp/lighthouse/issues/8817
    let (prev_randao, parent_block_number) = if ctx
        .spec
        .fork_name_at_slot::<T::EthSpec>(proposal_slot)
        .gloas_enabled()
    {
        (cached_head.head_random()?, None)
    } else {
        // Get the `prev_randao` and parent block number.
        let head_block_number = cached_head.head_block_number()?;
        if proposer_head == head_parent_block_root {
            (
                cached_head.parent_random()?,
                Some(head_block_number.saturating_sub(1)),
            )
        } else {
            (cached_head.head_random()?, Some(head_block_number))
        }
    };

    Ok(Some(PrePayloadAttributes {
        proposer_index,
        prev_randao,
        parent_block_number,
        parent_beacon_block_root: proposer_head,
    }))
}

/// Compute expected withdrawals for a proposal at `proposal_slot`.
pub(crate) fn get_expected_withdrawals_for_proposal<T: BeaconChainTypes>(
    ctx: &BlockProductionContext<'_, T>,
    forkchoice_update_params: &ForkchoiceUpdateParameters,
    proposal_slot: Slot,
) -> Result<Withdrawals<T::EthSpec>, Error> {
    let cached_head = ctx.canonical_head.cached_head();
    let head_state = &cached_head.snapshot.beacon_state;

    let parent_block_root = forkchoice_update_params.head_root;

    let (unadvanced_state, unadvanced_state_root) =
        if cached_head.head_block_root() == parent_block_root {
            (Cow::Borrowed(head_state), cached_head.head_state_root())
        } else {
            // TODO(gloas): this function needs updating to be envelope-aware
            // See: https://github.com/sigp/lighthouse/issues/8957
            let block = ctx
                .store
                .get_blinded_block(&parent_block_root)?
                .ok_or(Error::MissingBeaconBlock(parent_block_root))?;
            let (state_root, state) = ctx
                .store
                .get_advanced_hot_state(
                    parent_block_root,
                    StatePayloadStatus::Pending,
                    proposal_slot,
                    block.state_root(),
                )?
                .ok_or(Error::MissingBeaconState(block.state_root()))?;
            (Cow::Owned(state), state_root)
        };

    // Parent state epoch is the same as the proposal, we don't need to advance because the
    // list of expected withdrawals can only change after an epoch advance or a
    // block application.
    let proposal_epoch = proposal_slot.epoch(T::EthSpec::slots_per_epoch());
    if head_state.current_epoch() == proposal_epoch {
        return get_expected_withdrawals(&unadvanced_state, ctx.spec)
            .map(Into::into)
            .map_err(Error::PrepareProposerFailed);
    }

    // Advance the state using the partial method.
    debug!(
        %proposal_slot,
        ?parent_block_root,
        "Advancing state for withdrawals calculation"
    );
    let mut advanced_state = unadvanced_state.into_owned();
    partial_state_advance(
        &mut advanced_state,
        Some(unadvanced_state_root),
        proposal_epoch.start_slot(T::EthSpec::slots_per_epoch()),
        ctx.spec,
    )?;
    get_expected_withdrawals(&advanced_state, ctx.spec)
        .map(Into::into)
        .map_err(Error::PrepareProposerFailed)
}

/// Determine whether a fork choice update to the execution layer should be overridden.
///
/// This is *only* necessary when proposer re-orgs are enabled, because we have to prevent the
/// execution layer from enshrining the block we want to re-org as the head.
///
/// This function uses heuristics that align quite closely but not exactly with the re-org
/// conditions set out in `get_state_for_re_org` and `get_proposer_head`. The differences are
/// documented below.
fn overridden_forkchoice_update_params_fn<T: BeaconChainTypes>(
    ctx: &BlockProductionContext<'_, T>,
    canonical_forkchoice_params: ForkchoiceUpdateParameters,
) -> Result<ForkchoiceUpdateParameters, Error> {
    overridden_forkchoice_update_params_or_failure_reason_fn(ctx, &canonical_forkchoice_params)
        .or_else(|e| match *e {
            ProposerHeadError::DoNotReOrg(reason) => {
                trace!(
                    %reason,
                    "Not suppressing fork choice update"
                );
                Ok(canonical_forkchoice_params)
            }
            ProposerHeadError::Error(e) => Err(e),
        })
}

// TODO(gloas): wrong for Gloas, needs an update
fn overridden_forkchoice_update_params_or_failure_reason_fn<T: BeaconChainTypes>(
    ctx: &BlockProductionContext<'_, T>,
    canonical_forkchoice_params: &ForkchoiceUpdateParameters,
) -> Result<ForkchoiceUpdateParameters, Box<ProposerHeadError<Error>>> {
    let _timer = metrics::start_timer(&metrics::FORK_CHOICE_OVERRIDE_FCU_TIMES);

    // Never override if proposer re-orgs are disabled.
    let re_org_head_threshold = ctx
        .config
        .re_org_head_threshold
        .ok_or(Box::new(DoNotReOrg::ReOrgsDisabled.into()))?;

    let re_org_parent_threshold = ctx
        .config
        .re_org_parent_threshold
        .ok_or(Box::new(DoNotReOrg::ReOrgsDisabled.into()))?;

    let head_block_root = canonical_forkchoice_params.head_root;

    // Perform initial checks and load the relevant info from fork choice.
    let info = ctx
        .canonical_head
        .fork_choice_read_lock()
        .get_preliminary_proposer_head(
            head_block_root,
            re_org_head_threshold,
            re_org_parent_threshold,
            &ctx.config.re_org_disallowed_offsets,
            ctx.config.re_org_max_epochs_since_finalization,
        )
        .map_err(|e| e.map_inner_error(Error::ProposerHeadForkChoiceError))?;

    // The slot of our potential re-org block is always 1 greater than the head block because we
    // only attempt single-slot re-orgs.
    let head_slot = info.head_node.slot();
    let re_org_block_slot = head_slot + 1;
    let fork_choice_slot = info.current_slot;

    // If a re-orging proposal isn't made by the `re_org_cutoff` then we give up
    // and allow the fork choice update for the canonical head through so that we may attest
    // correctly.
    let current_slot_ok = if head_slot == fork_choice_slot {
        true
    } else if re_org_block_slot == fork_choice_slot {
        ctx.slot_clock
            .start_of(re_org_block_slot)
            .and_then(|slot_start| {
                let now = ctx.slot_clock.now_duration()?;
                let slot_delay = now.saturating_sub(slot_start);
                Some(slot_delay <= ctx.config.re_org_cutoff(ctx.spec.get_slot_duration()))
            })
            .unwrap_or(false)
    } else {
        false
    };
    if !current_slot_ok {
        return Err(Box::new(DoNotReOrg::HeadDistance.into()));
    }

    // Only attempt a re-org if we have a proposer registered for the re-org slot.
    let proposing_at_re_org_slot = {
        // We know our re-org block is not on the epoch boundary, so it has the same proposer
        // shuffling as the head (but not necessarily the parent which may lie in the previous
        // epoch).
        let shuffling_decision_root = if ctx
            .spec
            .fork_name_at_slot::<T::EthSpec>(re_org_block_slot)
            .fulu_enabled()
        {
            info.head_node.current_epoch_shuffling_id()
        } else {
            info.head_node.next_epoch_shuffling_id()
        }
        .shuffling_decision_block;
        let proposer_index = ctx
            .beacon_proposer_cache
            .lock()
            .get_slot::<T::EthSpec>(shuffling_decision_root, re_org_block_slot)
            .ok_or_else(|| {
                debug!(
                    slot = %re_org_block_slot,
                    decision_root = ?shuffling_decision_root,
                    "Fork choice override proposer shuffling miss"
                );
                Box::new(DoNotReOrg::NotProposing.into())
            })?
            .index as u64;

        ctx.execution_layer
            .ok_or(ProposerHeadError::Error(Error::ExecutionLayerMissing))?
            .has_proposer_preparation_data_blocking(proposer_index)
    };
    if !proposing_at_re_org_slot {
        return Err(Box::new(DoNotReOrg::NotProposing.into()));
    }

    // TODO(gloas): reorg weight logic needs updating for Gloas. For now use
    // total weight which is correct for pre-Gloas and conservative for post-Gloas.
    let head_weight = info.head_node.weight();
    let parent_weight = info.parent_node.weight();

    let (head_weak, parent_strong) = if fork_choice_slot == re_org_block_slot {
        (
            head_weight < info.re_org_head_weight_threshold,
            parent_weight > info.re_org_parent_weight_threshold,
        )
    } else {
        (true, true)
    };
    if !head_weak {
        return Err(Box::new(
            DoNotReOrg::HeadNotWeak {
                head_weight,
                re_org_head_weight_threshold: info.re_org_head_weight_threshold,
            }
            .into(),
        ));
    }
    if !parent_strong {
        return Err(Box::new(
            DoNotReOrg::ParentNotStrong {
                parent_weight,
                re_org_parent_weight_threshold: info.re_org_parent_weight_threshold,
            }
            .into(),
        ));
    }

    // Check that the head block arrived late and is vulnerable to a re-org. This check is only
    // a heuristic compared to the proper weight check in `get_state_for_re_org`, the reason
    // being that we may have only *just* received the block and not yet processed any
    // attestations for it. We also can't dequeue attestations for the block during the
    // current slot, which would be necessary for determining its weight.
    let head_block_late =
        block_observed_after_attestation_deadline(&ctx, head_block_root, head_slot);
    if !head_block_late {
        return Err(Box::new(DoNotReOrg::HeadNotLate.into()));
    }

    // TODO(gloas): V29 nodes don't carry execution_status, so this returns
    // None for post-Gloas re-orgs. Need to source the EL block hash from
    // the bid's block_hash instead. Re-org is disabled for Gloas for now.
    let parent_head_hash = info
        .parent_node
        .execution_status()
        .ok()
        .and_then(|execution_status| execution_status.block_hash());
    let forkchoice_update_params = ForkchoiceUpdateParameters {
        head_root: info.parent_node.root(),
        head_hash: parent_head_hash,
        justified_hash: canonical_forkchoice_params.justified_hash,
        finalized_hash: canonical_forkchoice_params.finalized_hash,
    };

    debug!(
        canonical_head = ?head_block_root,
        parent_root = ?info.parent_node.root(),
        slot = %fork_choice_slot,
        "Fork choice update overridden"
    );

    Ok(forkchoice_update_params)
}

/// Core block assembly logic: advance state, pack operations, begin payload fetch.
///
/// The `chain` parameter is needed only because `get_execution_payload` spawns
/// an async task that requires `Arc<BeaconChain<T>>`. Everything else goes through
/// `ctx` with explicit deps.
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, level = "debug")]
pub(crate) fn produce_partial_beacon_block<T: BeaconChainTypes>(
    ctx: &BlockProductionContext<'_, T>,
    chain: &Arc<BeaconChain<T>>,
    mut state: BeaconState<T::EthSpec>,
    state_root_opt: Option<Hash256>,
    produce_at_slot: Slot,
    randao_reveal: Signature,
    graffiti: Graffiti,
    builder_boost_factor: Option<u64>,
    block_production_version: BlockProductionVersion,
) -> Result<PartialBeaconBlock<T::EthSpec>, BlockProductionError> {
    // It is invalid to try to produce a block using a state from a future slot.
    if state.slot() > produce_at_slot {
        return Err(BlockProductionError::StateSlotTooHigh {
            produce_at_slot,
            state_slot: state.slot(),
        });
    }

    let slot_timer = metrics::start_timer(&metrics::BLOCK_PRODUCTION_SLOT_PROCESS_TIMES);

    // Ensure the state has performed a complete transition into the required slot.
    complete_state_advance(&mut state, state_root_opt, produce_at_slot, ctx.spec)?;

    drop(slot_timer);

    state.build_committee_cache(RelativeEpoch::Current, ctx.spec)?;
    state.apply_pending_mutations()?;

    let parent_root = if state.slot() > 0 {
        *state
            .get_block_root(state.slot() - 1)
            .map_err(|_| BlockProductionError::UnableToGetBlockRootFromState)?
    } else {
        state.latest_block_header().canonical_root()
    };

    let proposer_index = state.get_beacon_proposer_index(state.slot(), ctx.spec)? as u64;

    let pubkey = state
        .validators()
        .get(proposer_index as usize)
        .map(|v| v.pubkey)
        .ok_or(BlockProductionError::BeaconChain(Box::new(
            BeaconChainError::ValidatorIndexUnknown(proposer_index as usize),
        )))?;

    let builder_params = BuilderParams {
        pubkey,
        slot: state.slot(),
        chain_health: crate::beacon_chain::is_healthy(
            ctx.canonical_head,
            ctx.store,
            ctx.slot_clock,
            ctx.config,
            ctx.spec,
            ctx.genesis_block_root,
            &parent_root,
        )
        .map_err(|e| BlockProductionError::BeaconChain(Box::new(e)))?,
    };

    // If required, start the process of loading an execution payload from the EL early. This
    // allows it to run concurrently with things like attestation packing.
    let prepare_payload_handle = if state.fork_name_unchecked().bellatrix_enabled() {
        let prepare_payload_handle = get_execution_payload(
            chain.clone(),
            &state,
            parent_root,
            proposer_index,
            builder_params,
            builder_boost_factor,
            block_production_version,
        )?;
        Some(prepare_payload_handle)
    } else {
        None
    };

    let slashings_and_exits_span = debug_span!("get_slashings_and_exits").entered();
    let (mut proposer_slashings, mut attester_slashings, mut voluntary_exits) =
        ctx.op_pool.get_slashings_and_exits(&state, ctx.spec);
    drop(slashings_and_exits_span);

    let eth1_data = state.eth1_data().clone();

    let deposits = vec![];

    let bls_changes_span = debug_span!("get_bls_to_execution_changes").entered();
    let bls_to_execution_changes = ctx.op_pool.get_bls_to_execution_changes(&state, ctx.spec);
    drop(bls_changes_span);

    // Iterate through the naive aggregation pool and ensure all the attestations from there
    // are included in the operation pool.
    {
        let _guard = debug_span!("import_naive_aggregation_pool").entered();
        let _unagg_import_timer =
            metrics::start_timer(&metrics::BLOCK_PRODUCTION_UNAGGREGATED_TIMES);
        for attestation in ctx.attestation_manager.naive_aggregation_pool.read().iter() {
            let import = |attestation: &Attestation<T::EthSpec>| {
                let attesting_indices =
                    get_attesting_indices_from_state(&state, attestation.to_ref())?;
                ctx.op_pool
                    .insert_attestation(attestation.clone(), attesting_indices)
            };
            if let Err(e) = import(attestation) {
                // Don't stop block production if there's an error, just create a log.
                error!(
                    reason = ?e,
                    "Attestation did not transfer to op pool"
                );
            }
        }
    };

    let mut attestations = {
        let _guard = debug_span!("pack_attestations").entered();
        let _attestation_packing_timer =
            metrics::start_timer(&metrics::BLOCK_PRODUCTION_ATTESTATION_TIMES);

        // Epoch cache and total balance cache are required for op pool packing.
        state.build_total_active_balance_cache(ctx.spec)?;
        initialize_epoch_cache(&mut state, ctx.spec)?;

        let shuffling_is_compatible = |block_root: &Hash256, target_epoch: Epoch| -> bool {
            shuffling_is_compatible_with_fork_choice(
                block_root,
                target_epoch,
                &state,
                ctx.canonical_head,
                ctx.attestation_manager,
            )
        };
        let mut prev_filter_cache = HashMap::new();
        let prev_attestation_filter = |att: &CompactAttestationRef<T::EthSpec>| {
            *prev_filter_cache
                .entry((att.data.beacon_block_root, att.checkpoint.target_epoch))
                .or_insert_with(|| {
                    shuffling_is_compatible(
                        &att.data.beacon_block_root,
                        att.checkpoint.target_epoch,
                    )
                })
        };
        let mut curr_filter_cache = HashMap::new();
        let curr_attestation_filter = |att: &CompactAttestationRef<T::EthSpec>| {
            *curr_filter_cache
                .entry((att.data.beacon_block_root, att.checkpoint.target_epoch))
                .or_insert_with(|| {
                    shuffling_is_compatible(
                        &att.data.beacon_block_root,
                        att.checkpoint.target_epoch,
                    )
                })
        };

        ctx.op_pool
            .get_attestations(
                &state,
                prev_attestation_filter,
                curr_attestation_filter,
                ctx.spec,
            )
            .map_err(BlockProductionError::OpPoolError)?
    };

    // If paranoid mode is enabled re-check the signatures of every included message.
    // This will be a lot slower but guards against bugs in block production and can be
    // quickly rolled out without a release.
    if ctx.config.paranoid_block_proposal {
        let mut tmp_ctxt = ConsensusContext::new(state.slot());
        attestations.retain(|att| {
            verify_attestation_for_block_inclusion(
                &state,
                att.to_ref(),
                &mut tmp_ctxt,
                VerifySignatures::True,
                ctx.spec,
            )
            .map_err(|e| {
                warn!(
                    err = ?e,
                    block_slot = %state.slot(),
                    attestation = ?att,
                    "Attempted to include an invalid attestation"
                );
            })
            .is_ok()
        });

        proposer_slashings.retain(|slashing| {
            slashing
                .clone()
                .validate(&state, ctx.spec)
                .map_err(|e| {
                    warn!(
                        err = ?e,
                        block_slot = %state.slot(),
                        ?slashing,
                        "Attempted to include an invalid proposer slashing"
                    );
                })
                .is_ok()
        });

        attester_slashings.retain(|slashing| {
            slashing
                .clone()
                .validate(&state, ctx.spec)
                .map_err(|e| {
                    warn!(
                        err = ?e,
                        block_slot = %state.slot(),
                        ?slashing,
                        "Attempted to include an invalid attester slashing"
                    );
                })
                .is_ok()
        });

        voluntary_exits.retain(|exit| {
            exit.clone()
                .validate(&state, ctx.spec)
                .map_err(|e| {
                    warn!(
                        err = ?e,
                        block_slot = %state.slot(),
                        ?exit,
                        "Attempted to include an invalid voluntary exit"
                    );
                })
                .is_ok()
        });
    }

    let slot = state.slot();

    let sync_aggregate = if matches!(&state, BeaconState::Base(_)) {
        None
    } else {
        let sync_aggregate = ctx
            .op_pool
            .get_sync_aggregate(&state)
            .map_err(BlockProductionError::OpPoolError)?
            .unwrap_or_else(|| {
                warn!(
                    slot = %state.slot(),
                    "Producing block with no sync contributions"
                );
                SyncAggregate::new()
            });
        Some(sync_aggregate)
    };

    Ok(PartialBeaconBlock {
        state,
        slot,
        proposer_index,
        parent_root,
        randao_reveal,
        eth1_data,
        graffiti,
        proposer_slashings,
        attester_slashings,
        attestations,
        deposits,
        voluntary_exits,
        sync_aggregate,
        prepare_payload_handle,
        bls_to_execution_changes,
    })
}

/// Payload integration and block completion.
///
/// The `chain` parameter is needed only for `compute_beacon_block_reward` which
/// uses `self.state_at_slot` in the Phase0 case. All other deps go through `ctx`.
#[instrument(skip_all, level = "debug")]
pub(crate) fn complete_partial_beacon_block<
    T: BeaconChainTypes,
    Payload: AbstractExecPayload<T::EthSpec>,
>(
    ctx: &BlockProductionContext<'_, T>,
    chain: &BeaconChain<T>,
    partial_beacon_block: PartialBeaconBlock<T::EthSpec>,
    block_contents: Option<BlockProposalContents<T::EthSpec, Payload>>,
    verification: ProduceBlockVerification,
) -> Result<BeaconBlockResponse<T::EthSpec, Payload>, BlockProductionError> {
    let PartialBeaconBlock {
        mut state,
        slot,
        proposer_index,
        parent_root,
        randao_reveal,
        eth1_data,
        graffiti,
        proposer_slashings,
        attester_slashings,
        attestations,
        deposits,
        voluntary_exits,
        sync_aggregate,
        // We don't need the prepare payload handle since the `execution_payload` is passed into
        // this function. We can assume that the handle has already been consumed in order to
        // produce said `execution_payload`.
        prepare_payload_handle: _,
        bls_to_execution_changes,
    } = partial_beacon_block;

    let (attester_slashings_base, attester_slashings_electra) =
        attester_slashings.into_iter().fold(
            (Vec::new(), Vec::new()),
            |(mut base, mut electra), slashing| {
                match slashing {
                    AttesterSlashing::Base(slashing) => base.push(slashing),
                    AttesterSlashing::Electra(slashing) => electra.push(slashing),
                }
                (base, electra)
            },
        );
    let (attestations_base, attestations_electra) = attestations.into_iter().fold(
        (Vec::new(), Vec::new()),
        |(mut base, mut electra), attestation| {
            match attestation {
                Attestation::Base(attestation) => base.push(attestation),
                Attestation::Electra(attestation) => electra.push(attestation),
            }
            (base, electra)
        },
    );

    let (inner_block, maybe_blobs_and_proofs, execution_payload_value) = match &state {
        BeaconState::Base(_) => (
            BeaconBlock::Base(BeaconBlockBase {
                slot,
                proposer_index,
                parent_root,
                state_root: Hash256::zero(),
                body: BeaconBlockBodyBase {
                    randao_reveal,
                    eth1_data,
                    graffiti,
                    proposer_slashings: proposer_slashings
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    attester_slashings: attester_slashings_base
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    attestations: attestations_base
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    deposits: deposits
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    voluntary_exits: voluntary_exits
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    _phantom: PhantomData,
                },
            }),
            None,
            Uint256::ZERO,
        ),
        BeaconState::Altair(_) => (
            BeaconBlock::Altair(BeaconBlockAltair {
                slot,
                proposer_index,
                parent_root,
                state_root: Hash256::zero(),
                body: BeaconBlockBodyAltair {
                    randao_reveal,
                    eth1_data,
                    graffiti,
                    proposer_slashings: proposer_slashings
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    attester_slashings: attester_slashings_base
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    attestations: attestations_base
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    deposits: deposits
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    voluntary_exits: voluntary_exits
                        .try_into()
                        .map_err(BlockProductionError::SszTypesError)?,
                    sync_aggregate: sync_aggregate
                        .ok_or(BlockProductionError::MissingSyncAggregate)?,
                    _phantom: PhantomData,
                },
            }),
            None,
            Uint256::ZERO,
        ),
        BeaconState::Bellatrix(_) => {
            let block_proposal_contents =
                block_contents.ok_or(BlockProductionError::MissingExecutionPayload)?;
            let execution_payload_value = block_proposal_contents.block_value().to_owned();
            (
                BeaconBlock::Bellatrix(BeaconBlockBellatrix {
                    slot,
                    proposer_index,
                    parent_root,
                    state_root: Hash256::zero(),
                    body: BeaconBlockBodyBellatrix {
                        randao_reveal,
                        eth1_data,
                        graffiti,
                        proposer_slashings: proposer_slashings
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attester_slashings: attester_slashings_base
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attestations: attestations_base
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        deposits: deposits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        voluntary_exits: voluntary_exits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        sync_aggregate: sync_aggregate
                            .ok_or(BlockProductionError::MissingSyncAggregate)?,
                        execution_payload: block_proposal_contents
                            .to_payload()
                            .try_into()
                            .map_err(|_| BlockProductionError::InvalidPayloadFork)?,
                    },
                }),
                None,
                execution_payload_value,
            )
        }
        BeaconState::Capella(_) => {
            let block_proposal_contents =
                block_contents.ok_or(BlockProductionError::MissingExecutionPayload)?;
            let execution_payload_value = block_proposal_contents.block_value().to_owned();

            (
                BeaconBlock::Capella(BeaconBlockCapella {
                    slot,
                    proposer_index,
                    parent_root,
                    state_root: Hash256::zero(),
                    body: BeaconBlockBodyCapella {
                        randao_reveal,
                        eth1_data,
                        graffiti,
                        proposer_slashings: proposer_slashings
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attester_slashings: attester_slashings_base
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attestations: attestations_base
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        deposits: deposits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        voluntary_exits: voluntary_exits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        sync_aggregate: sync_aggregate
                            .ok_or(BlockProductionError::MissingSyncAggregate)?,
                        execution_payload: block_proposal_contents
                            .to_payload()
                            .try_into()
                            .map_err(|_| BlockProductionError::InvalidPayloadFork)?,
                        bls_to_execution_changes: bls_to_execution_changes
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                    },
                }),
                None,
                execution_payload_value,
            )
        }
        BeaconState::Deneb(_) => {
            let (
                payload,
                kzg_commitments,
                maybe_blobs_and_proofs,
                _maybe_requests,
                execution_payload_value,
            ) = block_contents
                .ok_or(BlockProductionError::MissingExecutionPayload)?
                .deconstruct();

            (
                BeaconBlock::Deneb(BeaconBlockDeneb {
                    slot,
                    proposer_index,
                    parent_root,
                    state_root: Hash256::zero(),
                    body: BeaconBlockBodyDeneb {
                        randao_reveal,
                        eth1_data,
                        graffiti,
                        proposer_slashings: proposer_slashings
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attester_slashings: attester_slashings_base
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attestations: attestations_base
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        deposits: deposits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        voluntary_exits: voluntary_exits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        sync_aggregate: sync_aggregate
                            .ok_or(BlockProductionError::MissingSyncAggregate)?,
                        execution_payload: payload
                            .try_into()
                            .map_err(|_| BlockProductionError::InvalidPayloadFork)?,
                        bls_to_execution_changes: bls_to_execution_changes
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        blob_kzg_commitments: kzg_commitments.ok_or(
                            BlockProductionError::MissingKzgCommitment(
                                "Kzg commitments missing from block contents".to_string(),
                            ),
                        )?,
                    },
                }),
                maybe_blobs_and_proofs,
                execution_payload_value,
            )
        }
        BeaconState::Electra(_) => {
            let (
                payload,
                kzg_commitments,
                maybe_blobs_and_proofs,
                maybe_requests,
                execution_payload_value,
            ) = block_contents
                .ok_or(BlockProductionError::MissingExecutionPayload)?
                .deconstruct();

            (
                BeaconBlock::Electra(BeaconBlockElectra {
                    slot,
                    proposer_index,
                    parent_root,
                    state_root: Hash256::zero(),
                    body: BeaconBlockBodyElectra {
                        randao_reveal,
                        eth1_data,
                        graffiti,
                        proposer_slashings: proposer_slashings
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attester_slashings: attester_slashings_electra
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attestations: attestations_electra
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        deposits: deposits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        voluntary_exits: voluntary_exits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        sync_aggregate: sync_aggregate
                            .ok_or(BlockProductionError::MissingSyncAggregate)?,
                        execution_payload: payload
                            .try_into()
                            .map_err(|_| BlockProductionError::InvalidPayloadFork)?,
                        bls_to_execution_changes: bls_to_execution_changes
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        blob_kzg_commitments: kzg_commitments
                            .ok_or(BlockProductionError::InvalidPayloadFork)?,
                        execution_requests: maybe_requests
                            .ok_or(BlockProductionError::MissingExecutionRequests)?,
                    },
                }),
                maybe_blobs_and_proofs,
                execution_payload_value,
            )
        }
        BeaconState::Fulu(_) => {
            let (
                payload,
                kzg_commitments,
                maybe_blobs_and_proofs,
                maybe_requests,
                execution_payload_value,
            ) = block_contents
                .ok_or(BlockProductionError::MissingExecutionPayload)?
                .deconstruct();

            (
                BeaconBlock::Fulu(BeaconBlockFulu {
                    slot,
                    proposer_index,
                    parent_root,
                    state_root: Hash256::zero(),
                    body: BeaconBlockBodyFulu {
                        randao_reveal,
                        eth1_data,
                        graffiti,
                        proposer_slashings: proposer_slashings
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attester_slashings: attester_slashings_electra
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        attestations: attestations_electra
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        deposits: deposits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        voluntary_exits: voluntary_exits
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        sync_aggregate: sync_aggregate
                            .ok_or(BlockProductionError::MissingSyncAggregate)?,
                        execution_payload: payload
                            .try_into()
                            .map_err(|_| BlockProductionError::InvalidPayloadFork)?,
                        bls_to_execution_changes: bls_to_execution_changes
                            .try_into()
                            .map_err(BlockProductionError::SszTypesError)?,
                        blob_kzg_commitments: kzg_commitments
                            .ok_or(BlockProductionError::InvalidPayloadFork)?,
                        execution_requests: maybe_requests
                            .ok_or(BlockProductionError::MissingExecutionRequests)?,
                    },
                }),
                maybe_blobs_and_proofs,
                execution_payload_value,
            )
        }
        BeaconState::Gloas(_) => {
            return Err(BlockProductionError::GloasNotImplemented(
                "Attempting to produce gloas beacon block via non gloas code path".to_owned(),
            ));
        }
    };

    let block = SignedBeaconBlock::from_block(
        inner_block,
        // The block is not signed here, that is the task of a validator client.
        Signature::empty(),
    );

    let block_size = block.ssz_bytes_len();
    debug!(%block_size, "Produced block on state");

    metrics::observe(&metrics::BLOCK_SIZE, block_size as f64);

    if block_size > ctx.config.max_network_size {
        return Err(BlockProductionError::BlockTooLarge(block_size));
    }

    let process_timer = metrics::start_timer(&metrics::BLOCK_PRODUCTION_PROCESS_TIMES);
    let signature_strategy = match verification {
        ProduceBlockVerification::VerifyRandao => BlockSignatureStrategy::VerifyRandao,
        ProduceBlockVerification::NoVerification => BlockSignatureStrategy::NoVerification,
    };

    // Use a context without block root or proposer index so that both are checked.
    let mut ctxt = ConsensusContext::new(block.slot());

    let consensus_block_value = crate::beacon_block_reward::compute_beacon_block_reward(
        block.message(),
        &mut state,
        &chain.store,
        &chain.canonical_head,
        &chain.spec,
    )
    .map(|reward| reward.total)
    .unwrap_or(0);

    per_block_processing(
        &mut state,
        &block,
        signature_strategy,
        VerifyBlockRoot::True,
        &mut ctxt,
        ctx.spec,
    )?;
    drop(process_timer);

    let state_root_timer = metrics::start_timer(&metrics::BLOCK_PRODUCTION_STATE_ROOT_TIMES);
    let state_root = state.update_tree_hash_cache()?;
    drop(state_root_timer);

    let (mut block, _) = block.deconstruct();
    *block.state_root_mut() = state_root;

    let blob_items = match maybe_blobs_and_proofs {
        Some((blobs, proofs)) => {
            let expected_kzg_commitments = block.body().blob_kzg_commitments().map_err(|_| {
                BlockProductionError::InvalidBlockVariant(
                    "deneb block does not contain kzg commitments".to_string(),
                )
            })?;

            if expected_kzg_commitments.len() != blobs.len() {
                return Err(BlockProductionError::MissingKzgCommitment(format!(
                    "Missing KZG commitment for slot {}. Expected {}, got: {}",
                    block.slot(),
                    blobs.len(),
                    expected_kzg_commitments.len()
                )));
            }

            Some((proofs, blobs))
        }
        None => None,
    };

    metrics::inc_counter(&metrics::BLOCK_PRODUCTION_SUCCESSES);

    trace!(
        parent = ?block.parent_root(),
        attestations = block.body().attestations_len(),
        slot = %block.slot(),
        "Produced beacon block"
    );

    Ok(BeaconBlockResponse {
        block,
        state,
        blob_items,
        execution_payload_value,
        consensus_block_value,
    })
}
