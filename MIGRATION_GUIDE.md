# Migration Guide: Unstable Commits onto Modularized BeaconChain

This guide is for AI agents cherry-picking new commits from `unstable` onto this branch. The branch restructures `beacon_chain` from a god object into focused components. Any code added to `unstable` after the base commit will conflict and must be adapted.

## Base Commit

**Hash:** `d3c13c4cf0`
**Description:** `Gloas: envelope peer penalties and REJECT/IGNORE mapping (#8981)`

All changes on `unstable` after this commit need migration.

## Architecture Summary

Before: `BeaconChain<T>` was a god object with 40+ fields and 200+ methods. Business logic, state queries, verification, and orchestration all lived in `impl BeaconChain<T>`.

After:

| Concept | Where it lives now |
|---------|-------------------|
| Business logic (attestations, exits, slashings, sync committee, DA) | Component structs (`AttestationManager`, `OperationsManager`, etc.) |
| Verification | Context structs (`AttestationVerificationContext`, etc.) -- NOT `&BeaconChain` |
| Orchestration (block import, block production, execution) | Free functions with context structs (`BlockImportContext`, `BlockProductionContext`) |
| State/root queries | Free functions in `state_query.rs` taking `(store, canonical_head, spec)` |
| `BeaconChain<T>` struct | Bag of pub fields, zero business logic in `beacon_chain.rs` |
| Drop/persistence | Components own their own (`CanonicalHead`, `OperationsManager`, `DataAvailabilityManager`) |

### Components

| Component | Module | Owns |
|-----------|--------|------|
| `OperationsManager<E>` | `operations_manager/` | Voluntary exits, proposer/attester slashings, BLS-to-execution changes, op pool persistence |
| `AttestationManager<E>` | `attestation_manager/` | Attestation pools, observed attesters/aggregators, shuffling cache, early attester cache |
| `SyncCommitteeManager<E>` | `sync_committee_manager/` | Sync aggregation pool, observed contributions/contributors |
| `DataAvailabilityManager<T>` | `data_availability_manager/` | DA checker, observed blob/column sidecars, KZG, custody persistence |
| `ExecutionManager<T>` | `execution_manager/` | Proposer cache, fork choice signal, `block_is_known_to_fork_choice` |
| `ValidatorQueryService<T>` | `validator_query_service/` | Validator pubkey cache |

### Context Structs

| Struct | File | Purpose |
|--------|------|---------|
| `AttestationVerificationContext` | `attestation_verification.rs` | All attestation verification |
| `BlockImportContext` | `block_import_methods.rs` | Block import helper free functions |
| `BlockProductionContext` | `block_production/mod.rs` | Block production helper free functions |
| `SyncCommitteeVerificationContext` | `sync_committee_verification.rs` | Sync committee verification |

Each has a module-private `*_from_chain()` helper that constructs the context from `&BeaconChain<T>`. This is a backward-compat bridge -- new callers should construct directly from individual component refs.

## Migration Patterns

### A. New method added to `impl BeaconChain<T>`

1. Identify which component owns the concern (see table above).
2. If pure business logic on one component's state: add the method to that component.
3. If orchestration across multiple components: write a free function, add deps to an existing context struct or create a new one.
4. If it is a thin delegation to store/canonical_head/slot_clock: do not add it. Callers access the pub field directly.
5. Update callers to use the component or free function.

Example -- a new method `verify_foo(&self, foo: Foo) -> Result<()>` that checks `self.observed_foo` and inserts into `self.op_pool`:

```rust
// BEFORE (on unstable): added to impl BeaconChain<T>
impl<T: BeaconChainTypes> BeaconChain<T> {
    pub fn verify_foo(&self, foo: Foo) -> Result<()> {
        self.observed_foo.check(&foo)?;
        self.op_pool.insert(foo);
        Ok(())
    }
}

// AFTER (on this branch): added to OperationsManager
impl<E: EthSpec> OperationsManager<E> {
    pub fn verify_foo(&self, foo: Foo, head_state: &BeaconState<E>) -> Result<()> {
        // Takes explicit state params, not &self.canonical_head
        self.observed_foo.check(&foo)?;
        self.op_pool.insert(foo);
        Ok(())
    }
}
```

