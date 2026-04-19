use crate::task_spawner::{Priority, TaskSpawner};
use crate::utils::{NetworkTxFilter, OptionalConsensusVersionHeaderFilter, ResponseFilter};
use crate::version::{
    ResponseIncludesVersion, V1, V2, add_consensus_version_header, beacon_response,
    unsupported_version_rejection,
};
use crate::{sync_committees, utils};
use beacon_chain::events::EventKind;
use beacon_chain::observed_operations::ObservationOutcome;
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::{AttestationPoolQuery, EndpointVersion, Failure, GenericResponse};
use lighthouse_network::PubsubMessage;
use network::NetworkMessage;
use operation_pool::ReceivedPreCapella;
use slot_clock::SlotClock;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};
use types::{
    Attestation, AttestationData, AttesterSlashing, ForkName, ProposerSlashing,
    SignedBlsToExecutionChange, SignedVoluntaryExit, SingleAttestation, SyncCommitteeMessage,
};
use warp::filters::BoxedFilter;
use warp::{Filter, Reply};
use warp_utils::reject::convert_rejection;

pub type BeaconPoolPathFilter<T> = BoxedFilter<(
    TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    Arc<BeaconChain<T>>,
)>;
pub type BeaconPoolPathV2Filter<T> = BoxedFilter<(
    TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    Arc<BeaconChain<T>>,
)>;
pub type BeaconPoolPathAnyFilter<T> = BoxedFilter<(
    EndpointVersion,
    TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    Arc<BeaconChain<T>>,
)>;

/// POST beacon/pool/bls_to_execution_changes
pub fn post_beacon_pool_bls_to_execution_changes<T: BeaconChainTypes>(
    network_tx_filter: &NetworkTxFilter<T>,
    beacon_pool_path: &BeaconPoolPathFilter<T>,
) -> ResponseFilter {
    beacon_pool_path
        .clone()
        .and(warp::path("bls_to_execution_changes"))
        .and(warp::path::end())
        .and(warp_utils::json::json())
        .and(network_tx_filter.clone())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>,
             address_changes: Vec<SignedBlsToExecutionChange>,
             network_tx: UnboundedSender<NetworkMessage<T::EthSpec>>| {
                task_spawner.blocking_json_task(Priority::P0, move || {
                    let mut failures = vec![];

                    // Fetch head state once for all verifications (caller DI pattern).
                    let head = chain.canonical_head.cached_head();
                    let head_state = &head.snapshot.beacon_state;

                    for (index, address_change) in address_changes.into_iter().enumerate() {
                        let validator_index = address_change.message.validator_index;

                        // Check op pool for conflicts before checking the gossip duplicate filter.
                        let pool_result = chain
                            .operations
                            .op_pool
                            .bls_to_execution_change_in_pool_equals(&address_change);
                        let verify_result = match pool_result {
                            Some(true) => Ok(ObservationOutcome::AlreadyKnown),
                            Some(false) => {
                                Err(beacon_chain::BeaconChainError::BlsToExecutionConflictsWithPool)
                            }
                            None => chain
                                .operations
                                .observed_bls_to_execution_changes
                                .lock()
                                .verify_and_observe(address_change, head_state, &chain.spec)
                                .map_err(Into::into),
                        };

                        match verify_result {
                            Ok(ObservationOutcome::New(verified_address_change)) => {
                                let validator_index =
                                    verified_address_change.as_inner().message.validator_index;
                                let address = verified_address_change
                                    .as_inner()
                                    .message
                                    .to_execution_address;

                                // New to P2P *and* op pool, gossip immediately if post-Capella.
                                let current_fork = chain.spec.fork_name_at_slot::<T::EthSpec>(
                                    beacon_chain::state_query::current_slot(&chain.slot_clock)
                                        .unwrap_or_default(),
                                );
                                let received_pre_capella = if current_fork.capella_enabled() {
                                    ReceivedPreCapella::No
                                } else {
                                    ReceivedPreCapella::Yes
                                };
                                if matches!(received_pre_capella, ReceivedPreCapella::No) {
                                    utils::publish_pubsub_message(
                                        &network_tx,
                                        PubsubMessage::BlsToExecutionChange(Box::new(
                                            verified_address_change.as_inner().clone(),
                                        )),
                                    )?;
                                }

                                // Fire SSE event.
                                if let Some(event_handler) =
                                    chain.block_importer.event_handler.as_ref()
                                    && event_handler.has_bls_to_execution_change_subscribers()
                                {
                                    event_handler.register(EventKind::BlsToExecutionChange(
                                        Box::new(verified_address_change.clone().into_inner()),
                                    ));
                                }

                                // Import to op pool (may return `false` if there's a race).
                                let imported =
                                    chain.operations.op_pool.insert_bls_to_execution_change(
                                        verified_address_change,
                                        received_pre_capella,
                                    );

                                info!(
                                    %validator_index,
                                    ?address,
                                    published =
                                        matches!(received_pre_capella, ReceivedPreCapella::No),
                                    imported,
                                    "Processed BLS to execution change"
                                );
                            }
                            Ok(ObservationOutcome::AlreadyKnown) => {
                                debug!(%validator_index, "BLS to execution change already known");
                            }
                            Err(e) => {
                                warn!(
                                    validator_index,
                                    reason = ?e,
                                    source = "HTTP",
                                    "Invalid BLS to execution change"
                                );
                                failures.push(Failure::new(index, format!("invalid: {e:?}")));
                            }
                        }
                    }

                    if failures.is_empty() {
                        Ok(())
                    } else {
                        Err(warp_utils::reject::indexed_bad_request(
                            "some BLS to execution changes failed to verify".into(),
                            failures,
                        ))
                    }
                })
            },
        )
        .boxed()
}

