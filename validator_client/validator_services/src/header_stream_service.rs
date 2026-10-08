//! Consumes the MPBC builder header-stream websocket ahead of a Gloas block proposal.
//!
//! The validator client opens one websocket per configured builder (`BuilderStore::stream_targets`)
//! as soon as a `payload_attributes` SSE event names one of our proposers for the upcoming slot,
//! keeps the latest decoded bid per builder in a [`StreamBidStore`], and closes/reopens the streams
//! whenever the proposal's `(slot, parent_block_root, parent_block_hash)` changes (a re-org). At
//! proposal time `block_service` reads [`StreamBidStore::best_for`] to decide whether to call
//! `with_bid` instead of the ordinary `produceBlockV4` flow.
use crate::duties_service::DutiesService;
use crate::request_auth_cache::RequestAuthCache;
use beacon_node_fallback::BeaconNodeFallback;
use bls::PublicKeyBytes;
use builder_store::{BuilderStore, StreamTarget};
use eth2::types::{EventKind, EventTopic, SseExtendedPayloadAttributes};
use futures::{SinkExt, StreamExt};
use http::HeaderValue;
use parking_lot::{Mutex, RwLock};
use slot_clock::SlotClock;
use ssz::Encode;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use task_executor::TaskExecutor;
use tokio_tungstenite::Connector;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, info, warn};
use types::{
    EthSpec, ExecutionBlockHash, ForkVersionDecode, Hash256, SignedExecutionPayloadBid, Slot,
};
use validator_metrics::{inc_counter_vec, observe_duration};
use validator_store::ValidatorStore;

/// MPBC frame convention: the only defined frame kind, a full-replacement bid.
const FRAME_KIND_BID: u8 = 0x01;
/// MPBC convention for the Gloas fork byte; not assigned by a spec.
const FORK_BYTE_GLOAS: u8 = 7;

const HEADER_API_KEY: &str = "x-api-key";
const HEADER_TIMEOUT_MS: &str = "x-timeout-ms";
const HEADER_DATE_MILLISECONDS: &str = "date-milliseconds";
const HEADER_PARENT_BLOCK_ROOT: &str = "x-parent-block-root";
const HEADER_REQUEST_AUTH: &str = "x-request-auth";

/// Extra time, beyond the slot's end, that an open header-stream connection is kept alive for
/// before being closed by [`HeaderStreamService`]. A generous margin: the server is expected to
/// close well before this (per the window rules), so this is only a backstop.
const CONNECTION_GRACE: Duration = Duration::from_millis(500);

/// An explicit `rustls` client config for `wss://` header-stream connections, built with an
/// explicit `ring` [`rustls::crypto::CryptoProvider`] (matching the backend `reqwest`'s
/// `rustls-tls`/`rustls-tls-native-roots` features select elsewhere in Lighthouse).
///
/// This workspace links both the `ring` and `aws-lc-rs` crypto backends into `rustls` (pulled in
/// by different dependencies), so no provider is ever the unambiguous process-default. Relying on
/// `tokio-tungstenite`'s own default connector — built via `rustls::ClientConfig::builder()`,
/// which resolves the process-default provider — panics at runtime on the first `wss://` connect
/// with exactly that ambiguity. Building (and always passing) an explicit config sidesteps it
/// entirely: it never touches, or depends on, any process-global default.
static HEADER_STREAM_TLS_CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("the ring provider supports rustls's default TLS protocol versions")
    .with_root_certificates(root_store)
    .with_no_client_auth();

    Arc::new(config)
});

/// The best streamed builder bid for one proposal, as handed to `block_service`.
#[derive(Debug, Clone)]
pub struct StreamBidChoice<E: EthSpec> {
    pub bid: SignedExecutionPayloadBid<E>,
    /// The builder's HTTP base URL (not the websocket URL), to echo on `publishBlockV2` if the
    /// supplied bid is the one the beacon node used.
    pub builder_url: String,
    pub builder_boost_factor: u64,
    pub received_at: Instant,
}

struct BuilderBidEntry<E: EthSpec> {
    bid: SignedExecutionPayloadBid<E>,
    builder_boost_factor: u64,
    received_at: Instant,
}

struct ContextEntry<E: EthSpec> {
    parent_block_root: Hash256,
    parent_block_hash: ExecutionBlockHash,
    per_builder: HashMap<String, BuilderBidEntry<E>>,
}

/// Holds the latest accepted streamed bid per builder, for every `(slot, proposer)` context
/// currently being streamed (or just streamed, until the slot passes).
pub struct StreamBidStore<E: EthSpec> {
    contexts: RwLock<HashMap<(Slot, PublicKeyBytes), ContextEntry<E>>>,
}

impl<E: EthSpec> Default for StreamBidStore<E> {
    fn default() -> Self {
        Self {
            contexts: RwLock::new(HashMap::new()),
        }
    }
}

impl<E: EthSpec> StreamBidStore<E> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open (or reset) the context for `(slot, pubkey)`, discarding any bids recorded against a
    /// previous parent. Called whenever the proposal's parent changes.
    pub(crate) fn open_context(
        &self,
        slot: Slot,
        pubkey: PublicKeyBytes,
        parent_block_root: Hash256,
        parent_block_hash: ExecutionBlockHash,
    ) {
        self.contexts.write().insert(
            (slot, pubkey),
            ContextEntry {
                parent_block_root,
                parent_block_hash,
                per_builder: HashMap::new(),
            },
        );
    }

    /// Record the latest accepted bid from `builder_url` for `(slot, pubkey)`, replacing any
    /// earlier bid from the same builder. A no-op if the context has since been closed or
    /// reopened for a different parent: `parent_block_root`/`parent_block_hash` are the task's own
    /// (stale) view of the context it opened the stream against, checked here against whatever is
    /// currently stored, so a frame accepted by a task whose context has moved on is never
    /// recorded under the new context.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_bid(
        &self,
        slot: Slot,
        pubkey: PublicKeyBytes,
        parent_block_root: Hash256,
        parent_block_hash: ExecutionBlockHash,
        builder_url: String,
        bid: SignedExecutionPayloadBid<E>,
        builder_boost_factor: u64,
    ) {
        if let Some(context) = self.contexts.write().get_mut(&(slot, pubkey)) {
            if context.parent_block_root != parent_block_root
                || context.parent_block_hash != parent_block_hash
            {
                return;
            }
            context.per_builder.insert(
                builder_url,
                BuilderBidEntry {
                    bid,
                    builder_boost_factor,
                    received_at: Instant::now(),
                },
            );
        }
    }

    /// Remove the context for `(slot, pubkey)` entirely, discarding every builder's bid.
    fn clear_context(&self, slot: Slot, pubkey: PublicKeyBytes) {
        self.contexts.write().remove(&(slot, pubkey));
    }

    /// The `(parent_block_root, parent_block_hash)` the context for `(slot, pubkey)` is currently
    /// open against, if any.
    pub fn context_parent(
        &self,
        slot: Slot,
        pubkey: PublicKeyBytes,
    ) -> Option<(Hash256, ExecutionBlockHash)> {
        self.contexts
            .read()
            .get(&(slot, pubkey))
            .map(|context| (context.parent_block_root, context.parent_block_hash))
    }

    /// The best streamed bid for `(slot, pubkey)`, ranked by trusted value
    /// (`value + execution_payment`).
    pub fn best_for(&self, slot: Slot, pubkey: PublicKeyBytes) -> Option<StreamBidChoice<E>> {
        let contexts = self.contexts.read();
        let context = contexts.get(&(slot, pubkey))?;
        context
            .per_builder
            .iter()
            .max_by_key(|(_, entry)| {
                entry
                    .bid
                    .message()
                    .value()
                    .saturating_add(entry.bid.message().execution_payment())
            })
            .map(|(builder_url, entry)| StreamBidChoice {
                bid: entry.bid.clone(),
                builder_url: builder_url.clone(),
                builder_boost_factor: entry.builder_boost_factor,
                received_at: entry.received_at,
            })
    }
}

