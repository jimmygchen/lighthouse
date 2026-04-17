# Modularize BeaconChain -- Final Report

## Start State (unstable)

- `BeaconChain<T>`: ~9000+ lines in a single file, 200+ methods
- Zero component extraction -- all business logic on one struct
- Untestable without `BeaconChainHarness` (requires database, fork choice, slot clock)
- Every method has implicit access to 40+ fields via `&self`

## End State (this branch)

### Line Count Reduction

| File | Before | After | Change |
|------|--------|-------|--------|
| `beacon_chain.rs` | ~9000+ | 2864 | **-68%** |

The remaining ~2860 lines are orchestration methods (async block import,
block production, execution layer) and state query wrappers that external
callers depend on.

### 7 Extracted Component Structs

Each component owns its state and logic. Constructable with `::new()`
for isolated unit testing.

| Component | Lines | Tests | Responsibility |
|-----------|-------|-------|---------------|
| `OperationsManager<E>` | 670 | 10 | Voluntary exits, proposer/attester slashings, BLS-to-execution changes |
| `AttestationManager<E>` | 657 | 7 | Attestation pools, observed attesters/aggregators, shuffling cache, early attester cache |
| `SyncCommitteeManager<E>` | 448 | 5 | Sync aggregation pool, observed contributions/contributors/aggregators, committee period lookups |
| `DataAvailabilityManager<T>` | 407 | 4 | Blob/column sidecar verification, DA checker, KZG, custody |
| `ExecutionManager<T>` | 341 | 6 | Proposer cache, fork choice signal, `block_is_known_to_fork_choice`, `is_optimistic_or_invalid` |
| `ValidatorQueryService<T>` | 300 | 11 | Validator pubkey cache lookups |
| `BlockImportState<E>` | 197 | 3 | Block/envelope times caches, observed block producers, pre-finalization cache |

**Total: 3020 lines of component code, 46 unit tests.**

### 4 Context Structs

Replace `&BeaconChain<T>` in function signatures with explicit dependency
bundles. Each has a `from_chain` convenience constructor for incremental
migration.

| Context Struct | File | Functions Served |
|---------------|------|-----------------|
| `AttestationVerificationContext` | `attestation_verification.rs` | All attestation verification (26 call sites migrated) |
| `BlockImportContext` | `block_import_methods.rs` | 9 block import helper free functions |
| `BlockProductionContext` | `block_production/mod.rs` | 7 block production helper free functions |
| `ExecutionOrchestrationContext` | `execution_methods.rs` | Execution layer orchestration free functions |

### Key Transformations

- **attestation_verification module**: Fully decoupled from `BeaconChain`.
  All 26 call sites migrated from `&BeaconChain<T>` to
  `AttestationVerificationContext`.
- **Block import**: 9 helper methods converted to free functions.
  `impl BeaconChain<T>` block import code split into
  `block_import_methods.rs`.
- **Block production**: 7 helper methods converted to free functions.
  `impl BeaconChain<T>` block production code split into
  `block_production/` module.
- **Execution orchestration**: Helpers extracted into free functions in
  `execution_methods.rs`. `block_is_known_to_fork_choice` moved to
  `ExecutionManager`.
- **Sync committee, attestation aggregation, fork digest**: Methods moved
  to `SyncCommitteeManager`, `AttestationManager`, or converted to free
  functions.
- **Delegation wrappers**: Two rounds of removal. Stateful delegation
  wrappers inlined at call sites. HTTP API callers migrated to direct
  component access.

## Commit History

