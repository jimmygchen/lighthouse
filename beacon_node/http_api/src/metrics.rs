use crate::API_PREFIX;
pub use metrics::*;
use std::sync::LazyLock;

pub static HTTP_API_PATHS_TOTAL: LazyLock<Result<IntCounterVec>> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "http_api_paths_total",
        "Count of HTTP requests received",
        &["path"],
    )
});
pub static HTTP_API_STATUS_CODES_TOTAL: LazyLock<Result<IntCounterVec>> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "http_api_status_codes_total",
        "Count of HTTP status codes returned",
        &["status"],
    )
});
pub static HTTP_API_PATHS_TIMES: LazyLock<Result<HistogramVec>> = LazyLock::new(|| {
    try_create_histogram_vec(
        "http_api_paths_times",
        "Duration to process HTTP requests per path",
        &["path"],
    )
});

pub static HTTP_API_BLOCK_BROADCAST_DELAY_TIMES: LazyLock<Result<HistogramVec>> =
    LazyLock::new(|| {
        try_create_histogram_vec(
            "http_api_block_broadcast_delay_times",
            "Time between start of the slot and when the block completed broadcast and processing",
            &["provenance"],
        )
    });
pub static HTTP_API_BLOCK_GOSSIP_TIMES: LazyLock<Result<HistogramVec>> = LazyLock::new(|| {
    try_create_histogram_vec_with_buckets(
        "http_api_block_gossip_times",
        "Time between receiving the block on HTTP and publishing it on gossip",
        decimal_buckets(-2, 2),
        &["provenance"],
    )
});
pub static HTTP_API_STATE_SSZ_ENCODE_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "http_api_state_ssz_encode_times",
        "Time to SSZ encode a BeaconState for a response",
    )
});
pub static HTTP_API_STATE_ROOT_TIMES: LazyLock<Result<Histogram>> = LazyLock::new(|| {
    try_create_histogram(
        "http_api_state_root_times",
        "Time to load a state root for a request",
    )
});

/// Creates a `warp` logging wrapper which we use for Prometheus metrics (not necessarily logging,
/// per say).
pub fn prometheus_metrics() -> warp::filters::log::Log<impl Fn(warp::filters::log::Info) + Clone> {
    warp::log::custom(move |info| {
        // Here we restrict the `info.path()` value to some predefined values. Without this, we end
        // up with a new metric type each time someone includes something unique in the path (e.g.,
        // a block hash).
        let path = {
            let equals = |s: &'static str| -> Option<&'static str> {
                if info.path() == format!("/{}/{}", API_PREFIX, s) {
                    Some(s)
                } else {
                    None
                }
            };

            let starts_with = |s: &'static str| -> Option<&'static str> {
                if info.path().starts_with(&format!("/{}/{}", API_PREFIX, s)) {
                    Some(s)
                } else {
                    None
                }
            };

            // First line covers `POST /v1/beacon/blocks` only
            equals("v1/beacon/blocks")
                .or_else(|| starts_with("v2/beacon/blocks"))
                .or_else(|| starts_with("v1/beacon/blob_sidecars"))
                .or_else(|| starts_with("v1/beacon/blobs"))
                .or_else(|| starts_with("v1/beacon/blocks/head/root"))
                .or_else(|| starts_with("v1/beacon/blinded_blocks"))
                .or_else(|| starts_with("v2/beacon/blinded_blocks"))
                .or_else(|| starts_with("v1/beacon/headers"))
                .or_else(|| starts_with("v1/beacon/light_client"))
                .or_else(|| starts_with("v1/beacon/pool/attestations"))
                .or_else(|| starts_with("v2/beacon/pool/attestations"))
                .or_else(|| starts_with("v1/beacon/pool/attester_slashings"))
                .or_else(|| starts_with("v1/beacon/pool/bls_to_execution_changes"))
                .or_else(|| starts_with("v1/beacon/pool/proposer_slashings"))
                .or_else(|| starts_with("v1/beacon/pool/sync_committees"))
                .or_else(|| starts_with("v1/beacon/pool/voluntary_exits"))
                .or_else(|| starts_with("v1/beacon/rewards/blocks"))
                .or_else(|| starts_with("v1/beacon/rewards/attestations"))
                .or_else(|| starts_with("v1/beacon/rewards/sync_committee"))
                .or_else(|| starts_with("v1/beacon/rewards"))
                .or_else(|| starts_with("v1/beacon/states"))
                .or_else(|| starts_with("v1/beacon/"))
                .or_else(|| starts_with("v2/beacon/"))
                .or_else(|| starts_with("v1/builder/states"))
                .or_else(|| starts_with("v1/config/deposit_contract"))
                .or_else(|| starts_with("v1/config/fork_schedule"))
                .or_else(|| starts_with("v1/config/spec"))
                .or_else(|| starts_with("v1/config/"))
                .or_else(|| starts_with("v1/debug/"))
                .or_else(|| starts_with("v2/debug/"))
                .or_else(|| starts_with("v1/events"))
                .or_else(|| starts_with("v1/events/"))
                .or_else(|| starts_with("v1/node/health"))
                .or_else(|| starts_with("v1/node/identity"))
                .or_else(|| starts_with("v1/node/peers"))
                .or_else(|| starts_with("v1/node/peer_count"))
                .or_else(|| starts_with("v1/node/syncing"))
                .or_else(|| starts_with("v1/node/version"))
                .or_else(|| starts_with("v1/node"))
                .or_else(|| starts_with("v1/validator/aggregate_and_proofs"))
                .or_else(|| starts_with("v2/validator/aggregate_and_proofs"))
                .or_else(|| starts_with("v1/validator/aggregate_attestation"))
                .or_else(|| starts_with("v2/validator/aggregate_attestation"))
                .or_else(|| starts_with("v1/validator/attestation_data"))
                .or_else(|| starts_with("v1/validator/beacon_committee_subscriptions"))
                .or_else(|| starts_with("v1/validator/blinded_blocks"))
                .or_else(|| starts_with("v2/validator/blinded_blocks"))
                .or_else(|| starts_with("v1/validator/blocks"))
                .or_else(|| starts_with("v2/validator/blocks"))
                .or_else(|| starts_with("v3/validator/blocks"))
                .or_else(|| starts_with("v1/validator/contribution_and_proofs"))
                .or_else(|| starts_with("v1/validator/duties/attester"))
                .or_else(|| starts_with("v1/validator/duties/proposer"))
                .or_else(|| starts_with("v1/validator/duties/sync"))
                .or_else(|| starts_with("v1/validator/liveness"))
                .or_else(|| starts_with("v1/validator/prepare_beacon_proposer"))
                .or_else(|| starts_with("v1/validator/register_validator"))
                .or_else(|| starts_with("v1/validator/sync_committee_contribution"))
                .or_else(|| starts_with("v1/validator/sync_committee_subscriptions"))
                .or_else(|| starts_with("v1/validator/"))
                .or_else(|| starts_with("v2/validator/"))
                .or_else(|| starts_with("v3/validator/"))
                .or_else(|| starts_with("lighthouse"))
                .unwrap_or("other")
        };

        metrics::inc_counter_vec(&HTTP_API_PATHS_TOTAL, &[path]);
        metrics::inc_counter_vec(&HTTP_API_STATUS_CODES_TOTAL, &[&info.status().to_string()]);
        metrics::observe_timer_vec(&HTTP_API_PATHS_TIMES, &[path], info.elapsed());
    })
}