/// Pure evaluation of one decoded frame against its proposal context and builder policy. Returns
/// the accepted bid, or `Err(outcome)` naming the `vc_builder_header_stream_frames_total` outcome
/// label to record.
fn evaluate_frame<E: EthSpec>(
    bytes: &[u8],
    slot: Slot,
    parent_block_root: Hash256,
    parent_block_hash: ExecutionBlockHash,
    target: &StreamTarget,
) -> Result<SignedExecutionPayloadBid<E>, &'static str> {
    let kind = *bytes.first().ok_or(validator_metrics::DECODE_FAILED)?;
    if kind != FRAME_KIND_BID {
        return Err(validator_metrics::DECODE_FAILED);
    }
    let fork = *bytes.get(1).ok_or(validator_metrics::DECODE_FAILED)?;
    if fork != FORK_BYTE_GLOAS {
        return Err(validator_metrics::UNSUPPORTED_FORK);
    }
    let bid =
        SignedExecutionPayloadBid::<E>::from_ssz_bytes_by_fork(&bytes[2..], types::ForkName::Gloas)
            .map_err(|_| validator_metrics::DECODE_FAILED)?;

    let message = bid.message();
    if message.slot() != slot
        || message.parent_block_hash() != parent_block_hash
        || message.parent_block_root() != parent_block_root
    {
        return Err(validator_metrics::CONTEXT_MISMATCH);
    }
    if message.execution_payment() > target.max_execution_payment {
        return Err(validator_metrics::OVER_PAYMENT_CAP);
    }
    let trusted_value = message.value().saturating_add(message.execution_payment());
    if trusted_value < target.min_bid {
        return Err(validator_metrics::BELOW_MIN_BID);
    }
    Ok(bid)
}

/// Builds the `ws`/`wss` URL for `GET /eth/v1/builder/header_stream/{slot}/{parent_hash}/{pubkey}`
/// from a builder's (http/https) URL.
fn to_header_stream_url(
    url: &builder_types::BuilderUrl,
    slot: Slot,
    parent_block_hash: ExecutionBlockHash,
    pubkey: &PublicKeyBytes,
) -> Result<String, String> {
    let full = url
        .to_sensitive_url()
        .map_err(|_| "invalid builder url".to_string())?
        .expose_full()
        .to_string();
    let ws_base = if let Some(rest) = full.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = full.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err(format!("unsupported builder url scheme: {full}"));
    };
    let mut parsed =
        url::Url::parse(&ws_base).map_err(|e| format!("failed to parse builder url: {e:?}"))?;
    parsed
        .path_segments_mut()
        .map_err(|()| "builder url cannot be a base".to_string())?
        .push("eth")
        .push("v1")
        .push("builder")
        .push("header_stream")
        .push(&slot.to_string())
        .push(&format!("{parent_block_hash:?}"))
        .push(&pubkey.as_hex_string());
    Ok(parsed.to_string())
}

/// Active header-stream connections for one proposer's current proposal context.
///
/// The slot is carried entirely in the `active` map's key, not duplicated here.
struct ActiveContext {
    parent_block_root: Hash256,
    parent_block_hash: ExecutionBlockHash,
    handles: Vec<tokio::task::JoinHandle<Option<()>>>,
}

struct Inner<S: ValidatorStore, T> {
    duties_service: Arc<DutiesService<S, T>>,
    validator_store: Arc<S>,
    builder_store: BuilderStore,
    request_auth_cache: RequestAuthCache,
    beacon_nodes: Arc<BeaconNodeFallback<T>>,
    slot_clock: T,
    executor: TaskExecutor,
    store: Arc<StreamBidStore<S::E>>,
    /// Active header-stream connections, keyed by `(slot, pubkey)` rather than just `pubkey`: a
    /// `payload_attributes` event for a different slot (e.g. the next one) must never abort or
    /// clear an already-open context for an earlier slot of the same proposer.
    active: Mutex<HashMap<(Slot, PublicKeyBytes), ActiveContext>>,
}

/// Subscribes to BN `payload_attributes` SSE events and, for our own upcoming proposals, opens and
/// maintains the builder header-stream websockets.
pub struct HeaderStreamService<S: ValidatorStore, T> {
    inner: Arc<Inner<S, T>>,
}

