# BeaconChain Architecture

## Goal

Enable a productive LLM-assisted development workflow where humans
focus on architecture, specifications, and correctness review, while
LLMs handle component implementation, test generation, and fast
iteration — without compromising correctness, performance, or liveness.

The modular architecture is the prerequisite. Today's `BeaconChain<T>`
god object (40+ fields, 200+ methods) makes this impossible — an LLM
can't safely work on one concern without understanding everything.
Breaking it into focused components with clean boundaries is what makes
the workflow viable.

### What this unlocks

- **Components are independently testable** — construct with `::new()`,
  pass in test state, assert results. No `BeaconChainHarness` needed.
- **LLMs can implement against specs** — a component with explicit
  parameters and no hidden state fetching is something an LLM can
  reason about and implement correctly.
- **Fast iteration** — change a component's internals without risking
  the rest of the system. Test in isolation, then integration.
- **Spec-driven development** — scenario specs for behavior alignment,
  formal specs for mathematically proven invariants. Both feed into
  LLM-generated implementations.

### What stays the same

Correctness, performance, and liveness guarantees. This restructuring
changes how code is organized and who writes it. It does not change
runtime characteristics.

## Overview

`BeaconChain<T>` is fully replaced by focused components. Components own
state and logic. Callers (HTTP API, NetworkBeaconProcessor, Sync Manager)
hold `Arc` refs to components but contain no business logic of their own.
Complex workflows like block import and block production remain as
`impl BeaconChain<T>` methods but are organized into separate files:

- `block_import_methods.rs` — chain segment processing, blob/data column
  handling, availability checks, and the core `import_block` pipeline
- `block_production/` — state loading, partial block assembly, payload
  integration, and block completion
- `execution_methods.rs` — execution engine forkchoice updates, proposer
  preparation, and optimistic status queries

```
Builder (startup)
  │
  ├── OperationsManager ──── Arc ──┬── NetworkBeaconProcessor
  │                                └── HTTP API Context
  │
  ├── AttestationManager ─── Arc ──┬── NetworkBeaconProcessor
  │                                └── HTTP API Context
  │
  ├── SyncCommitteeManager ─ Arc ──── NetworkBeaconProcessor
  │
  ├── DataAvailabilityMgr ── Arc ──┬── NetworkBeaconProcessor
  │                                └── Sync Manager
  │
  ├── CanonicalHead ──────── Arc ──┬── NetworkBeaconProcessor
  │                                └── HTTP API Context
  │
  ├── ExecutionManager ───── Arc ──── HTTP API Context
  │
  └── ValidatorQueryService ─ Arc ── HTTP API Context
```

Callers are bags of refs — they hold dependencies but don't implement
logic. Handler functions pull the specific refs they need from the
caller struct.

## Core Pattern: Separate Business Logic from Infrastructure

Components own verification logic and state. Infrastructure like chain
state, current slot, and event dispatch is provided by the caller —
either as values, references, or callbacks.

Most verification follows a three-step pattern:

1. **Fetch state** — caller gets head state, current epoch, fork choice data
2. **Verify/process** — component runs business logic against that state
3. **Mutate owned data** — component updates observed_*, pools, caches

Components own steps 2 and 3. Callers handle step 1.

```rust
// Component: pure logic + owned state, receives context as params
impl<E: EthSpec> OperationsManager<E> {
    pub fn verify_voluntary_exit(
        &self,
        exit: SignedVoluntaryExit,
        head_state: &BeaconState<E>,
        wall_clock_epoch: Epoch,
    ) -> Result<ObservationOutcome<SignedVoluntaryExit, E>> {
        self.observed_voluntary_exits
            .lock()
            .verify_and_observe_at(exit, wall_clock_epoch, head_state, &self.spec)
    }
}

// Caller (e.g. gossip handler): fetches state, calls component
fn handle_voluntary_exit(
    operations: &OperationsManager<E>,
    canonical_head: &CanonicalHead<T>,
    slot_clock: &T::SlotClock,
    event_handler: &Option<ServerSentEventHandler<E>>,
    exit: SignedVoluntaryExit,
) -> Result<()> {
    let head = canonical_head.cached_head();
    let epoch = slot_clock.now().unwrap().epoch(E::slots_per_epoch());
    let outcome = operations.verify_voluntary_exit(
        exit, &head.snapshot.beacon_state, epoch,
    )?;
    if let ObservationOutcome::New(ref verified) = outcome {
        if let Some(handler) = event_handler {
            handler.register(EventKind::VoluntaryExit(verified.clone().into_inner()));
        }
    }
    Ok(())
}
```

## Components

### `OperationsManager<E: EthSpec>`

Voluntary exits, proposer slashings, attester slashings, BLS-to-execution
changes. Verification, deduplication, and op pool insertion.

**Owns:** `observed_voluntary_exits`, `observed_proposer_slashings`,
`observed_attester_slashings`, `observed_bls_to_execution_changes`

**Holds:** `spec`, `op_pool`

