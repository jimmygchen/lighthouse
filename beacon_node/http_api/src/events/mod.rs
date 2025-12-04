use crate::task_spawner::{Priority, TaskSpawner};
use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::types::{EventQuery, EventTopic};
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use warp::Rejection;
use warp::reply::Response;
use warp::sse::Event;

pub fn get_events<T: BeaconChainTypes>(
    topics_res: Result<EventQuery, Rejection>,
    task_spawner: TaskSpawner<<T as BeaconChainTypes>::EthSpec>,
    chain: Arc<BeaconChain<T>>,
) -> impl Future<Output = Response> {
    task_spawner.blocking_response_task(Priority::P0, move || {
        let topics = topics_res?;
        // for each topic subscribed spawn a new subscription
        let mut receivers = Vec::with_capacity(topics.topics.len());

        if let Some(event_handler) = chain.event_handler.as_ref() {
            for topic in topics.topics {
                let receiver = match topic {
                    EventTopic::Head => event_handler.subscribe_head(),
                    EventTopic::Block => event_handler.subscribe_block(),
                    EventTopic::BlobSidecar => event_handler.subscribe_blob_sidecar(),
                    EventTopic::DataColumnSidecar => event_handler.subscribe_data_column_sidecar(),
                    EventTopic::Attestation => event_handler.subscribe_attestation(),
                    EventTopic::SingleAttestation => event_handler.subscribe_single_attestation(),
                    EventTopic::VoluntaryExit => event_handler.subscribe_exit(),
                    EventTopic::FinalizedCheckpoint => event_handler.subscribe_finalized(),
                    EventTopic::ChainReorg => event_handler.subscribe_reorgs(),
                    EventTopic::ContributionAndProof => event_handler.subscribe_contributions(),
                    EventTopic::PayloadAttributes => event_handler.subscribe_payload_attributes(),
                    EventTopic::LateHead => event_handler.subscribe_late_head(),
                    EventTopic::LightClientFinalityUpdate => {
                        event_handler.subscribe_light_client_finality_update()
                    }
                    EventTopic::LightClientOptimisticUpdate => {
                        event_handler.subscribe_light_client_optimistic_update()
                    }
                    EventTopic::BlockReward => event_handler.subscribe_block_reward(),
                    EventTopic::AttesterSlashing => event_handler.subscribe_attester_slashing(),
                    EventTopic::ProposerSlashing => event_handler.subscribe_proposer_slashing(),
                    EventTopic::BlsToExecutionChange => {
                        event_handler.subscribe_bls_to_execution_change()
                    }
                    EventTopic::BlockGossip => event_handler.subscribe_block_gossip(),
                };

                receivers.push(
                    BroadcastStream::new(receiver)
                        .map(|msg| {
                            match msg {
                                Ok(data) => Event::default()
                                    .event(data.topic_name())
                                    .json_data(data)
                                    .unwrap_or_else(|e| {
                                        Event::default().comment(format!("error - bad json: {e:?}"))
                                    }),
                                // Do not terminate the stream if the channel fills
                                // up. Just drop some messages and send a comment to
                                // the client.
                                Err(BroadcastStreamRecvError::Lagged(n)) => Event::default()
                                    .comment(format!("error - dropped {n} messages")),
                            }
                        })
                        .map(Ok::<_, std::convert::Infallible>),
                );
            }
        } else {
            return Err(warp_utils::reject::custom_server_error(
                "event handler was not initialized".to_string(),
            ));
        }

        let s = futures::stream::select_all(receivers);

        Ok(warp::sse::reply(warp::sse::keep_alive().stream(s)))
    })
}