/// GET beacon/pool/bls_to_execution_changes
pub fn get_beacon_pool_bls_to_execution_changes<T: BeaconChainTypes>(
    beacon_pool_path: &BeaconPoolPathFilter<T>,
) -> ResponseFilter {
    beacon_pool_path
        .clone()
        .and(warp::path("bls_to_execution_changes"))
        .and(warp::path::end())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>, chain: Arc<BeaconChain<T>>| {
                task_spawner.blocking_json_task(Priority::P1, move || {
                    let address_changes =
                        chain.operations.op_pool.get_all_bls_to_execution_changes();
                    Ok(GenericResponse::from(address_changes))
                })
            },
        )
        .boxed()
}

/// POST beacon/pool/sync_committees
pub fn post_beacon_pool_sync_committees<T: BeaconChainTypes>(
    network_tx_filter: &NetworkTxFilter<T>,
    beacon_pool_path: &BeaconPoolPathFilter<T>,
) -> ResponseFilter {
    beacon_pool_path
        .clone()
        .and(warp::path("sync_committees"))
        .and(warp::path::end())
        .and(warp_utils::json::json())
        .and(network_tx_filter.clone())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>,
             signatures: Vec<SyncCommitteeMessage>,
             network_tx: UnboundedSender<NetworkMessage<T::EthSpec>>| {
                task_spawner.blocking_json_task(Priority::P0, move || {
                    sync_committees::process_sync_committee_signatures(
                        signatures, network_tx, &chain,
                    )?;
                    Ok(GenericResponse::from(()))
                })
            },
        )
        .boxed()
}

/// GET beacon/pool/voluntary_exits
pub fn get_beacon_pool_voluntary_exits<T: BeaconChainTypes>(
    beacon_pool_path: &BeaconPoolPathFilter<T>,
) -> ResponseFilter {
    beacon_pool_path
        .clone()
        .and(warp::path("voluntary_exits"))
        .and(warp::path::end())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>, chain: Arc<BeaconChain<T>>| {
                task_spawner.blocking_json_task(Priority::P1, move || {
                    let attestations = chain.operations.op_pool.get_all_voluntary_exits();
                    Ok(GenericResponse::from(attestations))
                })
            },
        )
        .boxed()
}

