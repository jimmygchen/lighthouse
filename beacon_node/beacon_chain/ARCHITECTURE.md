# BeaconChain, Modularised

Decomposing `BeaconChain<T>` into testable components — designed for fast
iteration and human-AI collaboration.

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
  feeding directly into LLM-generated implementations and tests.

### What stays the same

Correctness, performance, and liveness guarantees. This restructuring
changes how code is organized and who writes it. It does not change
runtime characteristics.

## Overview

`BeaconChain<T>` is replaced by focused components. Components own state
and logic. Callers (HTTP API, NetworkBeaconProcessor, Sync Manager) hold
`Arc` refs to components but contain no business logic of their own.

The top-level type `BeaconChain<T>` holds the component `Arc`s and
coordinates startup. Block import and block production live on two
scoped orchestrators (`BlockImporter<T>`, `BlockProducer<T>`) whose
methods use `&self` over their owned `Arc` refs.

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

### Orchestrators

Block import and block production are handled by two scoped
orchestrators that own their `Arc` refs and use `&self` methods:

- **`BlockImporter<T>`** — block, blob, and data-column import.
  Owns `observed_block_producers`, `observed_slashable`,
  `event_handler`, `validator_monitor`. Holds `Arc` refs to
  `canonical_head`, `attestation_manager`, `data_availability_manager`,
  etc.

- **`BlockProducer<T>`** — state loading, partial block assembly,
  execution payload integration. Holds `Arc` refs to `op_pool`,
  `canonical_head`, `execution_manager`, `attestation_manager`, etc.

### Duplicate fields on BeaconChain

`event_handler` and `validator_monitor` are Arc-cloned onto both
`BeaconChain<T>` and `BlockImporter<T>`. They remain on `BeaconChain`
because 25+ call sites each in `http_api`, `network`,
`canonical_head`, `block_verification`, `execution_methods`,
`state_advance_timer`, `metrics`, and `attestation_simulator` access
them directly. Consolidating all callers to route through
`block_importer` would be too invasive.

### Remaining unmapped fields

Several fields on `BeaconChain<T>` don't have a clear single owner:

- `config: ChainConfig` — referenced 20+ times across every domain.
- `light_client_server_cache`, `light_client_server_tx` — cross-cutting.
- `store_migrator` (BackgroundMigrator) — triggered by finalization.
- `shutdown_sender` — used in error paths across multiple components.
- `genesis_state_root`, `genesis_validators_root`, `genesis_time`,
  `genesis_backfill_slot` — scattered usage.

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
/// BlockProducer owns its Arc refs; methods use &self.
/// No Arc<BeaconChain>, no god object.
pub struct BlockProducer<T: BeaconChainTypes> {
    spec: Arc<ChainSpec>,
    op_pool: Arc<OperationPool<T::EthSpec>>,
    canonical_head: Arc<CanonicalHead<T>>,
    execution_manager: Arc<ExecutionManager<T>>,
    // ...
}

impl<T: BeaconChainTypes> BlockProducer<T> {
    pub async fn produce_block_on_state(
        &self,
        state: BeaconState<T::EthSpec>,
        produce_at_slot: Slot,
        randao_reveal: Signature,
        // ...
    ) -> Result<BeaconBlockResponseWrapper<T::EthSpec>> {
        let attestations = self.op_pool.get_attestations(&state, &self.spec)?;
        let health = is_healthy(&self.canonical_head, /* ... */)?;
        // ...
    }
}
```

The same pattern applies to `BlockImporter<T>` — it owns its `Arc` refs
and methods use `&self` to import blocks, blobs, and data columns.

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

Integration-level testing of `BlockImporter<T>` and `BlockProducer<T>`
uses the existing `BeaconChainHarness`, which wires up the full chain
including orchestrators.

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
    block_importer:    Arc<BlockImporter<T>>,
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

## Results

Seven components extracted plus two scoped orchestrators
(`BlockImporter<T>`, `BlockProducer<T>`). ~2,900 lines of new unit
tests (104 tests), full CI green, and a local testnet that produces
blocks and finalises. The top-level type (`BeaconChain<T>`) shrank
from 7,317 to 125 lines and lost its 200+ methods.

### Wins

- **Seven components extracted; four cohesive.**
  `OperationsManager`, `AttestationManager`, `SyncCommitteeManager`, and
  `ValidatorQueryService` came out as self-contained units with clear
  ownership of their observed sets and pools. `DataAvailabilityManager`,
  `ExecutionManager` extracted too but are thinner wrappers.
- **Two scoped orchestrators.**
  `BlockImporter<T>` and `BlockProducer<T>` own their `Arc` refs and
  use `&self` methods, replacing the previous `*Context` struct literal
  pattern.
- **~2,900 lines of new unit tests** (104 tests across 7 component
  test files). Six component test files construct components directly
  without `BeaconChainHarness`; `BlockImporter` tests use the harness
  for integration-level coverage.
- **Top-level file shrank from 7,317 to 125 lines.** `beacon_chain.rs`
  now contains only the struct, trait, type aliases, and error conversions.
  All types, enums, constants, and functions moved to owning modules.

### Remaining work

- **`store_migrator`** on `BeaconChain<T>` — the only non-component field
  left. Could be moved into the store layer.
- **`chain()` back-references** — orchestrators hold a `Weak<BeaconChain<T>>`
  and expose `chain()` to upgrade it. 5 call sites remain (4 in
  `BlockImporter`, 1 in `BlockProducer`), all used to pass
  `Arc<BeaconChain<T>>` into cross-module verification helpers
  (`block_verification`, `blob_verification`, etc.) that still take
  `&BeaconChain<T>`. Rewriting those signatures to accept narrow deps
  would eliminate the back-reference entirely.

### Key metrics

- **Top-level file:** 7,317 → 125 lines (struct + trait + aliases only)
- **Top-level fields:** 40+ → 22 (9 component Arcs, 5 infra, 5 genesis, 2 shared Arcs, 1 migrator)
- **Top-level methods:** 200+ → 0
- **Components + orchestrators:** 7 + 2 (`BlockImporter`, `BlockProducer`)
- **New unit tests:** ~2,900 lines (104 tests across 7 component test files)

### Verification

- **CI:** `cargo check --workspace --tests --release` green, `make lint`
  clean, `cargo fmt` applied.
- **Fulu tests:** 425/425 passed (`FORK_NAME=fulu`, release mode).
- **Kurtosis local testnet:** 4-BN mesh, 27-hour run (slot 30,969–32,530).
  0 errors, 0 panics, 0 reorgs. Block publish delay avg ~2 ms, import
  delay avg ~80 ms.
- **Coverage:** report at http://204.168.250.92:8082/ (same command as
  unstable baseline).