```
5ca6b007d6 Move sync committee, validator liveness, and fork digest methods off BeaconChain
b772628db1 Convert block production helpers to free functions with BlockProductionContext
4be6db6ffd Convert 9 block import helpers to free functions with BlockImportContext
72afb8e6a5 Extract execution orchestration helpers into free functions with ExecutionOrchestrationContext
0116a04cd9 Refactor attestation_verification to accept AttestationVerificationContext instead of &BeaconChain
3524b5df15 Fix compile errors, move attestation aggregation to AttestationManager, is_optimistic to ExecutionManager
44c5123672 Split beacon_chain.rs impl blocks into block_import_methods, execution_methods, and block_production
29f7935f8c Fix review issues: DAM dead fields, visibility, test coverage
2b93f83233 Remove stateful delegation wrappers, inline at call sites
b1cb8d07df Remove remaining delegation wrappers from BeaconChain
6bad69de7c Wire SyncCommitteeManager and ValidatorQueryService into BeaconChain
35ec2075b8 Rename BlockWorkflow to BlockImportState, strip orchestration methods
02dcda7bfe Add goal framing and align ARCHITECTURE.md with HTML
9d3422dccc Remove delegation wrappers and migrate remaining callers
fc8770b0c9 Migrate HTTP API callers to direct component access
082a9d5f97 Extract BlockWorkflow state from BeaconChain
b8a257e071 Extract ExecutionManager from BeaconChain
818c22af13 Update ARCHITECTURE.md with refined principles and unmapped fields
ecd38c0ce1 Fix field access paths after component extraction merge
c922e45be5 Extract DataAvailabilityManager from BeaconChain
0be9aaedb1 Extract AttestationManager from BeaconChain
251fe8ebfa Extract ValidatorQueryService from BeaconChain
0f721d0594 Extract SyncCommitteeManager from BeaconChain
d8c3e43663 Extract OperationsManager from BeaconChain
7cde673c97 Add ARCHITECTURE.md for BeaconChain modular redesign
```

25 commits on this branch.

## What Remains for Production

### 1. Caller Migration (HTTP API + NetworkBeaconProcessor)

External callers still receive `Arc<BeaconChain<T>>` and call
`chain.method()`. Migrating them to hold individual component `Arc`s and
call free functions directly would eliminate the remaining delegation
wrappers and let `BeaconChain<T>` shrink further.

### 2. State Query Methods (~1000 lines)

`beacon_chain.rs` contains many wrappers around `canonical_head` and
`store` (e.g., `head_beacon_state()`, `get_block()`,
`block_root_at_slot()`). Callers should access these subsystems directly.

### 3. Async Orchestration Methods

`process_block`, `produce_block_with_verification`, and execution layer
methods need `Arc<Self>` for `spawn_blocking_handle`. These stay on
`impl BeaconChain<T>` until callers are restructured to pass component
`Arc`s individually.

### 4. sync_committee_verification Module

Still takes `&BeaconChain<T>` -- same pattern as
`attestation_verification` before migration. Needs a
`SyncCommitteeVerificationContext` following the same approach.

### 5. BlockProductionContext Escape Hatch

`BlockProductionContext` holds `&Arc<BeaconChain<T>>` for 3 methods that
still need the full chain reference (`is_healthy`,
`get_execution_payload`, `compute_beacon_block_reward`). These need to be
refactored into standalone functions.

### 6. Gloas Block Production

`block_production/gloas.rs` (~895 lines) has not been converted to free
functions yet. It follows the same pattern as the main block production
code and can be converted using `BlockProductionContext`.

## Key Architectural Decisions

1. **Context structs over trait objects.** Dependencies are explicit in
   the type signature, constructable for testing, zero runtime overhead.

2. **`from_chain` convenience constructors.** Enable incremental
   migration -- callers that hold `&BeaconChain` today can adopt context
   structs without restructuring. New callers construct contexts from
   individual component refs.

3. **Free functions for orchestration.** Methods that don't need `&self`
   state become free functions that take a context struct. This makes
   dependencies visible and enables testing without constructing the
   full chain.

4. **Component methods for owned state.** Logic that reads or mutates a
   component's owned fields (pools, caches, observed sets) stays as
   `impl Component` methods.

5. **Callbacks for infrastructure deps.** Components that need
   fork choice reads or event dispatch receive them as closures or
   passed-in values, not by holding a reference to the chain.
