# BeaconChain Architecture

This document describes the modular architecture of the `beacon_chain` crate.

## Overview

`BeaconChain<T>` is the composition root — a thin orchestrator that holds
components and shared infrastructure. Business logic lives in components.

```
┌──────────────────────────────────────────────────────────────┐
│                     BeaconChain<T>                            │
│  Orchestrator — fetches state, calls components, coordinates │
│                                                              │
│  Components (own their mutable state):                       │
│    operations:       OperationsManager<E>                    │
│    sync_committee:   SyncCommitteeManager<E>                 │
│    attestations:     AttestationManager<E>                   │
│    data_availability: DataAvailabilityManager<T>             │
│    execution:        ExecutionManager<T>                     │
│    validators:       ValidatorQueryService<E>                │
│                                                              │
│  Shared infra (state fetching, used by orchestration):       │
│    canonical_head, store, slot_clock, spec, ...              │
│                                                              │
│  Hard methods (stay here, use components internally):        │
│    import_block, process_block, recompute_head, ...          │
└──────────────────────────────────────────────────────────────┘
```

## Core Pattern: Separate Verification from State Fetching

Most methods follow a three-step pattern:

1. **Fetch state** — get head state, wall clock state, fork choice data
2. **Verify/process** — run business logic against that state
3. **Mutate owned data** — update observed_*, pools, caches

Components own steps 2 and 3. BeaconChain orchestrates step 1.

```rust
// Component: pure logic + owned state, receives context as params
impl<E: EthSpec> OperationsManager<E> {
    pub fn verify_voluntary_exit(
        &self,
        exit: SignedVoluntaryExit,
        head_state: &BeaconState<E>,       // provided by caller
        wall_clock_epoch: Epoch,            // provided by caller
    ) -> Result<SigVerifiedOp<SignedVoluntaryExit, E>> {
        self.observed_voluntary_exits.verify(&exit)?;
        exit.validate(head_state, &self.spec)?;
        Ok(SigVerifiedOp::new(exit))
    }
}

// BeaconChain: fetches state, delegates to component
impl<T: BeaconChainTypes> BeaconChain<T> {
    pub fn verify_voluntary_exit_for_gossip(
        &self,
        exit: SignedVoluntaryExit,
    ) -> Result<SigVerifiedOp<SignedVoluntaryExit, T::EthSpec>> {
        let head = self.canonical_head.cached_head();
        let epoch = self.epoch()?;
        self.operations.verify_voluntary_exit(exit, &head.snapshot.beacon_state, epoch)
    }
}
```

## Components

### `OperationsManager<E: EthSpec>`

Voluntary exits, proposer slashings, attester slashings, BLS-to-execution
changes.

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

## Rules

### 1. Components are generic over `E: EthSpec`, not `T: BeaconChainTypes`

Unless a component genuinely needs store access or the slot clock type,
use `E: EthSpec`. This gives simpler generics and faster compilation.

Exceptions: `DataAvailabilityManager` and `ExecutionManager` need `T`.

### 2. Components never acquire fork choice write locks

Only BeaconChain orchestration methods (`import_block`, `recompute_head`,
`import_attester_slashing`) acquire fork choice write locks. This preserves
the lock ordering documented in `canonical_head.rs`.

### 3. Components never touch `event_handler`

Import methods return event data. BeaconChain orchestration registers SSE
events. This keeps components pure and side-effect-free.

### 4. Components don't hold `SlotClock`

Time-dependent values are passed as parameters:
- `wall_clock_epoch: Epoch`
- `is_post_capella: bool`
- `wall_clock_state: &BeaconState<E>`

### 5. State is passed as parameters, not fetched internally

Components receive `&BeaconState<E>`, `&CachedHead<E>`, or computed values
as method parameters. They never hold `CanonicalHead<T>` or `BeaconStore<T>`.

## Testing

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

`BeaconChainHarness` integration tests continue to test the orchestration
layer (BeaconChain methods that fetch state and call components).

## What Stays on BeaconChain

- `slot()`, `epoch()` — convenience methods
- `import_block()`, `process_block()` — lock-sensitive, 14+ field accesses
- `recompute_head()` — fork choice write lock
- `per_slot_task()` — cross-component orchestration
- `persist_*()` — persistence coordination
- State query methods — orchestration (fetch from store, advance, return)
- Block production — already in `block_production/` module
- `Drop` impl
