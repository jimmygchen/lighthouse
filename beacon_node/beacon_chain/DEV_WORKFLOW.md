# BeaconChain Development Workflow

How to develop against the modularized `beacon_chain` crate.
For the full architecture vision, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Component-First Development

New features go into components, not `impl BeaconChain<T>`.

1. **Identify the domain.** Which component owns this concern?
   (See [Existing Components](#existing-components) below.)
2. **Write the component method.** It takes explicit parameters -- no
   hidden state fetching, no `&self` access to the god object.
3. **Write unit tests.** Construct the component with `::new()`, pass
   test state, assert results. No `BeaconChainHarness` needed.
4. **Wire integration.** The caller (HTTP API handler,
   `NetworkBeaconProcessor`, or an `impl BeaconChain<T>` orchestration
   method) fetches infrastructure state and calls the component.

### Adding a New Method

**Never** add business logic directly to `impl BeaconChain<T>`.

| Scenario | Where to put it |
|----------|----------------|
| Logic that operates on a component's owned state | Component method |
| Orchestration that composes multiple components | Free function with a context struct |
| Cross-cutting infrastructure (per-slot tasks, health checks) | `impl BeaconChain<T>` (rare, justify in PR) |

### Free Functions and Context Structs

For orchestration that touches multiple components but does not need
`&self` state, write a free function and pass a context struct:

```rust
// Define a context struct with explicit dependencies
pub(crate) struct MyContext<'a, T: BeaconChainTypes> {
    pub canonical_head: &'a CanonicalHead<T>,
    pub attestation_manager: &'a AttestationManager<T::EthSpec>,
    pub spec: &'a ChainSpec,
    // ... only what you need
}

impl<'a, T: BeaconChainTypes> MyContext<'a, T> {
    /// Convenience constructor for callers that still hold &BeaconChain.
    pub fn from_chain(chain: &'a BeaconChain<T>) -> Self {
        Self {
            canonical_head: &chain.canonical_head,
            attestation_manager: &chain.attestation_manager,
            spec: &chain.spec,
        }
    }
}

/// Free function -- testable by constructing MyContext directly.
pub(crate) fn do_something<T: BeaconChainTypes>(
    ctx: &MyContext<'_, T>,
    input: SomeInput,
) -> Result<SomeOutput, Error> {
    // ...
}
```

The `from_chain` constructor enables incremental migration: existing
callers that hold `&BeaconChain` can call `MyContext::from_chain(chain)`
today. New callers construct the context from individual component refs.

## Context Structs

Four context structs currently exist. Each makes a domain's dependencies
explicit and enables testing without `BeaconChainHarness`.

| Struct | File | Purpose |
|--------|------|---------|
| `AttestationVerificationContext` | `attestation_verification.rs` | All attestation verification functions |
| `BlockImportContext` | `block_import_methods.rs` | Block import helper free functions |
| `BlockProductionContext` | `block_production/mod.rs` | Block production helper free functions |
| `ExecutionOrchestrationContext` | `execution_methods.rs` | Execution layer orchestration free functions |

All four have a `from_chain` constructor for backward compatibility.

## Testing Pattern

Components are directly constructable, so unit tests are fast and
focused. Use descriptive test names that read as specifications:

```
<verb>_<scenario>_<expected_outcome>
```

Examples from the codebase:

```rust
#[test]
fn valid_exit_accepted() { ... }

#[test]
fn duplicate_exit_returns_already_known() { ... }

#[test]
fn exit_different_validator_accepted() { ... }

#[test]
fn invalid_exit_bad_signature_rejected() { ... }
```

### Unit Test Template

```rust
#[test]
fn <verb>_<scenario>() {
    // Setup: construct component directly
    let spec = Arc::new(E::default_spec());
    let manager = OperationsManager::<E>::new(spec.clone(), Arc::new(OperationPool::new()));

    // Setup: create test state
    let (state, keypairs) = interop_genesis_state::<E>(&keypairs, 0, &spec).unwrap();

    // Act
    let result = manager.verify_voluntary_exit(exit, &state, Epoch::new(0));

    // Assert
    assert!(result.is_err());
}
```

Key points:
- **No `BeaconChainHarness`** -- construct the component with `::new()`.
- **Explicit state** -- pass `BeaconState`, `Epoch`, etc. as parameters.
- **Fast** -- no slot processing, no database, no fork choice.

### Integration Tests

For end-to-end workflows that need the full pipeline (state transitions,
fork choice, database), use `BeaconChainHarness` as before. The existing
harness tests continue to work unchanged.

## Existing Components

| Component | Module | Owns | Lines |
|-----------|--------|------|-------|
| `OperationsManager<E>` | `operations_manager/` | Voluntary exits, proposer/attester slashings, BLS-to-execution changes | 670 |
| `AttestationManager<E>` | `attestation_manager/` | Attestation pools, observed attesters/aggregators, shuffling cache | 657 |
| `SyncCommitteeManager<E>` | `sync_committee_manager/` | Sync aggregation pool, observed contributions/contributors | 448 |
| `DataAvailabilityManager<T>` | `data_availability_manager/` | Blob/column sidecars, DA checker, KZG | 407 |
| `ExecutionManager<T>` | `execution_manager/` | Proposer cache, fork choice signal, `block_is_known_to_fork_choice` | 341 |
| `ValidatorQueryService<T>` | `validator_query_service/` | Validator pubkey cache | 300 |
| `BlockImportState<E>` | `block_import_state/` | Block times cache, observed block producers, pre-finalization cache | 197 |

Total: 46 unit tests across all component modules.

## Migration Guide for Existing Contributors

If you already know the `beacon_chain` crate from before modularization,
here is where things moved and why.

### BeaconChain is now a data struct

`BeaconChain<T>` has **pub fields and no methods** in `beacon_chain.rs`.
There is no `impl BeaconChain<T>` block in that file. The struct is a
bag of components -- callers access them directly via the pub fields.

Some `impl BeaconChain<T>` blocks still exist in other files
(`block_import_methods.rs`, `execution_methods.rs`,
`block_production/mod.rs`, `state_query.rs`, etc.) as thin delegation
wrappers annotated with `TODO(modularize)`. These exist only so external
callers can keep calling `chain.method()` during migration -- they will
be removed.

**The rule**: don't add `impl BeaconChain<T>`. Add logic to the
appropriate component, or write a free function with a context struct.

### Where business logic lives

Each domain has its own component struct that owns state and logic:

| Component | What it owns |
|-----------|-------------|
| `AttestationManager<E>` | Attestation pools, observed attesters/aggregators, shuffling cache |
| `OperationsManager<E>` | Voluntary exits, proposer/attester slashings, BLS-to-execution changes |
| `SyncCommitteeManager<E>` | Sync aggregation pool, observed contributions/contributors |
| `DataAvailabilityManager<T>` | Blob/column sidecars, DA checker, KZG |
| `ExecutionManager<T>` | Proposer cache, fork choice signal, `block_is_known_to_fork_choice` |
| `ValidatorQueryService<T>` | Validator pubkey cache and index lookups |
| `BlockImportState<E>` | Block times cache, observed block producers, pre-finalization cache |

Components are constructed with `::new()` and take explicit dependencies.
No hidden global state.

### Where verification happens

- **Attestation verification**: Uses `AttestationVerificationContext`
  (in `attestation_verification.rs`), not `&BeaconChain`. Takes refs to
  `CanonicalHead`, `AttestationManager`, `ValidatorQueryService`, etc.
- **Sync committee verification**: Still takes `&BeaconChain` --
  migration pending.

### Where orchestration lives

Cross-component orchestration uses **free functions + context structs**:

| File | Context struct | Domain |
|------|---------------|--------|
| `block_import_methods.rs` | `BlockImportContext` | Block import pipeline |
| `block_production/mod.rs` | `BlockProductionContext` | Block production |
| `execution_methods.rs` | `ExecutionOrchestrationContext` | Execution layer interactions |
| `state_query.rs` | *(no context struct -- bare free functions)* | State and block root queries |

`state_query.rs` functions take `(store, canonical_head, spec)` as
explicit params. Example: `forwards_iter_block_roots(store,
canonical_head, start_slot)`.

All context structs have a `from_chain` constructor for backward compat.

### Persistence

Components that need to persist data implement `Drop` themselves:

- `OperationsManager` -- persists the operation pool on drop
- `CanonicalHead` -- persists fork choice on drop
- `DataAvailabilityManager` -- persists the custody context on drop

There is no `BeaconChain::drop`. Each component is responsible for its
own shutdown cleanup.

### Testing

Components are directly constructable -- no `BeaconChainHarness` needed
for unit tests:

```rust
let manager = OperationsManager::<E>::new(spec, op_pool);
let result = manager.verify_voluntary_exit(exit, &state, epoch);
assert!(result.is_ok());
```

Use `BeaconChainHarness` only for integration tests that need the full
pipeline (state transitions, fork choice, database).

### Test naming convention

Test names read as specifications:

```
<verb>_<scenario>[_<expected_outcome>]
```

Examples from the codebase:

```rust
fn valid_exit_accepted() { ... }
fn duplicate_exit_returns_already_known() { ... }
fn get_aggregated_attestation_rejects_optimistic_block() { ... }
fn slot_prior_to_bellatrix_when_no_fork_epoch() { ... }
```

Inside the test body, use **Given / When / Then** comments to mark
phases. This is not mandatory for trivial tests, but helps readers
follow the intent of anything beyond a few lines:

```rust
#[test]
fn rejects_attestation_from_optimistic_block() {
    // Given an attestation in the pool referencing an optimistic block
    let manager = make_manager();
    let attestation = make_attestation(&spec, slot, block_root, 0);
    manager.add_to_naive_aggregation_pool(attestation.to_ref()).unwrap();

    // When we retrieve it with execution status = Optimistic
    let result = manager.get_aggregated_attestation(attestation.to_ref(), |_| {
        Some(ExecutionStatus::Optimistic(ExecutionBlockHash::zero()))
    });

    // Then it's rejected
    assert!(matches!(result, Err(Error::HeadBlockNotFullyVerified { .. })));
}
```

## What Remains on BeaconChain

`BeaconChain<T>` still has ~2860 lines across three files:

- `beacon_chain.rs` (~2860 lines) -- orchestration methods, state queries
- `block_import_methods.rs` (~1830 lines) -- block import pipeline
- `execution_methods.rs` (~620 lines) -- execution layer methods
- `block_production/` (~2590 lines) -- block production pipeline

These remain because they are **async orchestration methods** that need
`Arc<Self>` for task spawning (`spawn_blocking_handle`) or because they
are **state query wrappers** around `canonical_head` and `store` that
many external callers depend on.

### Why Not Move Them Now

The callers -- HTTP API routes and `NetworkBeaconProcessor` handlers --
currently receive `Arc<BeaconChain<T>>` and call `chain.method()`. Moving
the logic requires migrating those callers to hold individual component
`Arc`s and call free functions. This is the next phase of work.

### Delegation Methods

Some methods on `impl BeaconChain<T>` are thin delegations annotated with
`TODO(modularize)`. These exist solely so external callers can keep
calling `chain.method()` during the migration. They will be removed when
callers are migrated.
