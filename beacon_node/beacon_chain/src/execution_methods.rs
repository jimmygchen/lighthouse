//! Execution layer integration and fork choice update methods.
//!
//! All methods are free functions. Async methods that need `spawn_blocking` take
//! `Arc<BeaconComponents<T>>` directly. Callers access these via delegation methods
//! on `BeaconComponents` defined elsewhere (e.g., `canonical_head.rs`).

use crate::beacon_components::{
    BeaconChainTypes, INVALID_JUSTIFIED_PAYLOAD_SHUTDOWN_REASON, OverrideForkchoiceUpdate,
    PrePayloadAttributes,
};
use crate::block_production::BlockProductionContext;
use crate::errors::BeaconChainError as Error;
use crate::events::ServerSentEventHandler;
use crate::{BeaconChainError, BeaconComponents};
use eth2::beacon_response::ForkVersionedResponse;
use eth2::types::{EventKind, SseExtendedPayloadAttributes};
use execution_layer::{ExecutionBlockHash, PayloadAttributes, PayloadStatus};
use fork_choice::{ForkchoiceUpdateParameters, InvalidationOperation};
use futures::channel::mpsc::Sender;
use logging::crit;
use slot_clock::SlotClock;
use std::sync::Arc;
use task_executor::ShutdownReason;
use tracing::{debug, error, info, warn};
use types::*;

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Send a shutdown signal when the justified checkpoint is detected as invalid.
///
/// Returns `Err` to halt upstream processing after the shutdown is triggered.
pub(crate) fn handle_invalid_justified_checkpoint(
    shutdown_sender: &mut Sender<ShutdownReason>,
    justified_root: Hash256,
    execution_block_hash: Option<ExecutionBlockHash>,
) -> Result<(), Error> {
    crit!(
        msg = "ensure you are not connected to a malicious network. This error is not \
        recoverable, please reach out to the lighthouse developers for assistance.",
        "The justified checkpoint is invalid"
    );

    if let Err(e) = shutdown_sender.try_send(ShutdownReason::Failure(
        INVALID_JUSTIFIED_PAYLOAD_SHUTDOWN_REASON,
    )) {
        crit!(
            msg = "shut down may already be under way",
            error = ?e,
            "Unable to trigger client shut down"
        );
    }

    Err(Error::JustifiedPayloadInvalid {
        justified_root,
        execution_block_hash,
    })
}

/// Emit a `PayloadAttributes` server-sent event if there are subscribers and
/// the required data is available.
pub(crate) fn emit_payload_attributes_event<E: EthSpec>(
    event_handler: Option<&ServerSentEventHandler<E>>,
    pre_payload_attributes: &PrePayloadAttributes,
    payload_attributes: PayloadAttributes,
    forkchoice_update_params: &ForkchoiceUpdateParameters,
    prepare_slot: Slot,
    proposer: u64,
    spec: &ChainSpec,
) {
    if let Some(event_handler) = event_handler
        && event_handler.has_payload_attributes_subscribers()
        && let Some(parent_block_number) = pre_payload_attributes.parent_block_number
    {
        let head_root = forkchoice_update_params.head_root;
        event_handler.register(EventKind::PayloadAttributes(ForkVersionedResponse {
            data: SseExtendedPayloadAttributes {
                proposal_slot: prepare_slot,
                proposer_index: proposer,
                parent_block_root: head_root,
                parent_block_number,
                parent_block_hash: forkchoice_update_params.head_hash.unwrap_or_default(),
                payload_attributes: payload_attributes.into(),
            },
            metadata: Default::default(),
            version: spec.fork_name_at_slot::<E>(prepare_slot),
        }));
    }
}