### B. New field added to `BeaconChain<T>` struct

1. Determine which component should own it by the concern it serves.
2. Add the field to that component's struct and update its `::new()` constructor.
3. Add a `pub` field to `BeaconChain<T>` only if external callers (HTTP API, network) need direct access AND the component migration is not yet complete.
4. Update `builder.rs` to construct the component with the new field.

If the field belongs to no existing component (e.g., a new cross-cutting concern), add it directly to `BeaconChain<T>` as a `pub` field and document it as unmapped.

### C. Changes to existing methods

The method likely moved. Find it:

| Old location | New location |
|-------------|-------------|
| `chain.slot()` | `state_query::current_slot(&chain.slot_clock)` |
| `chain.epoch()` | `state_query::current_epoch::<E, _>(&chain.slot_clock)` |
| `chain.get_blinded_block(root)` | `chain.store.get_blinded_block(root)` |
| `chain.get_state(root, slot)` | `chain.store.get_state(root, slot)` |
| `chain.heads()` | `chain.canonical_head.fork_choice_read_lock().get_heads()` |
| `chain.enr_fork_id()` | `beacon_chain::enr_fork_id::<T>(&chain.slot_clock, &chain.spec, chain.genesis_validators_root)` |
| `chain.verify_voluntary_exit(exit)` | `chain.operations.verify_voluntary_exit(exit, &head_state, epoch)` |
| `chain.verify_proposer_slashing(ps)` | `chain.operations.verify_proposer_slashing(ps, &head_state, epoch)` |
| `chain.verify_attester_slashing(as)` | `chain.operations.verify_attester_slashing(as, &head_state, epoch)` |
| `chain.verify_bls_to_execution_change(ch)` | `chain.operations.verify_bls_to_execution_change(ch, &head_state)` |
| `chain.verify_unaggregated_attestation_for_gossip(att)` | `AttestationVerificationContext::from_chain(chain)` then verify |
| `chain.verify_sync_committee_message_for_gossip(msg)` | `VerifiedSyncCommitteeMessage::verify(...)` (sync_committee_verification.rs) |
| `chain.persist_op_pool()` | Handled by `OperationsManager::drop()` automatically |
| `chain.persist_custody_context()` | Handled by `DataAvailabilityManager::drop()` automatically |
| `chain.forwards_iter_block_roots(start)` | `state_query::forwards_iter_block_roots(&chain.store, &chain.canonical_head, start)` |
| `chain.state_root_at_slot(slot)` | `state_query::state_root_at_slot(...)` |
| `chain.block_root_at_slot(slot, skip)` | `state_query::block_root_at_slot(...)` |

For methods still on `impl BeaconChain<T>` in other files (`block_import_methods.rs`, `execution_methods.rs`, `block_production/`): apply the change there. These are unchanged in location but use context structs internally.

### D. New verification logic

Do NOT take `&BeaconChain`. Follow the existing context struct pattern:

```rust
pub struct FooVerificationContext<'a, T: BeaconChainTypes> {
    pub canonical_head: &'a CanonicalHead<T>,
    pub store: &'a BeaconStore<T>,
    pub spec: &'a ChainSpec,
    // only what you need
}

/// Module-private bridge for callers still holding &BeaconChain.
fn foo_verification_context_from_chain<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
) -> FooVerificationContext<'_, T> {
    FooVerificationContext {
        canonical_head: &chain.canonical_head,
        store: &chain.store,
        spec: &chain.spec,
    }
}

pub fn verify_foo<T: BeaconChainTypes>(
    ctx: &FooVerificationContext<'_, T>,
    foo: SignedFoo,
) -> Result<VerifiedFoo, FooError> {
    // verification logic here
}
```

### E. New tests

Prefer component unit tests. Construct the component with `::new()`, pass test state, assert results. No `BeaconChainHarness`.