### `SyncCommitteeManager<E: EthSpec>`

Sync committee message and contribution verification, aggregation pool.

**Owns:** `naive_sync_aggregation_pool`, `observed_sync_contributions`,
`observed_sync_contributors`, `observed_sync_aggregators`

**Holds:** `spec`, `op_pool`

### `AttestationManager<E: EthSpec>`

Attestation production, verification, aggregation, pool management.

**Owns:** `naive_aggregation_pool`, `observed_attestations`,
`observed_gossip_attesters`, `observed_block_attesters`,
`observed_aggregators`, `early_attester_cache`, `shuffling_cache`

**Holds:** `spec`, `op_pool`, `genesis_block_root`

### `DataAvailabilityManager<T: BeaconChainTypes>`

Blob and data column processing, custody, DA boundary calculations.

**Owns:** `data_availability_checker`, `observed_blob_sidecars`,
`observed_column_sidecars`, `kzg`, `rng`

**Holds:** `spec`, `store`, `task_executor`

### `ExecutionManager<T: BeaconChainTypes>`

Execution layer integration, proposer preparation, forkchoice updates.

**Owns:** `beacon_proposer_cache`, `fork_choice_signal_tx/rx`

**Holds:** `spec`, `execution_layer`

### `ValidatorQueryService<T: BeaconChainTypes>`

Validator pubkey lookups, committee cache access.

**Note:** This is the one component generic over `T: BeaconChainTypes`
rather than `E: EthSpec`, because `ValidatorPubkeyCache` requires store
access for persistence. This is a known exception to the general pattern.

**Owns:** `validator_pubkey_cache`

**Holds:** `spec`

### Unmapped fields

Block import caches (`block_times_cache`, `envelope_times_cache`,
`pre_finalization_block_cache`, `observed_block_producers`,
`observed_slashable`) remain on `BeaconChain` directly. They are
tightly coupled to `block_import_methods.rs` and `canonical_head.rs`,
which access them through `&self`/`&chain`.

Several `BeaconChain<T>` fields don't have a clear home in the above
components and need further design work:

- `config: ChainConfig` — referenced 20+ times across every domain
  (re-org settings, builder fallback thresholds, sync tolerance, light
  client toggle, payload preparation). Needs partitioning into
  per-component config structs or a shared read-only reference.
- `light_client_server_cache`, `light_client_server_tx` — used during
  block import and head recomputation. Cross-cutting.
- `store_migrator` (BackgroundMigrator) — triggered by finalization.
- `graffiti_calculator` — used in block production.
- `pending_payload_envelopes` — ePBS-related.
- `shutdown_sender` — used in error paths across multiple components.
- `genesis_state_root`, `genesis_validators_root`, `genesis_time`,
  `genesis_backfill_slot` — scattered usage, no clear single owner.

## Design Principles

### 1. Favour composition over god objects

Workflows compose the specific capabilities they need — not whole
components. Pass what you need, not what _has_ what you need.

**Example: block production.** Today `produce_block_on_state` accesses
`self.*` for everything because the god object makes it easy. Tracing
what it actually uses reveals 3 domain dependencies out of 40+ fields:

| Dependency | What it uses |
|-----------|-------------|
| `&OperationPool` | Pull attestations, exits, slashings, sync aggregate |
| `CanonicalHead` (read) | Head slot, finalized checkpoint, forkchoice params |
| `ExecutionLayer` | Fee recipient, gas limit, `get_payload` |

The remaining dependencies are infrastructure (`&ChainSpec`, `SlotClock`,
`TaskExecutor`, `BlockProductionConfig`) that every async workflow needs.

```rust
/// Block production composed from thin slices.
/// No Arc<BeaconChain>, no god object.
pub async fn produce_block(
    state: BeaconState<E>,
    // Domain dependencies (the actual coupling)
    op_pool: &OperationPool<E>,
    canonical_head: &CanonicalHead<T>,
    execution_layer: &ExecutionLayer<E>,
    // Infrastructure
    spec: &ChainSpec,
    config: &BlockProductionConfig,
    slot_clock: &T::SlotClock,
) -> Result<BeaconBlock<E>> {
    let attestations = op_pool.get_attestations(&state, spec)?;
    let (slashings, exits) = op_pool.get_slashings_and_exits(&state, spec);
    let health = check_chain_health(canonical_head, slot_clock, config);
    let payload = execution_layer.get_payload(
        canonical_head.forkchoice_update_params(),
        health.use_builder(),
    ).await?;
    assemble_block(state, attestations, slashings, exits, payload, spec)
}
```

This applies to other complex call sites — block import, the HTTP API,
sync — each composes the thin slices it needs.

### 2. Separate business logic from infrastructure

Components own verification logic and state. Infrastructure like chain
state, current slot, and event dispatch is provided by the caller —
either as values, references, or callbacks.

For example, the caller could read `slot_clock.now()` and pass the
resulting `Epoch` in, or pass a callback for mid-workflow side effects
like SSE events.