pub async fn process_invalid_execution_payload<T: BeaconChainTypes>(
    chain: &Arc<BeaconComponents<T>>,
    op: &InvalidationOperation,
) -> Result<(), Error> {
    debug!(?op, "Processing payload invalidation");

    // Update the execution status in fork choice.
    //
    // Use a blocking task since it interacts with the `canonical_head` lock. Lock contention
    // on the core executor is bad.
    let inner_chain = chain.clone();
    let inner_op = op.clone();
    let fork_choice_result = crate::beacon_components::spawn_blocking_handle(
        &chain.task_executor,
        move || {
            inner_chain
                .canonical_head
                .fork_choice_write_lock()
                .on_invalid_execution_payload(&inner_op)
        },
        "invalid_payload_fork_choice_update",
    )
    .await?;

    // Update fork choice.
    if let Err(e) = fork_choice_result {
        crit!(
            error = ?e,
            latest_valid_ancestor = ?op.latest_valid_ancestor(),
            block_root = ?op.block_root(),
            "Failed to process invalid payload"
        );
    }

    // Run fork choice since it's possible that the payload invalidation might result in a new
    // head.
    crate::canonical_head::recompute_head_at_current_slot(chain).await;

    // Obtain the justified root from fork choice.
    //
    // Use a blocking task since it interacts with the `canonical_head` lock. Lock contention
    // on the core executor is bad.
    let inner_chain = chain.clone();
    let justified_block = crate::beacon_components::spawn_blocking_handle(
        &chain.task_executor,
        move || {
            inner_chain
                .canonical_head
                .fork_choice_read_lock()
                .get_justified_block()
        },
        "invalid_payload_fork_choice_get_justified",
    )
    .await??;

    if justified_block.execution_status.is_invalid() {
        // Delegate to the free function for shutdown signalling.
        let mut shutdown_sender = chain.shutdown_sender.clone();
        return handle_invalid_justified_checkpoint(
            &mut shutdown_sender,
            justified_block.root,
            justified_block.execution_status.block_hash(),
        );
    }

    Ok(())
}

pub fn block_is_known_to_fork_choice<T: BeaconChainTypes>(
    chain: &BeaconComponents<T>,
    root: &Hash256,
) -> bool {
    chain
        .execution_manager
        .block_is_known_to_fork_choice(&chain.canonical_head, root)
}