```rust
#[test]
fn rejects_invalid_foo() {
    // Given
    let spec = Arc::new(E::default_spec());
    let manager = OperationsManager::<E>::new(spec.clone(), Arc::new(OperationPool::new()));
    let (state, _keypairs) = interop_genesis_state::<E>(&keypairs, 0, &spec).unwrap();

    // When
    let result = manager.verify_foo(invalid_foo, &state, Epoch::new(0));

    // Then
    assert!(result.is_err());
}
```

Use `BeaconChainHarness` only for integration tests needing the full pipeline (state transitions, fork choice, database).

Test naming: `<verb>_<scenario>[_<expected_outcome>]`. Use Given/When/Then comments inside the body.

## Step-by-Step Migration for Each Commit

```bash
# 1. Cherry-pick the commit (expect conflicts)
git cherry-pick <commit>

# 2. For each conflict in beacon_chain.rs:
#    - The method/field was moved. Find its new home using the tables above.
#    - grep -r "fn method_name" beacon_node/beacon_chain/src/ to locate it.

# 3. For each conflict in builder.rs:
#    - New fields may need to be wired through component constructors.
#    - Check the component's ::new() signature.

# 4. Apply the change at the new location.
#    - Component method -> component module
#    - Context struct field -> context struct definition
#    - Free function -> the relevant _methods.rs or state_query.rs file

# 5. Verify compilation
cargo check -p beacon_chain

# 6. Run tests
cargo nextest run -p beacon_chain
```

## Common Gotchas

### Slot and epoch access

```rust
// BEFORE
let slot = self.slot()?;
let epoch = self.epoch()?;

// AFTER
use crate::state_query;
let slot = state_query::current_slot(&self.slot_clock)?;
let epoch = state_query::current_epoch::<T::EthSpec, _>(&self.slot_clock)?;
// Or if the caller already has slot_clock as a param:
let slot = slot_clock.now().ok_or(Error::UnableToReadSlot)?;
```

### Store access

```rust
// BEFORE
let block = self.get_blinded_block(&root)?;

// AFTER (direct store access)
let block = self.store.get_blinded_block(&root)?;
```

### Verification takes context structs, not `&BeaconChain`

```rust
// BEFORE
let verified = chain.verify_unaggregated_attestation_for_gossip(att)?;

// AFTER
let ctx = AttestationVerificationContext { /* fields */ };
// or: let ctx = attestation_verification_context_from_chain(chain);
let verified = ctx.verify_unaggregated_attestation_for_gossip(att)?;
```

### Component methods take explicit state params

```rust
// BEFORE (accessed self.canonical_head internally)
let outcome = self.verify_voluntary_exit(exit)?;

// AFTER (caller fetches state, passes it in)
let head = chain.canonical_head.cached_head();
let epoch = slot_clock.now().unwrap().epoch(E::slots_per_epoch());
let outcome = chain.operations.verify_voluntary_exit(
    exit, &head.snapshot.beacon_state, epoch,
)?;
```

### Drop/persistence is on components

```rust
// BEFORE
impl<T: BeaconChainTypes> Drop for BeaconChain<T> {
    fn drop(&mut self) {
        self.persist_op_pool();
        self.persist_fork_choice();
        self.persist_custody_context();
    }
}

// AFTER -- no BeaconChain::drop. Each component handles its own:
// - OperationsManager::drop() persists the op pool
// - CanonicalHead::drop() persists fork choice
// - DataAvailabilityManager::drop() persists custody context
```

### `from_chain` is a bridge, not the target pattern

The `*_from_chain()` functions exist for backward compat. If writing new code, construct the context struct directly from individual component refs. Do not add new `from_chain` constructors.

### Shared Arc fields

Some fields exist on both `BeaconChain` and a component (e.g., `op_pool`, `execution_layer`, `data_availability_checker`, `kzg`). These are `Arc` clones. New code should access through the component. The top-level field on `BeaconChain` is for callers not yet migrated.

### Metrics and SSE events

Several deleted wrapper methods included metrics timers and SSE event dispatch. If the upstream commit adds instrumentation inside a wrapper method, ensure the metrics/events are preserved at the call site or in a helper function.