/// POST beacon/pool/voluntary_exits
pub fn post_beacon_pool_voluntary_exits<T: BeaconChainTypes>(
    network_tx_filter: &NetworkTxFilter<T>,
    beacon_pool_path: &BeaconPoolPathFilter<T>,
) -> ResponseFilter {
    beacon_pool_path
        .clone()
        .and(warp::path("voluntary_exits"))
        .and(warp::path::end())
        .and(warp_utils::json::json())
        .and(network_tx_filter.clone())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>,
             exit: SignedVoluntaryExit,
             network_tx: UnboundedSender<NetworkMessage<T::EthSpec>>| {
                task_spawner.blocking_json_task(Priority::P0, move || {
                    // Fetch state and epoch for verification (caller DI pattern).
                    let head = chain.canonical_head.cached_head();
                    let head_state = &head.snapshot.beacon_state;
                    let wall_clock_epoch =
                        beacon_chain::state_query::current_epoch::<T::EthSpec, _>(
                            &chain.slot_clock,
                        )
                        .map_err(|e| {
                            warp_utils::reject::object_invalid(format!(
                                "gossip verification failed: {:?}",
                                e
                            ))
                        })?;

                    let outcome = chain
                        .operations
                        .observed_voluntary_exits
                        .lock()
                        .verify_and_observe_at(
                            exit.clone(),
                            wall_clock_epoch,
                            head_state,
                            &chain.spec,
                        )
                        .map_err(|e| {
                            warp_utils::reject::object_invalid(format!(
                                "gossip verification failed: {:?}",
                                e
                            ))
                        })?;

                    // Notify the validator monitor.
                    chain
                        .block_importer
                        .validator_monitor
                        .read()
                        .register_api_voluntary_exit(&exit.message);

                    if let ObservationOutcome::New(ref verified_exit) = outcome {
                        // Fire SSE event.
                        if let Some(event_handler) = chain.block_importer.event_handler.as_ref()
                            && event_handler.has_exit_subscribers()
                        {
                            event_handler.register(EventKind::VoluntaryExit(
                                verified_exit.as_inner().clone(),
                            ));
                        }

                        utils::publish_pubsub_message(
                            &network_tx,
                            PubsubMessage::VoluntaryExit(Box::new(
                                verified_exit.clone().into_inner(),
                            )),
                        )?;

                        chain
                            .operations
                            .op_pool
                            .insert_voluntary_exit(verified_exit.clone());
                    }

                    Ok(())
                })
            },
        )
        .boxed()
}

/// GET beacon/pool/proposer_slashings
pub fn get_beacon_pool_proposer_slashings<T: BeaconChainTypes>(
    beacon_pool_path: &BeaconPoolPathFilter<T>,
) -> ResponseFilter {
    beacon_pool_path
        .clone()
        .and(warp::path("proposer_slashings"))
        .and(warp::path::end())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>, chain: Arc<BeaconChain<T>>| {
                task_spawner.blocking_json_task(Priority::P1, move || {
                    let attestations = chain.operations.op_pool.get_all_proposer_slashings();
                    Ok(GenericResponse::from(attestations))
                })
            },
        )
        .boxed()
}

/// POST beacon/pool/proposer_slashings
pub fn post_beacon_pool_proposer_slashings<T: BeaconChainTypes>(
    network_tx_filter: &NetworkTxFilter<T>,
    beacon_pool_path: &BeaconPoolPathFilter<T>,
) -> ResponseFilter {
    beacon_pool_path
        .clone()
        .and(warp::path("proposer_slashings"))
        .and(warp::path::end())
        .and(warp_utils::json::json())
        .and(network_tx_filter.clone())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>,
             slashing: ProposerSlashing,
             network_tx: UnboundedSender<NetworkMessage<T::EthSpec>>| {
                task_spawner.blocking_json_task(Priority::P0, move || {
                    // Fetch wall clock state for verification (caller DI pattern).
                    let wall_clock_state = beacon_chain::state_query::wall_clock_state(
                        &chain.store,
                        &chain.canonical_head,
                        &chain.spec,
                        &chain.slot_clock,
                    )
                    .map_err(|e| {
                        warp_utils::reject::object_invalid(format!(
                            "gossip verification failed: {:?}",
                            e
                        ))
                    })?;

                    let outcome = chain
                        .operations
                        .observed_proposer_slashings
                        .lock()
                        .verify_and_observe(slashing.clone(), &wall_clock_state, &chain.spec)
                        .map_err(|e| {
                            warp_utils::reject::object_invalid(format!(
                                "gossip verification failed: {:?}",
                                e
                            ))
                        })?;

                    // Notify the validator monitor.
                    chain
                        .block_importer
                        .validator_monitor
                        .read()
                        .register_api_proposer_slashing(&slashing);

                    if let ObservationOutcome::New(verified_slashing) = outcome {
                        // Fire SSE event.
                        if let Some(event_handler) = chain.block_importer.event_handler.as_ref()
                            && event_handler.has_proposer_slashing_subscribers()
                        {
                            event_handler.register(EventKind::ProposerSlashing(Box::new(
                                verified_slashing.clone().into_inner(),
                            )));
                        }

                        utils::publish_pubsub_message(
                            &network_tx,
                            PubsubMessage::ProposerSlashing(Box::new(
                                verified_slashing.clone().into_inner(),
                            )),
                        )?;

                        chain
                            .operations
                            .op_pool
                            .insert_proposer_slashing(verified_slashing);
                    }

                    Ok(())
                })
            },
        )
        .boxed()
}