/// Determines the beacon proposer for the next slot. If that proposer is registered in the
/// `execution_layer`, provide the `execution_layer` with the necessary information to produce
/// `PayloadAttributes` for future calls to fork choice.
///
/// The `PayloadAttributes` are used by the EL to give it a look-ahead for preparing an optimal
/// set of transactions for a new `ExecutionPayload`.
///
/// This function will result in a call to `forkchoiceUpdated` on the EL if we're in the
/// tail-end of the slot (as defined by `config.prepare_payload_lookahead`).
///
/// Return `Ok(Some(head_block_root))` if this node prepared to propose at the next slot on
/// top of `head_block_root`.
pub async fn prepare_beacon_proposer<T: BeaconChainTypes>(
    chain: &Arc<BeaconComponents<T>>,
    current_slot: Slot,
) -> Result<Option<Hash256>, Error> {
    let prepare_slot = current_slot + 1;

    // There's no need to run the proposer preparation routine before the bellatrix fork.
    if chain
        .execution_manager
        .slot_is_prior_to_bellatrix(prepare_slot)
    {
        return Ok(None);
    }

    let execution_layer = chain
        .execution_layer
        .clone()
        .ok_or(Error::ExecutionLayerMissing)?;

    // Nothing to do if there are no proposers registered with the EL, exit early to avoid
    // wasting cycles.
    if !chain.config.always_prepare_payload
        && !execution_layer.has_any_proposer_preparation_data().await
    {
        return Ok(None);
    }

    // Load the cached head and its forkchoice update parameters.
    //
    // Use a blocking task since blocking the core executor on the canonical head read lock can
    // block the core tokio executor.
    let inner_chain = chain.clone();
    let tolerance_slots = chain.config.sync_tolerance_epochs * T::EthSpec::slots_per_epoch();
    let maybe_prep_data = crate::beacon_components::spawn_blocking_handle(
        &chain.task_executor,
        move || {
            let cached_head = inner_chain.canonical_head.cached_head();

            // Don't bother with proposer prep if the head is more than
            // `sync_tolerance_epochs` prior to the current slot.
            //
            // This prevents the routine from running during sync.
            let head_slot = cached_head.head_slot();
            if head_slot + tolerance_slots < current_slot {
                debug!(%head_slot, %current_slot, "Head too old for proposer prep");
                return Ok(None);
            }

            let canonical_fcu_params = cached_head.forkchoice_update_parameters();
            let ctx = BlockProductionContext {
                canonical_head: &inner_chain.canonical_head,
                store: &inner_chain.store,
                attestation_manager: &inner_chain.attestation_manager,
                execution_manager: &inner_chain.execution_manager,
                execution_layer: inner_chain.execution_layer.as_ref(),
                op_pool: &inner_chain.op_pool,
                spec: &inner_chain.spec,
                slot_clock: &inner_chain.slot_clock,
                config: &inner_chain.config,
                block_times_cache: &inner_chain.block_times_cache,
                beacon_proposer_cache: &inner_chain.beacon_proposer_cache,
                genesis_block_root: inner_chain.genesis_block_root,
            };
            let fcu_params = crate::block_production::overridden_forkchoice_update_params_fn(
                &ctx,
                canonical_fcu_params,
            )?;
            let pre_payload_attributes = crate::block_production::get_pre_payload_attributes(
                &ctx,
                prepare_slot,
                fcu_params.head_root,
                &cached_head,
            )?;
            Ok::<_, Error>(Some((fcu_params, pre_payload_attributes)))
        },
        "prepare_beacon_proposer_head_read",
    )
    .await??;

    let Some((forkchoice_update_params, Some(pre_payload_attributes))) = maybe_prep_data else {
        // Appropriate log messages have already been logged above and in
        // `get_pre_payload_attributes`.
        return Ok(None);
    };

    // If the execution layer doesn't have any proposer data for this validator then we assume
    // it's not connected to this BN and no action is required.
    let proposer = pre_payload_attributes.proposer_index;
    if !chain.config.always_prepare_payload
        && !execution_layer
            .has_proposer_preparation_data(proposer)
            .await
    {
        return Ok(None);
    }

    // Fetch payload attributes from the execution layer's cache, or compute them from scratch
    // if no matching entry is found. This saves recomputing the withdrawals which can take
    // considerable time to compute if a state load is required.
    let head_root = forkchoice_update_params.head_root;
    let payload_attributes = if let Some(payload_attributes) = execution_layer
        .payload_attributes(prepare_slot, head_root)
        .await
    {
        payload_attributes
    } else {
        let prepare_slot_fork = chain.spec.fork_name_at_slot::<T::EthSpec>(prepare_slot);

        let withdrawals = if prepare_slot_fork.capella_enabled() {
            let inner_chain = chain.clone();
            crate::beacon_components::spawn_blocking_handle(
                &chain.task_executor,
                move || {
                    let ctx = BlockProductionContext {
                        canonical_head: &inner_chain.canonical_head,
                        store: &inner_chain.store,
                        attestation_manager: &inner_chain.attestation_manager,
                        execution_manager: &inner_chain.execution_manager,
                        execution_layer: inner_chain.execution_layer.as_ref(),
                        op_pool: &inner_chain.op_pool,
                        spec: &inner_chain.spec,
                        slot_clock: &inner_chain.slot_clock,
                        config: &inner_chain.config,
                        block_times_cache: &inner_chain.block_times_cache,
                        beacon_proposer_cache: &inner_chain.beacon_proposer_cache,
                        genesis_block_root: inner_chain.genesis_block_root,
                    };
                    crate::block_production::get_expected_withdrawals_for_proposal(
                        &ctx,
                        &forkchoice_update_params,
                        prepare_slot,
                    )
                },
                "prepare_beacon_proposer_withdrawals",
            )
            .await?
            .map(Some)?
        } else {
            None
        };

        let parent_beacon_block_root = if prepare_slot_fork.deneb_enabled() {
            Some(pre_payload_attributes.parent_beacon_block_root)
        } else {
            None
        };

        let payload_attributes = PayloadAttributes::new(
            chain
                .slot_clock
                .start_of(prepare_slot)
                .ok_or(Error::InvalidSlot(prepare_slot))?
                .as_secs(),
            pre_payload_attributes.prev_randao,
            execution_layer.get_suggested_fee_recipient(proposer).await,
            withdrawals.map(Into::into),
            parent_beacon_block_root,
        );

        execution_layer
            .insert_proposer(
                prepare_slot,
                head_root,
                proposer,
                payload_attributes.clone(),
            )
            .await;

        // Only push a log to the user if this is the first time we've seen this proposer for
        // this slot.
        info!(
            %prepare_slot,
            validator = proposer,
            parent_root = ?head_root,
            "Prepared beacon proposer"
        );
        payload_attributes
    };

    // Push a server-sent event (probably to a block builder or relay).
    emit_payload_attributes_event(
        chain.event_handler.as_ref(),
        &pre_payload_attributes,
        payload_attributes,
        &forkchoice_update_params,
        prepare_slot,
        proposer,
        &chain.spec,
    );

    let Some(till_prepare_slot) = chain.slot_clock.duration_to_slot(prepare_slot) else {
        // `SlotClock::duration_to_slot` will return `None` when we are past the start
        // of `prepare_slot`. Don't bother sending a `forkchoiceUpdated` in that case,
        // it's too late.
        //
        // This scenario might occur on an overloaded/under-resourced node.
        warn!(
            %prepare_slot,
            validator = proposer,
            "Delayed proposer preparation"
        );
        return Ok(None);
    };

    // If we are close enough to the proposal slot, send an fcU, which will have payload
    // attributes filled in by the execution layer cache we just primed.
    if chain.config.always_prepare_payload
        || till_prepare_slot <= chain.config.prepare_payload_lookahead
    {
        debug!(
            ?till_prepare_slot,
            %prepare_slot,
            "Sending forkchoiceUpdate for proposer prep"
        );

        update_execution_engine_forkchoice(
            chain,
            current_slot,
            forkchoice_update_params,
            OverrideForkchoiceUpdate::AlreadyApplied,
        )
        .await?;
    }

    Ok(Some(head_root))
}

