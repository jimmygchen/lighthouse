use crate::publish_pubsub_message;
use crate::task_spawner::{Priority, TaskSpawner};
use beacon_chain::observed_operations::ObservationOutcome;
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::Failure;
use lighthouse_network::PubsubMessage;
use network::NetworkMessage;
use operation_pool::ReceivedPreCapella;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};
use types::SignedBlsToExecutionChange;
use warp::reply::Response;

pub fn post_beacon_pool_bls_to_execution_changes<T: BeaconChainTypes>(
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
    address_changes: Vec<SignedBlsToExecutionChange>,
    network_tx: UnboundedSender<NetworkMessage<<T as BeaconChainTypes>::EthSpec>>,
) -> impl Future<Output = Response> {
    task_spawner.blocking_json_task(Priority::P0, move || {
        let mut failures = vec![];

        for (index, address_change) in address_changes.into_iter().enumerate() {
            let validator_index = address_change.message.validator_index;

            match chain.verify_bls_to_execution_change_for_http_api(address_change) {
                Ok(ObservationOutcome::New(verified_address_change)) => {
                    let validator_index =
                        verified_address_change.as_inner().message.validator_index;
                    let address = verified_address_change
                        .as_inner()
                        .message
                        .to_execution_address;

                    // New to P2P *and* op pool, gossip immediately if post-Capella.
                    let received_pre_capella =
                        if chain.current_slot_is_post_capella().unwrap_or(false) {
                            ReceivedPreCapella::No
                        } else {
                            ReceivedPreCapella::Yes
                        };
                    if matches!(received_pre_capella, ReceivedPreCapella::No) {
                        publish_pubsub_message(
                            &network_tx,
                            PubsubMessage::BlsToExecutionChange(Box::new(
                                verified_address_change.as_inner().clone(),
                            )),
                        )?;
                    }

                    // Import to op pool (may return `false` if there's a race).
                    let imported = chain.import_bls_to_execution_change(
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
}