/// GET beacon/pool/attester_slashings
pub fn get_beacon_pool_attester_slashings<T: BeaconChainTypes>(
    beacon_pool_path_any: &BeaconPoolPathAnyFilter<T>,
) -> ResponseFilter {
    beacon_pool_path_any
        .clone()
        .and(warp::path("attester_slashings"))
        .and(warp::path::end())
        .then(
            |endpoint_version: EndpointVersion,
             task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>| {
                task_spawner.blocking_response_task(Priority::P1, move || {
                    let slashings = chain.operations.op_pool.get_all_attester_slashings();

                    // Use the current slot to find the fork version, and convert all messages to the
                    // current fork's format. This is to ensure consistent message types matching
                    // `Eth-Consensus-Version`.
                    let current_slot =
                        chain
                            .slot_clock
                            .now()
                            .ok_or(warp_utils::reject::custom_server_error(
                                "unable to read slot clock".to_string(),
                            ))?;
                    let fork_name = chain.spec.fork_name_at_slot::<T::EthSpec>(current_slot);
                    let slashings = slashings
                        .into_iter()
                        .filter(|slashing| {
                            (fork_name.electra_enabled()
                                && matches!(slashing, AttesterSlashing::Electra(_)))
                                || (!fork_name.electra_enabled()
                                    && matches!(slashing, AttesterSlashing::Base(_)))
                        })
                        .collect::<Vec<_>>();

                    let require_version = match endpoint_version {
                        V1 => ResponseIncludesVersion::No,
                        V2 => ResponseIncludesVersion::Yes(fork_name),
                        _ => return Err(unsupported_version_rejection(endpoint_version)),
                    };

                    let res = beacon_response(require_version, &slashings);
                    Ok(add_consensus_version_header(
                        warp::reply::json(&res).into_response(),
                        fork_name,
                    ))
                })
            },
        )
        .boxed()
}

// POST beacon/pool/attester_slashings
pub fn post_beacon_pool_attester_slashings<T: BeaconChainTypes>(
    network_tx_filter: &NetworkTxFilter<T>,
    beacon_pool_path_any: &BeaconPoolPathAnyFilter<T>,
) -> ResponseFilter {
    beacon_pool_path_any
        .clone()
        .and(warp::path("attester_slashings"))
        .and(warp::path::end())
        .and(warp_utils::json::json())
        .and(network_tx_filter.clone())
        .then(
            // V1 and V2 are identical except V2 has a consensus version header in the request.
            // We only require this header for SSZ deserialization, which isn't supported for
            // this endpoint presently.
            |_endpoint_version: EndpointVersion,
             task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>,
             slashing: AttesterSlashing<T::EthSpec>,
             network_tx: UnboundedSender<NetworkMessage<T::EthSpec>>| {
                task_spawner.blocking_json_task(Priority::P0, move || {
                    // Fetch wall clock state for verification (caller DI pattern).
                    let wall_clock_state = beacon_chain::state_query::wall_clock_state(
                        &chain.store,
                        &chain.canonical_head,
                        &chain.spec,
                        &chain.slot_clock,
                    )
                    .map_err(|e| {
                        warp_utils::reject::object_invalid(format!(
                            "gossip verification failed: {:?}",
                            e
                        ))
                    })?;

                    let outcome = chain
                        .operations
                        .observed_attester_slashings
                        .lock()
                        .verify_and_observe(slashing.clone(), &wall_clock_state, &chain.spec)
                        .map_err(|e| {
                            warp_utils::reject::object_invalid(format!(
                                "gossip verification failed: {:?}",
                                e
                            ))
                        })?;

                    // Notify the validator monitor.
                    chain
                        .block_importer
                        .validator_monitor
                        .read()
                        .register_api_attester_slashing(slashing.to_ref());

                    if let ObservationOutcome::New(verified_slashing) = outcome {
                        // Apply to fork choice.
                        chain
                            .canonical_head
                            .fork_choice_write_lock()
                            .on_attester_slashing(verified_slashing.as_inner().to_ref());

                        // Fire SSE event.
                        if let Some(event_handler) = chain.block_importer.event_handler.as_ref()
                            && event_handler.has_attester_slashing_subscribers()
                        {
                            event_handler.register(EventKind::AttesterSlashing(Box::new(
                                verified_slashing.clone().into_inner(),
                            )));
                        }

                        utils::publish_pubsub_message(
                            &network_tx,
                            PubsubMessage::AttesterSlashing(Box::new(
                                verified_slashing.clone().into_inner(),
                            )),
                        )?;

                        chain
                            .operations
                            .op_pool
                            .insert_attester_slashing(verified_slashing);
                    }

                    Ok(())
                })
            },
        )
        .boxed()
}