pub async fn update_execution_engine_forkchoice<T: BeaconChainTypes>(
    chain: &Arc<BeaconComponents<T>>,
    current_slot: Slot,
    input_params: ForkchoiceUpdateParameters,
    override_forkchoice_update: OverrideForkchoiceUpdate,
) -> Result<(), Error> {
    let execution_layer = chain
        .execution_layer
        .as_ref()
        .ok_or(Error::ExecutionLayerMissing)?;

    // Determine whether to override the forkchoiceUpdated message if we want to re-org
    // the current head at the next slot.
    let params = if override_forkchoice_update == OverrideForkchoiceUpdate::Yes {
        let inner_chain = chain.clone();
        crate::beacon_components::spawn_blocking_handle(
            &chain.task_executor,
            move || {
                let ctx = BlockProductionContext {
                    canonical_head: &inner_chain.canonical_head,
                    store: &inner_chain.store,
                    attestation_manager: &inner_chain.attestation_manager,
                    execution_manager: &inner_chain.execution_manager,
                    execution_layer: inner_chain.execution_layer.as_ref(),
                    op_pool: &inner_chain.op_pool,
                    spec: &inner_chain.spec,
                    slot_clock: &inner_chain.slot_clock,
                    config: &inner_chain.config,
                    block_times_cache: &inner_chain.block_times_cache,
                    beacon_proposer_cache: &inner_chain.beacon_proposer_cache,
                    genesis_block_root: inner_chain.genesis_block_root,
                };
                crate::block_production::overridden_forkchoice_update_params_fn(&ctx, input_params)
            },
            "update_execution_engine_forkchoice_override",
        )
        .await??
    } else {
        input_params
    };

    // Take the global lock for updating the execution engine fork choice.
    //
    // Whilst holding this lock we must:
    //
    // 1. Read the canonical head.
    // 2. Issue a forkchoiceUpdated call to the execution engine.
    //
    // This will allow us to ensure that we provide the execution layer with an *ordered* view
    // of the head. I.e., we will never communicate a past head after communicating a later
    // one.
    //
    // There is a "deadlock warning" in this function. The downside of this nice ordering is the
    // potential for deadlock. I would advise against any other use of
    // `execution_engine_forkchoice_lock` apart from the one here.
    let forkchoice_lock = execution_layer.execution_engine_forkchoice_lock().await;

    let (head_block_root, head_hash, justified_hash, finalized_hash) =
        if let Some(head_hash) = params.head_hash {
            (
                params.head_root,
                head_hash,
                params
                    .justified_hash
                    .unwrap_or_else(ExecutionBlockHash::zero),
                params
                    .finalized_hash
                    .unwrap_or_else(ExecutionBlockHash::zero),
            )
        } else {
            // Proposing the block for the merge is no longer supported.
            return Ok(());
        };

    let forkchoice_updated_response = execution_layer
        .notify_forkchoice_updated(
            head_hash,
            justified_hash,
            finalized_hash,
            current_slot,
            head_block_root,
        )
        .await
        .map_err(Error::ExecutionForkChoiceUpdateFailed);

    // The head has been read and the execution layer has been updated. It is now valid to send
    // another fork choice update.
    drop(forkchoice_lock);

    match forkchoice_updated_response {
        Ok(status) => match status {
            PayloadStatus::Valid => {
                // Ensure that fork choice knows that the block is no longer optimistic.
                let inner_chain = chain.clone();
                let fork_choice_update_result = crate::beacon_components::spawn_blocking_handle(
                    &chain.task_executor,
                    move || {
                        inner_chain
                            .canonical_head
                            .fork_choice_write_lock()
                            .on_valid_execution_payload(head_block_root)
                    },
                    "update_execution_engine_valid_payload",
                )
                .await?;
                if let Err(e) = fork_choice_update_result {
                    error!(
                        error= ?e,
                        "Failed to validate payload"
                    )
                };
                Ok(())
            }
            // There's nothing to be done for a syncing response. If the block is already
            // `SYNCING` in fork choice, there's nothing to do. If already known to be `VALID`
            // or `INVALID` then we don't want to change it to syncing.
            PayloadStatus::Syncing => Ok(()),
            // The specification doesn't list `ACCEPTED` as a valid response to a fork choice
            // update. This response *seems* innocent enough, so we won't return early with an
            // error. However, we create a log to bring attention to the issue.
            PayloadStatus::Accepted => {
                warn!(
                    msg = "execution engine provided an unexpected response to a fork \
                    choice update. although this is not a serious issue, please raise \
                    an issue.",
                    "Fork choice update received ACCEPTED"
                );
                Ok(())
            }
            PayloadStatus::Invalid {
                latest_valid_hash,
                ref validation_error,
            } => {
                warn!(
                    ?validation_error,
                    ?latest_valid_hash,
                    ?head_hash,
                    head_block_root = ?head_block_root,
                    method = "fcU",
                    "Invalid execution payload"
                );

                match latest_valid_hash {
                    // The `latest_valid_hash` is set to `None` when the EE
                    // "cannot determine the ancestor of the invalid
                    // payload". In such a scenario we should only
                    // invalidate the head block and nothing else.
                    None => {
                        process_invalid_execution_payload(
                            chain,
                            &InvalidationOperation::InvalidateOne {
                                block_root: head_block_root,
                            },
                        )
                        .await?;
                    }
                    // An all-zeros execution block hash implies that
                    // the terminal block was invalid. We are being
                    // explicit in invalidating only the head block in
                    // this case.
                    Some(hash) if hash == ExecutionBlockHash::zero() => {
                        process_invalid_execution_payload(
                            chain,
                            &InvalidationOperation::InvalidateOne {
                                block_root: head_block_root,
                            },
                        )
                        .await?;
                    }
                    // The execution engine has stated that all blocks between the
                    // `head_execution_block_hash` and `latest_valid_hash` are invalid.
                    Some(latest_valid_hash) => {
                        process_invalid_execution_payload(
                            chain,
                            &InvalidationOperation::InvalidateMany {
                                head_block_root,
                                always_invalidate_head: true,
                                latest_valid_ancestor: latest_valid_hash,
                            },
                        )
                        .await?;
                    }
                }

                Err(BeaconChainError::ExecutionForkChoiceUpdateInvalid { status })
            }
            PayloadStatus::InvalidBlockHash {
                ref validation_error,
            } => {
                warn!(
                    ?validation_error,
                    ?head_hash,
                    ?head_block_root,
                    method = "fcU",
                    "Invalid execution payload block hash"
                );
                // The execution engine has stated that the head block is invalid, however it
                // hasn't returned a latest valid ancestor.
                //
                // Using a `None` latest valid ancestor will result in only the head block
                // being invalidated (no ancestors).
                process_invalid_execution_payload(
                    chain,
                    &InvalidationOperation::InvalidateOne {
                        block_root: head_block_root,
                    },
                )
                .await?;

                Err(BeaconChainError::ExecutionForkChoiceUpdateInvalid { status })
            }
        },
        Err(e) => Err(e),
    }
}

/// Returns the value of `execution_optimistic` for `block`.
pub fn is_optimistic_or_invalid_block<
    T: BeaconChainTypes,
    Payload: AbstractExecPayload<T::EthSpec>,
>(
    chain: &BeaconComponents<T>,
    block: &SignedBeaconBlock<T::EthSpec, Payload>,
) -> Result<bool, BeaconChainError> {
    chain
        .execution_manager
        .is_optimistic_or_invalid_block(&chain.canonical_head, block)
}

pub fn is_optimistic_or_invalid_head_block<
    T: BeaconChainTypes,
    Payload: AbstractExecPayload<T::EthSpec>,
>(
    chain: &BeaconComponents<T>,
    head_block: &SignedBeaconBlock<T::EthSpec, Payload>,
) -> Result<bool, BeaconChainError> {
    chain
        .execution_manager
        .is_optimistic_or_invalid_head_block(&chain.canonical_head, head_block)
}

pub fn is_optimistic_or_invalid_head<T: BeaconChainTypes>(
    chain: &BeaconComponents<T>,
) -> Result<bool, BeaconChainError> {
    chain
        .execution_manager
        .is_optimistic_or_invalid_head(&chain.canonical_head)
}