**Known hard case:** During `import_block`, `early_attester_cache`
insertion happens while holding the fork choice write lock, and SSE head
events are emitted in the same critical section. Moving side effects to
the caller requires careful design to preserve lock-scope semantics —
the caller must replicate the context (e.g., computing `dependent_root`
from state) that today exists only inside the locked section. A callback
pattern could address this.

### 3. Components are testable in isolation

You should be able to construct a component, pass in test state, and
assert results — without `BeaconChainHarness`, store, or fork choice.

### 4. Fork choice write locks are concentrated

Fork choice write locks are acquired only by:

- **Block import** — `fork_choice.on_block()` during `import_block`
  (`beacon_chain.rs:3896`)
- **Head recomputation** — `fork_choice.get_head()` in
  `recompute_head_inner` (`canonical_head.rs:616`) and
  `fork_choice.prune()` in `after_finalization`
  (`canonical_head.rs:1059`)
- **Attestation application** — `fork_choice.on_attestation()` for
  gossip attestations (`beacon_chain.rs:2305`)
- **Execution layer callbacks** — `fork_choice.on_invalid_execution_payload()`
  and `fork_choice.on_valid_execution_payload()` when the EL reports
  INVALID/VALID asynchronously (`beacon_chain.rs:5791`, `6149`)
- **Attester slashing import** — `fork_choice.on_attester_slashing()`
  for standalone gossip slashings (`beacon_chain.rs:2650`)

Components like OperationsManager, SyncCommitteeManager, and
ValidatorQueryService never touch fork choice locks. No fork choice
write locks exist outside the `beacon_chain` crate — the HTTP API,
network, and sync layers never acquire them directly.

This preserves the lock ordering documented in `canonical_head.rs`.

## Testing

### Unit tests — components directly

Components are directly testable without `BeaconChainHarness`:

```rust
#[test]
fn rejects_exit_for_unknown_validator() {
    let spec = Arc::new(ChainSpec::mainnet());
    let ops = OperationsManager::<MinimalEthSpec>::new(
        spec.clone(),
        Arc::new(OperationPool::new()),
    );
    let (state, _keypairs) = create_genesis_state(&spec, 4);
    let exit = make_exit_for_validator(999);
    assert!(ops.verify_voluntary_exit(exit, &state, Epoch::new(0)).is_err());
}
```

Composition also enables testing complex workflows directly:

```rust
#[tokio::test]
async fn block_includes_available_exits() {
    let spec = ChainSpec::mainnet();
    let (state, keypairs) = create_genesis_state(&spec, 16);

    let op_pool = OperationPool::new();
    let exit = make_signed_exit(&keypairs[0], 0, Epoch::new(5), &spec);
    op_pool.insert_voluntary_exit(exit.clone());

    let mock_el = MockExecutionLayer::default_payload();
    let mock_head = MockCanonicalHead::at_slot(10);

    let block = produce_block(
        state, &op_pool, &mock_head, &mock_el,
        &spec, &BlockProductionConfig::default(), &test_clock(),
    ).await.unwrap();

    assert_eq!(block.body().voluntary_exits().len(), 1);
    assert_eq!(block.body().voluntary_exits()[0], exit);
}
```

### Acceptance tests — event-based via sync manager

The existing `TestRig` infrastructure in the sync manager enables
event-based acceptance tests for component interactions and full
workflows without needing `BeaconChainHarness`.

### Integration tests — existing harness

`BeaconChainHarness` tests continue to work for end-to-end workflows.
They test the full call path including state fetching and side effects.

## Caller Dependency Injection

Callers hold refs to components but no logic of their own. Handler
functions pull the specific refs they need:

```rust
// NetworkBeaconProcessor — holds refs, no business logic
struct NetworkBeaconProcessor<T: BeaconChainTypes> {
    operations:        Arc<OperationsManager<T::EthSpec>>,
    attestations:      Arc<AttestationManager<T::EthSpec>>,
    sync_committee:    Arc<SyncCommitteeManager<T::EthSpec>>,
    data_availability: Arc<DataAvailabilityManager<T>>,
    canonical_head:    Arc<CanonicalHead<T>>,
    slot_clock:        T::SlotClock,
    event_handler:     Option<ServerSentEventHandler<T::EthSpec>>,
    // Holds refs — handler functions contain the logic
}

// HTTP API Context — same pattern, different subset
struct Context<T: BeaconChainTypes> {
    operations:        Arc<OperationsManager<T::EthSpec>>,
    attestations:      Arc<AttestationManager<T::EthSpec>>,
    canonical_head:    Arc<CanonicalHead<T>>,
    execution:         Arc<ExecutionManager<T>>,
    validator_query:   Arc<ValidatorQueryService<T::EthSpec>>,
    spec:              Arc<ChainSpec>,
    slot_clock:        T::SlotClock,
    config:            ChainConfig,
    // Holds refs — route handlers contain the logic
}
```

No single caller holds all components. Each holds only the refs its
handler functions need.
