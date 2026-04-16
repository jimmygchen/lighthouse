# BeaconChain Architecture

This document describes the modular architecture of the `beacon_chain` crate.

## Overview

Components are the primary units. They own their state, are independently
testable, and are shared across callers via `Arc`. There is no central god
object — callers receive only the components they need.

```
Builder (startup)
  │
  ├── OperationsManager ──── Arc ──┬── NetworkBeaconProcessor
  │                                └── HTTP API (pool endpoints)
  │
  ├── AttestationManager ─── Arc ──┬── NetworkBeaconProcessor
  │                                └── HTTP API (attestation endpoints)
  │
  ├── DataAvailabilityMgr ── Arc ──┬── NetworkBeaconProcessor
  │                                └── Sync manager
  │
  ├── CanonicalHead ──────── Arc ──┬── NetworkBeaconProcessor
  │                                ├── HTTP API
  │                                └── BlockWorkflow
  │
  ├── BlockWorkflow ──────── Arc ──── NetworkBeaconProcessor
  │   (holds refs to DA, Attestation, etc. for import_block)
  │
  └── (other components...)
```

After startup, components live independently. No single struct holds
everything. Each caller holds `Arc` refs to only what it uses.

## Core Pattern: Separate Verification from State Fetching

Most verification follows a three-step pattern:

1. **Fetch state** — get head state, wall clock epoch, fork choice data
2. **Verify/process** — run business logic against that state
3. **Mutate owned data** — update observed_*, pools, caches

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

### `ValidatorQueryService<E: EthSpec>`

Validator pubkey lookups, committee cache access.

**Owns:** `validator_pubkey_cache`

**Holds:** `spec`

### `BlockWorkflow<T: BeaconChainTypes>`

Block verification, import, and the cross-component coordination that
`import_block` requires. This is the only place that orchestrates across
multiple components, because block import genuinely needs it.

**Owns:** `block_times_cache`, `envelope_times_cache`,
`pre_finalization_block_cache`, `observed_block_producers`,
`observed_slashable`

**Holds:** `spec`, `store`, `slot_clock`, `canonical_head`, `op_pool`,
`task_executor`, `execution_layer`, `event_handler`, `validator_monitor`,
`slasher`, `genesis_block_root`

**Also holds refs to other components:**
- `attestations: Arc<AttestationManager<E>>` (early attester cache updates)
- `data_availability: Arc<DataAvailabilityManager<T>>` (DA checks)

This is one-directional (BlockWorkflow → others, no cycles).

### Unmapped fields

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

## Rules

### 1. Components are generic over `E: EthSpec`, not `T: BeaconChainTypes`

Unless a component genuinely needs store access or the slot clock type,
use `E: EthSpec`. This gives simpler generics and faster compilation.

Exceptions: `DataAvailabilityManager`, `ExecutionManager`, and
`BlockWorkflow` need `T`.

### 2. Fork choice write locks are concentrated in well-defined paths

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

### 3. Components are side-effect-free

Components don't hold `event_handler` or `SlotClock`. They don't emit
SSE events or fetch the current time. Import methods return data that the
caller uses for side effects (SSE events, metrics, etc.).

**Known hard case:** During `import_block`, `early_attester_cache`
insertion happens while holding the fork choice write lock, and SSE head
events are emitted in the same critical section. Moving side effects to
the caller requires careful design to preserve lock-scope semantics —
the caller must replicate the context (e.g., computing `dependent_root`
from state) that today exists only inside the locked section.

### 4. State is passed as parameters, not fetched internally

Components receive `&BeaconState<E>`, `&CachedHead<E>`, `Epoch`, or
`bool` flags as method parameters. They never hold `CanonicalHead<T>` or
`BeaconStore<T>`. The caller fetches state and passes it in.

This is what makes components directly testable — construct a test state,
pass it as a parameter, no chain infrastructure needed.

### 5. Favour composition over god objects

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

This applies to other complex call sites (HTTP API, sync) — each
composes the thin slices it needs rather than holding `Arc<BeaconChain>`.

## Testing

### Unit tests (AI writes, fast)

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

No harness, no store, no fork choice. Runs in under a second.

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

### BDD specs (humans write)

```gherkin
Feature: Voluntary Exit Verification

  Scenario: Valid exit is accepted
    Given a genesis state with 8 validators
    When validator 0 submits a voluntary exit at epoch 5
    Then the exit is accepted as new

  Scenario: Duplicate exit is ignored
    Given a genesis state with 8 validators
    And validator 0 has already submitted a voluntary exit
    When validator 0 submits the same exit again
    Then the exit is marked as already known

  Scenario: Exit for unknown validator is rejected
    Given a genesis state with 4 validators
    When validator 999 submits a voluntary exit
    Then the exit is rejected with an error
```

AI implements the component against the spec. Humans review the spec only.

### Integration tests (existing, still work)

`BeaconChainHarness` tests continue to work for end-to-end workflows.
They test the full call path including state fetching and side effects.

## Caller Dependency Injection

Callers receive only the components they need, injected at construction:

```rust
// NetworkBeaconProcessor — holds refs to what it uses
struct NetworkBeaconProcessor<T: BeaconChainTypes> {
    operations:        Arc<OperationsManager<T::EthSpec>>,
    attestations:      Arc<AttestationManager<T::EthSpec>>,
    sync_committee:    Arc<SyncCommitteeManager<T::EthSpec>>,
    data_availability: Arc<DataAvailabilityManager<T>>,
    block_workflow:    Arc<BlockWorkflow<T>>,
    canonical_head:    Arc<CanonicalHead<T>>,
    slot_clock:        T::SlotClock,
    event_handler:     Option<ServerSentEventHandler<T::EthSpec>>,
    // NOT the full BeaconChain — only what it needs
}
```

HTTP API route groups similarly receive only their relevant components.
No single caller holds all components.