impl<S: ValidatorStore, T> Clone for HeaderStreamService<S, T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: ValidatorStore + 'static, T: SlotClock + 'static> HeaderStreamService<S, T> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        duties_service: Arc<DutiesService<S, T>>,
        validator_store: Arc<S>,
        builder_store: BuilderStore,
        request_auth_cache: RequestAuthCache,
        beacon_nodes: Arc<BeaconNodeFallback<T>>,
        slot_clock: T,
        executor: TaskExecutor,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                duties_service,
                validator_store,
                builder_store,
                request_auth_cache,
                beacon_nodes,
                slot_clock,
                executor,
                store: Arc::new(StreamBidStore::default()),
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The shared store `block_service` queries for the best streamed bid at proposal time.
    pub fn bid_store(&self) -> Arc<StreamBidStore<S::E>> {
        self.inner.store.clone()
    }

    /// Starts the `payload_attributes` polling loop on the executor. Restarts the loop (with a
    /// short backoff) if the underlying BN event streams end or error.
    pub fn start_update_service(self) -> Result<(), String> {
        info!("Builder header-stream service started");
        let executor = self.inner.executor.clone();
        executor.spawn(
            async move {
                loop {
                    if let Err(e) = self.run_payload_attributes_loop().await {
                        warn!(
                            error = %e,
                            "Builder header-stream payload_attributes loop ended, restarting"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            },
            "header_stream_service",
        );
        Ok(())
    }

    async fn run_payload_attributes_loop(&self) -> Result<(), String> {
        let candidates = { self.inner.beacon_nodes.candidates.read().await.clone() };
        let mut streams = Vec::new();
        for candidate in &candidates {
            match candidate
                .beacon_node
                .get_events::<S::E>(&[EventTopic::PayloadAttributes])
                .await
            {
                Ok(stream) => streams.push(stream.map(|event| (candidate.index, event))),
                Err(e) => warn!(
                    error = ?e,
                    node_index = candidate.index,
                    "Failed to get payload_attributes stream"
                ),
            }
        }
        if streams.is_empty() {
            return Err("No beacon nodes available for payload_attributes streaming".to_string());
        }

        let mut combined = futures::stream::select_all(streams);
        while let Some((_index, event_result)) = combined.next().await {
            match event_result {
                Ok(EventKind::PayloadAttributes(versioned)) => {
                    self.handle_payload_attributes(versioned.data).await;
                }
                Ok(_) => continue,
                Err(e) => return Err(format!("payload_attributes stream error: {e:?}")),
            }
        }
        Err("payload_attributes stream ended unexpectedly".to_string())
    }

    /// Returns the pubkey of one of our own proposers for `slot`, if `proposer_index` names one.
    fn find_own_proposer(&self, slot: Slot, proposer_index: u64) -> Option<PublicKeyBytes> {
        self.inner
            .duties_service
            .block_proposers(slot)
            .into_iter()
            .find(|pubkey| {
                self.inner.validator_store.validator_index(pubkey) == Some(proposer_index)
            })
    }

    async fn handle_payload_attributes(&self, event: SseExtendedPayloadAttributes) {
        let slot = event.proposal_slot;
        let parent_block_root = event.parent_block_root;
        let parent_block_hash = event.parent_block_hash;

        let Some(pubkey) = self.find_own_proposer(slot, event.proposer_index) else {
            return;
        };

        // Keyed by (slot, pubkey): a payload_attributes event for a different slot (e.g. the next
        // one, which this BN fires several times per slot for) must never be compared against —
        // let alone reopen or clear — an already-open context for a different slot of the same
        // proposer. Only a changed parent for THIS slot does that.
        let unchanged = {
            let active = self.inner.active.lock();
            active.get(&(slot, pubkey)).is_some_and(|context| {
                context.parent_block_root == parent_block_root
                    && context.parent_block_hash == parent_block_hash
            })
        };
        if unchanged {
            return;
        }

        self.reopen_for(pubkey, slot, parent_block_root, parent_block_hash)
            .await;
    }

    /// Closes any previous streams for `(slot, pubkey)` and discards their bids, then opens new
    /// streams (one per configured header-stream target) for the new `(slot, parent_block_root,
    /// parent_block_hash)`.
    ///
    /// Only ever touches the entry for this exact slot: a previous entry for a different slot of
    /// the same proposer is left untouched (and is not "reopened" in the metric sense below).
    async fn reopen_for(
        &self,
        pubkey: PublicKeyBytes,
        slot: Slot,
        parent_block_root: Hash256,
        parent_block_hash: ExecutionBlockHash,
    ) {
        let previous = self.inner.active.lock().remove(&(slot, pubkey));
        if let Some(previous) = previous {
            for handle in previous.handles {
                handle.abort();
            }
            self.inner.store.clear_context(slot, pubkey);
            inc_counter_vec(
                &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                &[validator_metrics::REOPENED_PARENT_CHANGE],
            );
        }

        self.inner
            .store
            .open_context(slot, pubkey, parent_block_root, parent_block_hash);

        let targets = self.inner.builder_store.stream_targets(&pubkey);
        if targets.is_empty() {
            self.inner.active.lock().insert(
                (slot, pubkey),
                ActiveContext {
                    parent_block_root,
                    parent_block_hash,
                    handles: Vec::new(),
                },
            );
            return;
        }

        let slot_end = self
            .inner
            .slot_clock
            .start_of(slot)
            .unwrap_or_default()
            .saturating_add(self.inner.slot_clock.slot_duration())
            .saturating_add(CONNECTION_GRACE);

        let mut handles = Vec::with_capacity(targets.len());
        for target in targets {
            let service = self.clone();
            if let Some(handle) = self.inner.executor.spawn_handle(
                async move {
                    service
                        .run_stream(
                            target,
                            slot,
                            parent_block_root,
                            parent_block_hash,
                            pubkey,
                            slot_end,
                        )
                        .await;
                },
                "header_stream_connection",
            ) {
                handles.push(handle);
            }
        }

        self.inner.active.lock().insert(
            (slot, pubkey),
            ActiveContext {
                parent_block_root,
                parent_block_hash,
                handles,
            },
        );
    }

    /// Opens, reads from, and eventually closes one builder's header-stream websocket, reopening
    /// it once if the server closes early (see [`Self::run_header_stream_connection`]).
    async fn run_stream(
        &self,
        target: StreamTarget,
        slot: Slot,
        parent_block_root: Hash256,
        parent_block_hash: ExecutionBlockHash,
        pubkey: PublicKeyBytes,
        slot_end: Duration,
    ) {
        let builder_url_string = target.url.to_string();

        let ws_url = match to_header_stream_url(&target.url, slot, parent_block_hash, &pubkey) {
            Ok(url) => url,
            Err(e) => {
                warn!(error = %e, builder_url = %builder_url_string, "Failed to build header-stream url");
                inc_counter_vec(
                    &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                    &[validator_metrics::CONNECT_FAILED],
                );
                return;
            }
        };

        let now = self.inner.slot_clock.now_duration().unwrap_or_default();

        // Bounded at `slot_end` (the slot's end plus `CONNECTION_GRACE`, computed by the caller):
        // a hung signer must not hold this task open past the slot any more than a hung
        // handshake below may.
        let sign_deadline =
            slot_end.saturating_sub(self.inner.slot_clock.now_duration().unwrap_or(now));
        let signed_auth = match tokio::time::timeout(
            sign_deadline,
            self.inner.request_auth_cache.get_or_sign(
                slot,
                pubkey,
                target.auth_data.clone(),
                |request_auth_v1| {
                    self.inner
                        .validator_store
                        .sign_request_auth_v1(pubkey, request_auth_v1)
                },
            ),
        )
        .await
        {
            Ok(Ok(signed)) => signed,
            Ok(Err(e)) => {
                warn!(
                    error = ?e,
                    builder_url = %builder_url_string,
                    "Failed to sign header-stream request auth"
                );
                inc_counter_vec(
                    &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                    &[validator_metrics::CONNECT_FAILED],
                );
                return;
            }
            Err(_elapsed) => {
                warn!(
                    builder_url = %builder_url_string,
                    "Header-stream request-auth signing timed out before slot end"
                );
                inc_counter_vec(
                    &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                    &[validator_metrics::CONNECT_FAILED],
                );
                return;
            }
        };
        let auth_hex = format!("0x{}", hex::encode(signed_auth.as_ssz_bytes()));

        // The same instant used for `x-timeout-ms` below: the deadline this VC needs a streamed
        // bid by to still use it for this slot's proposal. Spec-following servers (MPBC docs,
        // Helix) cap the header-stream connection window at 3200 ms, so a stream opened early (on
        // `payload_attributes`, well ahead of the slot) can be closed by the server long before
        // this deadline; reopen once in that case so the VC still gets a chance at a bid.
        let slot_start = self.inner.slot_clock.start_of(slot).unwrap_or(now);
        let produce_deadline = slot_start.saturating_add(Duration::from_millis(500));

        let mut reopened = false;
        loop {
            let closed_early = self
                .run_header_stream_connection(
                    &target,
                    &builder_url_string,
                    &ws_url,
                    &auth_hex,
                    slot,
                    parent_block_root,
                    parent_block_hash,
                    pubkey,
                    slot_end,
                    produce_deadline,
                )
                .await;

            if closed_early && !reopened {
                reopened = true;
                inc_counter_vec(
                    &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                    &[validator_metrics::REOPENED_EARLY_CLOSE],
                );
                continue;
            }
            break;
        }
    }

    /// Opens one header-stream websocket connection and runs its read loop until the connection
    /// ends, recomputing `x-timeout-ms`/`date-milliseconds` for this attempt. Returns `true` only
    /// when the server closed the stream (`Close` frame or EOF) strictly before
    /// `produce_deadline`, which is the caller's signal that a reopen may still land a bid in
    /// time; any bid already recorded in [`StreamBidStore`] is left untouched either way.
    #[allow(clippy::too_many_arguments)]
    async fn run_header_stream_connection(
        &self,
        target: &StreamTarget,
        builder_url_string: &str,
        ws_url: &str,
        auth_hex: &str,
        slot: Slot,
        parent_block_root: Hash256,
        parent_block_hash: ExecutionBlockHash,
        pubkey: PublicKeyBytes,
        slot_end: Duration,
        produce_deadline: Duration,
    ) -> bool {
        let now = self.inner.slot_clock.now_duration().unwrap_or_default();
        let slot_start = self.inner.slot_clock.start_of(slot).unwrap_or(now);
        let timeout_ms = slot_start
            .as_millis()
            .saturating_add(500)
            .saturating_sub(now.as_millis())
            .max(1) as u64;

        let mut request = match ws_url.into_client_request() {
            Ok(request) => request,
            Err(e) => {
                warn!(error = %e, builder_url = %builder_url_string, "Failed to build header-stream request");
                inc_counter_vec(
                    &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                    &[validator_metrics::CONNECT_FAILED],
                );
                return false;
            }
        };
        {
            let headers = request.headers_mut();
            headers.insert(
                HEADER_API_KEY,
                HeaderValue::from_str(&target.api_key).unwrap_or(HeaderValue::from_static("")),
            );
            headers.insert(
                HEADER_DATE_MILLISECONDS,
                HeaderValue::from(now.as_millis() as u64),
            );
            headers.insert(HEADER_TIMEOUT_MS, HeaderValue::from(timeout_ms));
            headers.insert(
                HEADER_PARENT_BLOCK_ROOT,
                HeaderValue::from_str(&format!("{parent_block_root:?}"))
                    .unwrap_or(HeaderValue::from_static("")),
            );
            headers.insert(
                HEADER_REQUEST_AUTH,
                HeaderValue::from_str(auth_hex).unwrap_or(HeaderValue::from_static("")),
            );
        }

        // Bounded at `slot_end` too: a signer round trip above may have used up part of the
        // budget, and a handshake that never completes (a stalled or unresponsive builder) must
        // not hold this task open past the slot either.
        let connect_deadline =
            slot_end.saturating_sub(self.inner.slot_clock.now_duration().unwrap_or(now));
        let connector = Connector::Rustls(HEADER_STREAM_TLS_CONFIG.clone());
        let ws_stream = match tokio::time::timeout(
            connect_deadline,
            connect_async_tls_with_config(request, None, false, Some(connector)),
        )
        .await
        {
            Ok(Ok((stream, _response))) => stream,
            Ok(Err(e)) => {
                warn!(error = %e, builder_url = %builder_url_string, "Builder header-stream connect failed");
                inc_counter_vec(
                    &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                    &[validator_metrics::CONNECT_FAILED],
                );
                return false;
            }
            Err(_elapsed) => {
                warn!(
                    builder_url = %builder_url_string,
                    "Builder header-stream connect timed out before slot end"
                );
                inc_counter_vec(
                    &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                    &[validator_metrics::CONNECT_FAILED],
                );
                return false;
            }
        };
        inc_counter_vec(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::CONNECTED],
        );

        let (mut write, mut read) = ws_stream.split();
        let remaining = slot_end.saturating_sub(now);
        let sleep = tokio::time::sleep(remaining);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                _ = &mut sleep => {
                    let _ = write.send(Message::Close(None)).await;
                    return false;
                }
                message = read.next() => {
                    match message {
                        Some(Ok(Message::Binary(payload))) => {
                            match evaluate_frame::<S::E>(
                                payload.as_ref(),
                                slot,
                                parent_block_root,
                                parent_block_hash,
                                target,
                            ) {
                                Ok(bid) => {
                                    inc_counter_vec(
                                        &validator_metrics::BUILDER_HEADER_STREAM_FRAMES_TOTAL,
                                        &[validator_metrics::ACCEPTED],
                                    );
                                    self.inner.store.record_bid(
                                        slot,
                                        pubkey,
                                        parent_block_root,
                                        parent_block_hash,
                                        builder_url_string.to_string(),
                                        bid,
                                        target.builder_boost_factor,
                                    );
                                }
                                Err(outcome) => {
                                    debug!(
                                        outcome,
                                        builder_url = %builder_url_string,
                                        "Dropped builder header-stream frame"
                                    );
                                    inc_counter_vec(
                                        &validator_metrics::BUILDER_HEADER_STREAM_FRAMES_TOTAL,
                                        &[outcome],
                                    );
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            inc_counter_vec(
                                &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                                &[validator_metrics::CLOSED_BY_SERVER],
                            );
                            let now = self.inner.slot_clock.now_duration().unwrap_or(now);
                            return now < produce_deadline;
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => {
                            warn!(error = %e, builder_url = %builder_url_string, "Header-stream read error");
                            inc_counter_vec(
                                &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
                                &[validator_metrics::STREAM_ERROR],
                            );
                            return false;
                        }
                    }
                }
            }
        }
    }
}

/// Records [`validator_metrics::BUILDER_HEADER_STREAM_BID_AGE`] for a bid about to be used.
pub fn observe_bid_age(received_at: Instant) {
    observe_duration(
        &validator_metrics::BUILDER_HEADER_STREAM_BID_AGE,
        received_at.elapsed(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use builder_types::RequestAuthData;
    use types::{
        Address, ExecutionPayloadBidGloas, MainnetEthSpec, SignedExecutionPayloadBidGloas,
    };

    type E = MainnetEthSpec;

    fn test_target(max_execution_payment: u64, min_bid: u64) -> StreamTarget {
        StreamTarget {
            url: "https://builder.example.com".parse().unwrap(),
            api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
            auth_data: RequestAuthData::new(b"builder.example.com".to_vec()).unwrap(),
            max_execution_payment,
            min_bid,
            builder_boost_factor: 100,
        }
    }

    fn bid(
        slot: Slot,
        parent_block_hash: ExecutionBlockHash,
        value: u64,
        execution_payment: u64,
    ) -> SignedExecutionPayloadBid<E> {
        SignedExecutionPayloadBid::Gloas(SignedExecutionPayloadBidGloas {
            message: ExecutionPayloadBidGloas {
                parent_block_hash,
                parent_block_root: Hash256::ZERO,
                block_hash: ExecutionBlockHash::repeat_byte(9),
                prev_randao: Hash256::ZERO,
                fee_recipient: Address::repeat_byte(1),
                gas_limit: 30_000_000,
                builder_index: 0,
                slot,
                value,
                execution_payment,
                blob_kzg_commitments: Default::default(),
                execution_requests_root: Hash256::ZERO,
            },
            signature: bls::Signature::empty(),
        })
    }

    fn frame(bid: &SignedExecutionPayloadBid<E>) -> Vec<u8> {
        let mut out = vec![FRAME_KIND_BID, FORK_BYTE_GLOAS];
        out.extend_from_slice(&bid.as_ssz_bytes());
        out
    }

    #[test]
    fn evaluate_frame_rejects_unknown_kind() {
        let target = test_target(u64::MAX, 0);
        let bytes = [0x02, FORK_BYTE_GLOAS, 0, 0, 0];
        let result = evaluate_frame::<E>(
            &bytes,
            Slot::new(1),
            Hash256::ZERO,
            ExecutionBlockHash::zero(),
            &target,
        );
        assert_eq!(result, Err(validator_metrics::DECODE_FAILED));
    }

    #[test]
    fn evaluate_frame_rejects_unsupported_fork() {
        let target = test_target(u64::MAX, 0);
        let bytes = [FRAME_KIND_BID, 5, 0, 0, 0];
        let result = evaluate_frame::<E>(
            &bytes,
            Slot::new(1),
            Hash256::ZERO,
            ExecutionBlockHash::zero(),
            &target,
        );
        assert_eq!(result, Err(validator_metrics::UNSUPPORTED_FORK));
    }

    #[test]
    fn evaluate_frame_rejects_context_mismatch() {
        let target = test_target(u64::MAX, 0);
        let parent_hash = ExecutionBlockHash::repeat_byte(1);
        // `bid()` always embeds `parent_block_root: Hash256::ZERO`.
        let the_bid = bid(Slot::new(5), parent_hash, 10, 5);

        // Wrong slot.
        let result = evaluate_frame::<E>(
            &frame(&the_bid),
            Slot::new(6),
            Hash256::ZERO,
            parent_hash,
            &target,
        );
        assert_eq!(result, Err(validator_metrics::CONTEXT_MISMATCH));

        // Wrong parent_block_hash.
        let result = evaluate_frame::<E>(
            &frame(&the_bid),
            Slot::new(5),
            Hash256::ZERO,
            ExecutionBlockHash::repeat_byte(2),
            &target,
        );
        assert_eq!(result, Err(validator_metrics::CONTEXT_MISMATCH));

        // Wrong parent_block_root: the hash alone matching is not enough.
        let result = evaluate_frame::<E>(
            &frame(&the_bid),
            Slot::new(5),
            Hash256::repeat_byte(3),
            parent_hash,
            &target,
        );
        assert_eq!(result, Err(validator_metrics::CONTEXT_MISMATCH));
    }

    #[test]
    fn evaluate_frame_rejects_over_payment_cap() {
        let target = test_target(4, 0);
        let parent = ExecutionBlockHash::repeat_byte(1);
        let the_bid = bid(Slot::new(5), parent, 10, 5);
        let result = evaluate_frame::<E>(
            &frame(&the_bid),
            Slot::new(5),
            Hash256::ZERO,
            parent,
            &target,
        );
        assert_eq!(result, Err(validator_metrics::OVER_PAYMENT_CAP));
    }

    #[test]
    fn evaluate_frame_rejects_below_min_bid() {
        let target = test_target(u64::MAX, 100);
        let parent = ExecutionBlockHash::repeat_byte(1);
        let the_bid = bid(Slot::new(5), parent, 10, 5); // trusted value = 15
        let result = evaluate_frame::<E>(
            &frame(&the_bid),
            Slot::new(5),
            Hash256::ZERO,
            parent,
            &target,
        );
        assert_eq!(result, Err(validator_metrics::BELOW_MIN_BID));
    }

    #[test]
    fn evaluate_frame_accepts_valid_bid() {
        let target = test_target(u64::MAX, 10);
        let parent = ExecutionBlockHash::repeat_byte(1);
        let the_bid = bid(Slot::new(5), parent, 10, 5);
        let accepted = evaluate_frame::<E>(
            &frame(&the_bid),
            Slot::new(5),
            Hash256::ZERO,
            parent,
            &target,
        )
        .unwrap();
        assert_eq!(accepted, the_bid);
    }

    #[test]
    fn stream_bid_store_keeps_latest_per_builder_and_picks_best() {
        let store = StreamBidStore::<E>::default();
        let pubkey = PublicKeyBytes::empty();
        let slot = Slot::new(1);
        let parent = ExecutionBlockHash::repeat_byte(1);
        store.open_context(slot, pubkey, Hash256::ZERO, parent);

        assert!(store.best_for(slot, pubkey).is_none());

        store.record_bid(
            slot,
            pubkey,
            Hash256::ZERO,
            parent,
            "builder-a".to_string(),
            bid(slot, parent, 5, 1),
            100,
        );
        store.record_bid(
            slot,
            pubkey,
            Hash256::ZERO,
            parent,
            "builder-b".to_string(),
            bid(slot, parent, 20, 1),
            100,
        );
        let best = store.best_for(slot, pubkey).unwrap();
        assert_eq!(best.builder_url, "builder-b");

        // A later, lower-value frame from builder-b still replaces its own previous frame
        // (latest-wins per builder), which can change which builder is best.
        store.record_bid(
            slot,
            pubkey,
            Hash256::ZERO,
            parent,
            "builder-b".to_string(),
            bid(slot, parent, 1, 1),
            100,
        );
        let best = store.best_for(slot, pubkey).unwrap();
        assert_eq!(best.builder_url, "builder-a");
    }

    /// A frame accepted by a task whose own (stale) view of the context names a different parent
    /// than what's currently stored (e.g. the context was reopened for a new parent, same slot,
    /// while the frame was in flight) must not be recorded.
    #[test]
    fn record_bid_rejects_stale_parent_context() {
        let store = StreamBidStore::<E>::default();
        let pubkey = PublicKeyBytes::empty();
        let slot = Slot::new(1);
        let parent_root = Hash256::repeat_byte(1);
        let parent_hash = ExecutionBlockHash::repeat_byte(2);
        store.open_context(slot, pubkey, parent_root, parent_hash);

        // Stale root.
        store.record_bid(
            slot,
            pubkey,
            Hash256::repeat_byte(9),
            parent_hash,
            "builder-a".to_string(),
            bid(slot, parent_hash, 5, 1),
            100,
        );
        assert!(store.best_for(slot, pubkey).is_none());

        // Stale hash.
        store.record_bid(
            slot,
            pubkey,
            parent_root,
            ExecutionBlockHash::repeat_byte(9),
            "builder-a".to_string(),
            bid(slot, parent_hash, 5, 1),
            100,
        );
        assert!(store.best_for(slot, pubkey).is_none());

        // Matching parent is recorded.
        store.record_bid(
            slot,
            pubkey,
            parent_root,
            parent_hash,
            "builder-a".to_string(),
            bid(slot, parent_hash, 5, 1),
            100,
        );
        assert!(store.best_for(slot, pubkey).is_some());
    }

    #[test]
    fn stream_bid_store_open_context_clears_previous_parent() {
        let store = StreamBidStore::<E>::default();
        let pubkey = PublicKeyBytes::empty();
        let slot = Slot::new(1);
        let old_parent = ExecutionBlockHash::repeat_byte(1);
        let new_parent = ExecutionBlockHash::repeat_byte(2);

        store.open_context(slot, pubkey, Hash256::ZERO, old_parent);
        store.record_bid(
            slot,
            pubkey,
            Hash256::ZERO,
            old_parent,
            "builder-a".to_string(),
            bid(slot, old_parent, 5, 1),
            100,
        );
        assert!(store.best_for(slot, pubkey).is_some());

        // Reopening for a new parent discards the old builder's bid.
        store.open_context(slot, pubkey, Hash256::ZERO, new_parent);
        assert!(store.best_for(slot, pubkey).is_none());
        assert_eq!(
            store.context_parent(slot, pubkey),
            Some((Hash256::ZERO, new_parent))
        );
    }

    #[test]
    fn stream_bid_store_clear_context_removes_entry() {
        let store = StreamBidStore::<E>::default();
        let pubkey = PublicKeyBytes::empty();
        let slot = Slot::new(1);
        let parent = ExecutionBlockHash::repeat_byte(1);
        store.open_context(slot, pubkey, Hash256::ZERO, parent);
        store.record_bid(
            slot,
            pubkey,
            Hash256::ZERO,
            parent,
            "builder-a".to_string(),
            bid(slot, parent, 5, 1),
            100,
        );

        store.clear_context(slot, pubkey);
        assert!(store.best_for(slot, pubkey).is_none());
    }

    #[test]
    fn header_stream_url_converts_scheme_and_builds_path() {
        let url: builder_types::BuilderUrl =
            "https://builder.example.com:8550/base".parse().unwrap();
        let pubkey = PublicKeyBytes::empty();
        let got = to_header_stream_url(
            &url,
            Slot::new(7),
            ExecutionBlockHash::repeat_byte(3),
            &pubkey,
        )
        .unwrap();
        assert!(
            got.starts_with("wss://builder.example.com:8550/base/eth/v1/builder/header_stream/7/")
        );

        let url: builder_types::BuilderUrl = "http://127.0.0.1:9000".parse().unwrap();
        let got =
            to_header_stream_url(&url, Slot::new(1), ExecutionBlockHash::zero(), &pubkey).unwrap();
        assert!(got.starts_with("ws://127.0.0.1:9000/eth/v1/builder/header_stream/1/"));
    }

    /// This workspace links both the `ring` and `aws-lc-rs` rustls crypto backends (pulled in by
    /// different dependencies), so neither is ever the unambiguous process-default. Relying on
    /// `tokio-tungstenite`'s own default connector for a `wss://` URL (built via
    /// `rustls::ClientConfig::builder()`, which resolves that process-default) panics at runtime
    /// with exactly that ambiguity. Constructing the explicit connector used for every
    /// header-stream connection must not.
    #[test]
    fn header_stream_tls_connector_does_not_panic() {
        let _connector = Connector::Rustls(HEADER_STREAM_TLS_CONFIG.clone());
    }

    /// Opens a real websocket against a local `accept_hdr_async` server: asserts all five
    /// handshake headers (including the computed `x-timeout-ms`), that the latest frame wins even
    /// when its value is lower, and that a server-initiated close ends the loop cleanly.
    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn run_stream_sends_expected_headers_and_keeps_latest_frame() {
        use crate::duties_service::DutiesServiceBuilder;
        use validator_test_rig::validator_client_harness::ValidatorClientHarness;

        let harness = ValidatorClientHarness::new(1).await;
        let pubkey = harness.pubkeys[0];
        let slot = Slot::new(5);
        // `bid()` always embeds `parent_block_root: Hash256::ZERO` (checked by
        // `evaluate_frame`/`record_bid`), so the context must be opened against that same root.
        let parent_root = Hash256::ZERO;
        let parent_hash = ExecutionBlockHash::repeat_byte(0xbb);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let captured_headers: Arc<std::sync::Mutex<Option<HashMap<String, String>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured_headers_server = captured_headers.clone();

        let first_bid = bid(slot, parent_hash, 10, 1);
        let second_bid = bid(slot, parent_hash, 2, 1);
        let first_bid_server = first_bid.clone();
        let second_bid_server = second_bid.clone();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws_stream = tokio_tungstenite::accept_hdr_async(
                stream,
                move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let mut headers = HashMap::new();
                for name in [
                    HEADER_API_KEY,
                    HEADER_DATE_MILLISECONDS,
                    HEADER_TIMEOUT_MS,
                    HEADER_PARENT_BLOCK_ROOT,
                    HEADER_REQUEST_AUTH,
                ] {
                    if let Some(value) = req.headers().get(name) {
                        headers.insert(
                            name.to_string(),
                            value.to_str().unwrap_or_default().to_string(),
                        );
                    }
                }
                *captured_headers_server.lock().unwrap() = Some(headers);
                Ok(response)
            })
            .await
            .unwrap();

            ws_stream
                .send(Message::Binary(frame(&first_bid_server)))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            ws_stream
                .send(Message::Binary(frame(&second_bid_server)))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            ws_stream.send(Message::Close(None)).await.ok();
        });

        let url: builder_types::BuilderUrl = format!("http://{addr}").parse().unwrap();
        let api_key = "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string();
        let target = StreamTarget {
            url,
            api_key: api_key.clone(),
            auth_data: RequestAuthData::new(b"test-auth".to_vec()).unwrap(),
            max_execution_payment: u64::MAX,
            min_bid: 0,
            builder_boost_factor: 100,
        };
        let builder_url_string = format!("http://{addr}");

        let duties_service = Arc::new(
            DutiesServiceBuilder::new()
                .validator_store(harness.validator_store.clone())
                .slot_clock(harness.slot_clock.clone())
                .beacon_nodes(harness.beacon_nodes.clone())
                .executor(harness.test_runtime.task_executor.clone())
                .spec(harness.spec.clone())
                .build()
                .unwrap(),
        );

        let service = HeaderStreamService::new(
            duties_service,
            harness.validator_store.clone(),
            BuilderStore::open_or_create(harness._validator_dir.path()).unwrap(),
            RequestAuthCache::default(),
            harness.beacon_nodes.clone(),
            harness.slot_clock.clone(),
            harness.test_runtime.task_executor.clone(),
        );
        let store = service.bid_store();
        store.open_context(slot, pubkey, parent_root, parent_hash);

        let slot_end = harness.slot_clock.now_duration().unwrap() + Duration::from_secs(30);
        service
            .run_stream(target, slot, parent_root, parent_hash, pubkey, slot_end)
            .await;

        server.await.unwrap();

        let best = store
            .best_for(slot, pubkey)
            .expect("expected a recorded bid");
        assert_eq!(
            best.bid, second_bid,
            "the latest frame must win, even with a lower value"
        );
        assert_eq!(best.builder_url, builder_url_string);

        let headers = captured_headers
            .lock()
            .unwrap()
            .clone()
            .expect("server should have captured handshake headers");
        assert_eq!(headers[HEADER_API_KEY], api_key);
        assert_eq!(
            headers[HEADER_PARENT_BLOCK_ROOT],
            format!("{parent_root:?}")
        );
        assert!(headers.contains_key(HEADER_DATE_MILLISECONDS));
        assert!(headers.contains_key(HEADER_REQUEST_AUTH));

        let expected_timeout = harness
            .slot_clock
            .start_of(slot)
            .unwrap()
            .as_millis()
            .saturating_add(500)
            .saturating_sub(harness.slot_clock.now_duration().unwrap().as_millis())
            .max(1) as u64;
        assert_eq!(headers[HEADER_TIMEOUT_MS], expected_timeout.to_string());
    }

    /// A `payload_attributes` event for a different slot of the same proposer (e.g. the next one,
    /// which the BN fires several times per slot for) must not abort or clear an already-open
    /// context for an earlier slot. `active` is keyed by `(Slot, PublicKeyBytes)`, so `reopen_for`
    /// — exactly what `handle_payload_attributes` calls once it decides a context needs
    /// (re)opening — must leave the earlier slot's entry alone when driven for a later slot of the
    /// same pubkey.
    #[tokio::test]
    async fn reopen_for_different_slot_does_not_clear_earlier_slot_context() {
        use crate::duties_service::DutiesServiceBuilder;
        use validator_test_rig::validator_client_harness::ValidatorClientHarness;

        let harness = ValidatorClientHarness::new(1).await;
        let pubkey = harness.pubkeys[0];
        let slot_a = Slot::new(5);
        let slot_b = Slot::new(6);
        let parent_root_a = Hash256::repeat_byte(0xaa);
        let parent_hash_a = ExecutionBlockHash::repeat_byte(0xbb);
        let parent_root_b = Hash256::repeat_byte(0xcc);
        let parent_hash_b = ExecutionBlockHash::repeat_byte(0xdd);

        let duties_service = Arc::new(
            DutiesServiceBuilder::new()
                .validator_store(harness.validator_store.clone())
                .slot_clock(harness.slot_clock.clone())
                .beacon_nodes(harness.beacon_nodes.clone())
                .executor(harness.test_runtime.task_executor.clone())
                .spec(harness.spec.clone())
                .build()
                .unwrap(),
        );

        // No builders (and so no `header_streams` targets) are configured: `reopen_for` takes its
        // "nothing to stream" branch, so this test needs no websocket server.
        let service = HeaderStreamService::new(
            duties_service,
            harness.validator_store.clone(),
            BuilderStore::open_or_create(harness._validator_dir.path()).unwrap(),
            RequestAuthCache::default(),
            harness.beacon_nodes.clone(),
            harness.slot_clock.clone(),
            harness.test_runtime.task_executor.clone(),
        );

        service
            .reopen_for(pubkey, slot_a, parent_root_a, parent_hash_a)
            .await;
        service.inner.store.record_bid(
            slot_a,
            pubkey,
            parent_root_a,
            parent_hash_a,
            "builder-a".to_string(),
            bid(slot_a, parent_hash_a, 5, 1),
            100,
        );
        assert!(service.bid_store().best_for(slot_a, pubkey).is_some());

        // The next slot's payload_attributes event fires for the same proposer.
        service
            .reopen_for(pubkey, slot_b, parent_root_b, parent_hash_b)
            .await;

        // Slot A's context and its recorded bid are untouched.
        assert_eq!(
            service.inner.store.context_parent(slot_a, pubkey),
            Some((parent_root_a, parent_hash_a))
        );
        assert!(service.bid_store().best_for(slot_a, pubkey).is_some());
        // Slot B's context is open on its own, independent entry.
        assert_eq!(
            service.inner.store.context_parent(slot_b, pubkey),
            Some((parent_root_b, parent_hash_b))
        );

        // Opening a different slot's context is not a parent-change "reopen" of anything.
        let reopened = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::REOPENED_PARENT_CHANGE],
        )
        .map(|c| c.get())
        .unwrap_or(0);
        assert_eq!(
            reopened, 0,
            "a different slot's context must not count as reopened_parent_change"
        );

        // A same-slot parent change for slot A still reopens and clears it, and does count.
        let other_parent_root_a = Hash256::repeat_byte(0xee);
        let other_parent_hash_a = ExecutionBlockHash::repeat_byte(0xff);
        service
            .reopen_for(pubkey, slot_a, other_parent_root_a, other_parent_hash_a)
            .await;
        assert!(service.bid_store().best_for(slot_a, pubkey).is_none());
        assert_eq!(
            service.inner.store.context_parent(slot_a, pubkey),
            Some((other_parent_root_a, other_parent_hash_a))
        );
        let reopened_after = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::REOPENED_PARENT_CHANGE],
        )
        .map(|c| c.get())
        .unwrap_or(0);
        assert_eq!(reopened_after, 1);

        // Slot B is still untouched by slot A's parent change.
        assert_eq!(
            service.inner.store.context_parent(slot_b, pubkey),
            Some((parent_root_b, parent_hash_b))
        );
    }

    /// A header-stream handshake that never completes (a stalled or unresponsive builder) must not
    /// hold `run_stream` open past `slot_end`. A listener that accepts the TCP
    /// connection but never answers the HTTP upgrade simulates exactly that; without the connect
    /// deadline this would hang forever, so the outer `tokio::time::timeout` turns a regression
    /// into a loud test failure instead of hanging the suite.
    #[tokio::test]
    async fn run_stream_connect_deadline_bounds_a_stalled_handshake() {
        use crate::duties_service::DutiesServiceBuilder;
        use validator_test_rig::validator_client_harness::ValidatorClientHarness;

        let harness = ValidatorClientHarness::new(1).await;
        let pubkey = harness.pubkeys[0];
        let slot = Slot::new(5);
        let parent_root = Hash256::ZERO;
        let parent_hash = ExecutionBlockHash::repeat_byte(0xbb);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accepts the TCP connection but never answers the HTTP upgrade: the handshake hangs
        // forever without a deadline.
        let _server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await
        });

        let target = StreamTarget {
            url: format!("http://{addr}").parse().unwrap(),
            api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
            auth_data: RequestAuthData::new(b"test-auth".to_vec()).unwrap(),
            max_execution_payment: u64::MAX,
            min_bid: 0,
            builder_boost_factor: 100,
        };

        let duties_service = Arc::new(
            DutiesServiceBuilder::new()
                .validator_store(harness.validator_store.clone())
                .slot_clock(harness.slot_clock.clone())
                .beacon_nodes(harness.beacon_nodes.clone())
                .executor(harness.test_runtime.task_executor.clone())
                .spec(harness.spec.clone())
                .build()
                .unwrap(),
        );

        let service = HeaderStreamService::new(
            duties_service,
            harness.validator_store.clone(),
            BuilderStore::open_or_create(harness._validator_dir.path()).unwrap(),
            RequestAuthCache::default(),
            harness.beacon_nodes.clone(),
            harness.slot_clock.clone(),
            harness.test_runtime.task_executor.clone(),
        );
        service
            .bid_store()
            .open_context(slot, pubkey, parent_root, parent_hash);

        // A short deadline, well inside the outer bound below, instead of the many-second slot
        // that `slot_end` would otherwise give.
        let slot_end = harness.slot_clock.now_duration().unwrap() + Duration::from_millis(200);

        let before = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::CONNECT_FAILED],
        )
        .map(|c| c.get())
        .unwrap_or(0);

        tokio::time::timeout(
            Duration::from_secs(10),
            service.run_stream(target, slot, parent_root, parent_hash, pubkey, slot_end),
        )
        .await
        .expect("run_stream must return once slot_end passes, not hang on a stalled handshake");

        let after = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::CONNECT_FAILED],
        )
        .map(|c| c.get())
        .unwrap_or(0);
        assert_eq!(
            after,
            before + 1,
            "the stalled handshake must be recorded as connect_failed"
        );
    }

    /// A server that closes the stream (a `Close` frame) before the VC's deadline to decide on a
    /// bid for this slot (`slot_start + 500 ms`, the same instant used for `x-timeout-ms`) is
    /// spec-following: MPBC docs and Helix cap the connection window at 3200 ms. `run_stream`
    /// must reopen exactly once for that, with a freshly recomputed `x-timeout-ms`, keeping
    /// whatever bid the first connection already stored; and must not reopen again if the
    /// reopened connection also closes early.
    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn run_stream_reopens_once_after_early_server_close() {
        use crate::duties_service::DutiesServiceBuilder;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use validator_test_rig::validator_client_harness::ValidatorClientHarness;

        let harness = ValidatorClientHarness::new(1).await;
        let pubkey = harness.pubkeys[0];
        let slot = Slot::new(5);
        let parent_root = Hash256::ZERO;
        let parent_hash = ExecutionBlockHash::repeat_byte(0xbb);

        // Exactly at the slot's start: the produce deadline (slot_start + 500 ms) is still ahead,
        // so the first connection's early close below is eligible for a reopen.
        harness.slot_clock.set_slot(slot.as_u64());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connection_count = Arc::new(AtomicUsize::new(0));
        let connection_count_server = connection_count.clone();

        let captured_timeouts: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_timeouts_1 = captured_timeouts.clone();
        let captured_timeouts_2 = captured_timeouts.clone();

        let the_bid = bid(slot, parent_hash, 10, 1);
        let the_bid_server = the_bid.clone();

        let (handshake1_done_tx, handshake1_done_rx) = tokio::sync::oneshot::channel();
        let (close1_tx, close1_rx) = tokio::sync::oneshot::channel();
        let (second_ready_tx, second_ready_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
        let (no_third_tx, no_third_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            // First connection: capture its x-timeout-ms (fixed the instant the handshake
            // request is sent), then wait for the test's go-ahead before sending the one valid
            // frame and closing immediately.
            let (stream, _) = listener.accept().await.unwrap();
            connection_count_server.fetch_add(1, Ordering::SeqCst);
            let mut ws_stream = tokio_tungstenite::accept_hdr_async(
                stream,
                move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    if let Some(value) = req.headers().get(HEADER_TIMEOUT_MS) {
                        captured_timeouts_1
                            .lock()
                            .unwrap()
                            .push(value.to_str().unwrap_or_default().to_string());
                    }
                    Ok(response)
                },
            )
            .await
            .unwrap();
            handshake1_done_tx.send(()).unwrap();
            close1_rx.await.unwrap();

            ws_stream
                .send(Message::Binary(frame(&the_bid_server)))
                .await
                .unwrap();
            ws_stream.send(Message::Close(None)).await.unwrap();
            drop(ws_stream);

            // Second connection: the reopen. Capture its (fresh) x-timeout-ms, let the test check
            // the bid store before any second frame is sent, then close without sending one.
            let (stream, _) = listener.accept().await.unwrap();
            connection_count_server.fetch_add(1, Ordering::SeqCst);
            let mut ws_stream = tokio_tungstenite::accept_hdr_async(
                stream,
                move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    if let Some(value) = req.headers().get(HEADER_TIMEOUT_MS) {
                        captured_timeouts_2
                            .lock()
                            .unwrap()
                            .push(value.to_str().unwrap_or_default().to_string());
                    }
                    Ok(response)
                },
            )
            .await
            .unwrap();

            second_ready_tx.send(()).unwrap();
            proceed_rx.await.unwrap();

            ws_stream.send(Message::Close(None)).await.unwrap();
            drop(ws_stream);

            // No third connection must ever be attempted.
            let third = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
            no_third_tx.send(third.is_err()).unwrap();
        });

        let url: builder_types::BuilderUrl = format!("http://{addr}").parse().unwrap();
        let target = StreamTarget {
            url,
            api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
            auth_data: RequestAuthData::new(b"test-auth".to_vec()).unwrap(),
            max_execution_payment: u64::MAX,
            min_bid: 0,
            builder_boost_factor: 100,
        };

        let duties_service = Arc::new(
            DutiesServiceBuilder::new()
                .validator_store(harness.validator_store.clone())
                .slot_clock(harness.slot_clock.clone())
                .beacon_nodes(harness.beacon_nodes.clone())
                .executor(harness.test_runtime.task_executor.clone())
                .spec(harness.spec.clone())
                .build()
                .unwrap(),
        );

        let service = HeaderStreamService::new(
            duties_service,
            harness.validator_store.clone(),
            BuilderStore::open_or_create(harness._validator_dir.path()).unwrap(),
            RequestAuthCache::default(),
            harness.beacon_nodes.clone(),
            harness.slot_clock.clone(),
            harness.test_runtime.task_executor.clone(),
        );
        let store = service.bid_store();
        store.open_context(slot, pubkey, parent_root, parent_hash);

        let slot_end = harness.slot_clock.now_duration().unwrap() + Duration::from_secs(30);

        let before_reopened = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::REOPENED_EARLY_CLOSE],
        )
        .map(|c| c.get())
        .unwrap_or(0);

        let client = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .run_stream(target, slot, parent_root, parent_hash, pubkey, slot_end)
                    .await;
            }
        });

        // The first handshake's x-timeout-ms is now fixed. Advance the clock (still well short
        // of the produce deadline) so the reopened handshake's recomputed x-timeout-ms visibly
        // differs, then let the server send the first frame and close.
        tokio::time::timeout(Duration::from_secs(5), handshake1_done_rx)
            .await
            .expect("the first handshake must complete")
            .unwrap();
        harness.slot_clock.advance_time(Duration::from_millis(100));
        close1_tx.send(()).unwrap();

        // The reopen has landed: check the bid store before any second frame has been sent.
        tokio::time::timeout(Duration::from_secs(5), second_ready_rx)
            .await
            .expect("the client must reconnect")
            .unwrap();
        let best = store
            .best_for(slot, pubkey)
            .expect("the first bid must survive the reopen");
        assert_eq!(best.bid, the_bid);

        proceed_tx.send(()).unwrap();

        client.await.unwrap();
        let no_third = tokio::time::timeout(Duration::from_secs(5), no_third_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(no_third, "a third connection must never be attempted");
        server.await.unwrap();

        assert_eq!(
            connection_count.load(Ordering::SeqCst),
            2,
            "exactly one reopen (two connections total) is expected"
        );

        let after_reopened = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::REOPENED_EARLY_CLOSE],
        )
        .map(|c| c.get())
        .unwrap_or(0);
        assert_eq!(after_reopened, before_reopened + 1);

        let timeouts = captured_timeouts.lock().unwrap().clone();
        assert_eq!(timeouts.len(), 2);
        assert_ne!(
            timeouts[0], timeouts[1],
            "the reopened handshake must carry a freshly computed x-timeout-ms"
        );
    }

    /// A close that happens at or after the produce deadline (`slot_start + 500 ms`) must not
    /// trigger a reopen: the VC can no longer use a streamed bid for this slot's proposal by
    /// then.
    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn run_stream_does_not_reopen_after_deadline() {
        use crate::duties_service::DutiesServiceBuilder;
        use validator_test_rig::validator_client_harness::ValidatorClientHarness;

        let harness = ValidatorClientHarness::new(1).await;
        let pubkey = harness.pubkeys[0];
        let slot = Slot::new(5);
        let parent_root = Hash256::ZERO;
        let parent_hash = ExecutionBlockHash::repeat_byte(0xbb);

        // A full slot past the slot's start: the produce deadline (slot_start + 500 ms) is long
        // gone by the time the connection below closes.
        harness.slot_clock.set_slot(slot.as_u64() + 1);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let the_bid = bid(slot, parent_hash, 10, 1);
        let the_bid_server = the_bid.clone();

        let (no_second_tx, no_second_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws_stream = tokio_tungstenite::accept_hdr_async(
                stream,
                |_req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    Ok(response)
                },
            )
            .await
            .unwrap();
            ws_stream
                .send(Message::Binary(frame(&the_bid_server)))
                .await
                .unwrap();
            ws_stream.send(Message::Close(None)).await.unwrap();
            drop(ws_stream);

            // No second connection must ever be attempted.
            let second = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
            no_second_tx.send(second.is_err()).unwrap();
        });

        let url: builder_types::BuilderUrl = format!("http://{addr}").parse().unwrap();
        let target = StreamTarget {
            url,
            api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
            auth_data: RequestAuthData::new(b"test-auth".to_vec()).unwrap(),
            max_execution_payment: u64::MAX,
            min_bid: 0,
            builder_boost_factor: 100,
        };

        let duties_service = Arc::new(
            DutiesServiceBuilder::new()
                .validator_store(harness.validator_store.clone())
                .slot_clock(harness.slot_clock.clone())
                .beacon_nodes(harness.beacon_nodes.clone())
                .executor(harness.test_runtime.task_executor.clone())
                .spec(harness.spec.clone())
                .build()
                .unwrap(),
        );

        let service = HeaderStreamService::new(
            duties_service,
            harness.validator_store.clone(),
            BuilderStore::open_or_create(harness._validator_dir.path()).unwrap(),
            RequestAuthCache::default(),
            harness.beacon_nodes.clone(),
            harness.slot_clock.clone(),
            harness.test_runtime.task_executor.clone(),
        );
        let store = service.bid_store();
        store.open_context(slot, pubkey, parent_root, parent_hash);

        let slot_end = harness.slot_clock.now_duration().unwrap() + Duration::from_secs(30);

        let before_reopened = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::REOPENED_EARLY_CLOSE],
        )
        .map(|c| c.get())
        .unwrap_or(0);

        service
            .run_stream(target, slot, parent_root, parent_hash, pubkey, slot_end)
            .await;

        let no_second = tokio::time::timeout(Duration::from_secs(5), no_second_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            no_second,
            "a close after the produce deadline must not reopen the stream"
        );
        server.await.unwrap();

        let after_reopened = validator_metrics::get_int_counter(
            &validator_metrics::BUILDER_HEADER_STREAM_CONNECTIONS_TOTAL,
            &[validator_metrics::REOPENED_EARLY_CLOSE],
        )
        .map(|c| c.get())
        .unwrap_or(0);
        assert_eq!(after_reopened, before_reopened);

        // The frame received before the (too-late) close is still recorded.
        assert!(store.best_for(slot, pubkey).is_some());
    }
}