/// GET beacon/pool/attestations?committee_index,slot
pub fn get_beacon_pool_attestations<T: BeaconChainTypes>(
    beacon_pool_path_any: &BeaconPoolPathAnyFilter<T>,
) -> ResponseFilter {
    beacon_pool_path_any
        .clone()
        .and(warp::path("attestations"))
        .and(warp::path::end())
        .and(warp::query::<AttestationPoolQuery>())
        .then(
            |endpoint_version: EndpointVersion,
             task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>,
             query: AttestationPoolQuery| {
                task_spawner.blocking_response_task(Priority::P1, move || {
                    let query_filter = |data: &AttestationData, committee_indices: HashSet<u64>| {
                        query.slot.is_none_or(|slot| slot == data.slot)
                            && query
                                .committee_index
                                .is_none_or(|index| committee_indices.contains(&index))
                    };

                    let mut attestations = chain
                        .operations
                        .op_pool
                        .get_filtered_attestations(query_filter);
                    attestations.extend(
                        chain
                            .attestation_manager
                            .naive_aggregation_pool
                            .read()
                            .iter()
                            .filter(|&att| {
                                query_filter(att.data(), att.get_committee_indices_map())
                            })
                            .cloned(),
                    );
                    // Use the current slot to find the fork version, and convert all messages to the
                    // current fork's format. This is to ensure consistent message types matching
                    // `Eth-Consensus-Version`.
                    let current_slot =
                        chain
                            .slot_clock
                            .now()
                            .ok_or(warp_utils::reject::custom_server_error(
                                "unable to read slot clock".to_string(),
                            ))?;
                    let fork_name = chain.spec.fork_name_at_slot::<T::EthSpec>(current_slot);
                    let attestations = attestations
                        .into_iter()
                        .filter(|att| {
                            (fork_name.electra_enabled() && matches!(att, Attestation::Electra(_)))
                                || (!fork_name.electra_enabled()
                                    && matches!(att, Attestation::Base(_)))
                        })
                        .collect::<Vec<_>>();

                    let require_version = match endpoint_version {
                        V1 => ResponseIncludesVersion::No,
                        V2 => ResponseIncludesVersion::Yes(fork_name),
                        _ => return Err(unsupported_version_rejection(endpoint_version)),
                    };

                    let res = beacon_response(require_version, &attestations);
                    Ok(add_consensus_version_header(
                        warp::reply::json(&res).into_response(),
                        fork_name,
                    ))
                })
            },
        )
        .boxed()
}

pub fn post_beacon_pool_attestations_v2<T: BeaconChainTypes>(
    network_tx_filter: &NetworkTxFilter<T>,
    optional_consensus_version_header_filter: OptionalConsensusVersionHeaderFilter,
    beacon_pool_path_v2: &BeaconPoolPathV2Filter<T>,
) -> ResponseFilter {
    beacon_pool_path_v2
        .clone()
        .and(warp::path("attestations"))
        .and(warp::path::end())
        .and(warp_utils::json::json::<Vec<SingleAttestation>>())
        .and(optional_consensus_version_header_filter)
        .and(network_tx_filter.clone())
        .then(
            |task_spawner: TaskSpawner<T::EthSpec>,
             chain: Arc<BeaconChain<T>>,
             attestations: Vec<SingleAttestation>,
             _fork_name: Option<ForkName>,
             network_tx: UnboundedSender<NetworkMessage<T::EthSpec>>| async move {
                let result = crate::publish_attestations::publish_attestations(
                    task_spawner,
                    chain,
                    attestations,
                    network_tx,
                    true,
                )
                .await
                .map(|()| warp::reply::json(&()));
                convert_rejection(result).await
            },
        )
        .boxed()
}
