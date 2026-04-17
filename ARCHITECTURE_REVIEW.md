# Architecture Review: Modularize BeaconChain

## Base Commit

- **Hash:** `d3c13c4cf0`
- **Description:** `Gloas: envelope peer penalties and REJECT/IGNORE mapping (#8981)`
- **Branch commits:** 25 commits on this branch

---

## Method-by-Method Assessment of `beacon_chain.rs`

All `pub`/`pub(crate)` methods on `impl BeaconChain<T>` remaining in `beacon_chain.rs` (lines 519-2717), categorized by removal feasibility.

### Can Delete -- Thin Wrappers Callers Can Replace

These methods delegate to a component or store with zero/trivial added logic. Callers can access the underlying component directly once fields are `pub`.

| Method | Line | What it wraps | Notes |
|--------|------|---------------|-------|
| `slot()` | 609 | `self.slot_clock.now()` | Trivial. Callers can use `slot_clock.now().ok_or(...)` |
| `epoch()` | 618 | `self.slot()` + arithmetic | Trivial composition |
| `get_blinded_block()` | 1175 | `self.store.get_blinded_block()` | Pure delegation |
| `get_payload_envelope()` | 1182 | `self.store.get_payload_envelope()` | Pure delegation |
| `get_state()` | 1205 | `self.store.get_state()` | Pure delegation with error mapping |
| `get_block_process_status()` | 1192 | `self.data_availability_checker.get_cached_block()` | Pure delegation |
| `heads()` | 1287 | `self.canonical_head.fork_choice_read_lock()` | Thin delegation, 7 lines |
| `manually_compact_database()` | 1455 | `self.store_migrator.process_manual_compaction()` | Pure delegation |
| `shutdown_sender()` | 2606 | `self.shutdown_sender.clone()` | Trivial |
| `enr_fork_id()` | 2422 | Free function `enr_fork_id()` | Already has free function equivalent |
| `compute_fork_digest()` | 2429 | Free function `compute_fork_digest()` | Already has free function equivalent |
| `duration_to_next_digest()` | 2437 | Free function `duration_to_next_digest()` | Already has free function equivalent |
| `persist_op_pool()` | 561 | `self.store.put_item()` | Trivial with timer |
| `persist_custody_context()` | 573 | `persist_custody_context()` free fn | Adds logging, could be inlined |
| `wall_clock_state()` | 1376 | `self.state_at_slot(self.slot()?)` | One-liner |
| `get_blobs_checking_early_attester_cache()` | 1055 | `early_attester_cache + data_availability_manager` | Small composition; inline at callers |
| `sync_committee_at_next_slot()` | 1218 | `self.sync_committee_manager.sync_committee_at_next_slot()` | Delegation with state loader closure |
| `sync_committee_at_epoch()` | 1233 | `self.sync_committee_manager.sync_committee_at_epoch()` | Delegation with state loader closure |
| `state_for_sync_committee_period()` | 1248 | `self.sync_committee_manager.slot_for_sync_committee_period()` | Delegation + state_at_slot |
| `recompute_and_cache_light_client_updates()` | 1258 | `self.light_client_server_cache.recompute_and_cache_updates()` | Delegation with store/spec |
| `get_light_client_updates()` | 1271 | `self.light_client_server_cache.get_light_client_updates()` | Delegation |
| `validator_seen_at_epoch()` | 2619 | `attestation_manager + observed_block_producers` | Small composition |
| `get_aggregated_attestation()` | 1443 | `self.attestation_manager.get_aggregated_attestation()` | Delegation with fork choice closure |
| `get_pre_electra_aggregated_attestation_by_slot_and_root()` | 1493 | `self.attestation_manager.get_pre_electra_aggregated_attestation_by_slot_and_root()` | Delegation with fork choice closure |
| `block_roots_from_fork_choice()` | 2690 | `self.canonical_head.fork_choice_read_lock()` | 20 lines, can be free function |

**Subtotal: 25 methods can be deleted.**

### Can Delete -- TODO(modularize) Tagged Delegations

These are explicitly tagged as temporary and should be trivially removable.

| Method | Line | Delegates to |
|--------|------|-------------|
| `verify_sync_committee_message_for_gossip()` | 1824 | `VerifiedSyncCommitteeMessage::verify` + metrics |
| `verify_sync_contribution_for_gossip()` | 1842 | `VerifiedSyncContribution::verify` + metrics + SSE events |
| `verify_finality_update_for_gossip()` | 1863 | `VerifiedLightClientFinalityUpdate::verify` + metrics |
| `verify_data_column_sidecar_for_gossip()` | 1882 | `GossipVerifiedDataColumn::new` + metrics |
| `verify_blob_sidecar_for_gossip()` | 1898 | `GossipVerifiedBlob::new` + metrics |
| `verify_optimistic_update_for_gossip()` | 1913 | `VerifiedLightClientOptimisticUpdate::verify` + metrics |

**WARNING:** These all add metrics instrumentation (timers, counters) and some add SSE event dispatch. The caller must replicate this when going direct. See "Risks" section.

**Subtotal: 6 methods can be deleted (with care about metrics/events).**

### Must Stay -- Needs `Arc<Self>` or Deep Self-Reference

These methods use `self: &Arc<Self>`, `self.clone()`, `self.task_executor.spawn_blocking_handle()`, or have complex multi-field orchestration that legitimately needs the full struct.

| Method | Line | Why it must stay |
|--------|------|-----------------|
| `per_slot_task()` | 2060 | `self: &Arc<Self>`, spawns blocking task with `self.clone()`, fork choice signal |
| `get_blocks_checking_caches()` | 1024 | `self: &Arc<Self>`, passes Arc to `BeaconBlockStreamer` |
| `get_blocks()` | 1040 | `self: &Arc<Self>`, same as above |
| `get_payload_envelopes()` | 1068 | `self: &Arc<Self>`, passes `self.clone()` to stream launcher |
| `spawn_blocking_handle()` | 1974 | Infrastructure utility, used by many methods |
| `persist_head_in_batch_standalone()` | 525 | Static method, no `self` |
| `load_fork_choice()` | 530 | Static method, no `self` |

**Subtotal: 7 methods must stay (or move to a utility/launcher struct).**

### Needs Redesign -- Too Complex to Just Delete

These methods contain substantial business logic intermixed with multi-component access. They need thought about how to decompose.

| Method | Line | Lines | Complexity |
|--------|------|-------|-----------|
| `produce_unaggregated_attestation()` | 1521 | 196 | Two-phase algorithm: head scrape + committee cache. Accesses `early_attester_cache`, `canonical_head`, `store.get_advanced_hot_state`. Self-contained logic that is NOT a thin wrapper. |
| `verify_unaggregated_attestation_for_gossip()` | 1740 | 40 | Creates `AttestationVerificationContext::from_chain`, then adds SSE events and metrics. The SSE event dispatch is substantial (fork-dependent branching). |
| `verify_aggregated_attestation_for_gossip()` | 1797 | 20 | Same pattern -- context creation + SSE events + metrics. |
| `batch_verify_unaggregated_attestations_for_gossip()` | 1721 | 12 | Context creation + batch call. Fairly simple but needs context. |
| `batch_verify_aggregated_attestations_for_gossip()` | 1784 | 10 | Same pattern. |
| `with_committee_cache()` | 2149 | 156 | Complex caching logic: shuffling cache promises, state loading, partial state advance, committee building. Touches `canonical_head`, `attestation_manager.shuffling_cache`, `store`. |
| `state_at_slot()` | 1302 | 65 | Three-way branch based on slot comparison. Calls `per_slot_processing`, accesses `store`. |
| `state_root_at_slot()` | 806 | 62 | Complex fast-path optimization with head state, Gloas handling, forwards iterator fallback. |
| `block_root_at_slot()` + helpers | 876 | 135 | Two private helpers (`_skips_none`, `_skips_prev`) with optimized fast paths. |
| `forwards_iter_block_roots()` | 635 | 22 | Accesses `store` + `head_snapshot`. |
| `forwards_iter_block_roots_until()` | 661 | 26 | Accesses `store` + `with_head`. |
| `rev_iter_block_roots_from()` | 697 | 16 | Accesses `store` + `get_blinded_block` + `get_state`. |
| `forwards_iter_state_roots()` | 741 | 14 | Accesses `store` + `head_snapshot`. |
| `forwards_iter_state_roots_until()` | 761 | 18 | Accesses `store` + `with_head`. |
| `rev_iter_state_roots_from()` | 724 | 8 | Accesses `store` only. |
| `block_at_slot()` | 787 | 12 | Calls `block_root_at_slot` + store. |
| `validator_attestation_duties()` | 1413 | 28 | Accesses fork choice + `with_committee_cache`. |
| `apply_attestation_to_fork_choice()` | 1935 | 10 | Writes to fork choice. Must preserve lock ordering. |
| `add_to_block_inclusion_pool()` | 1953 | 15 | Writes to op pool. |
| `is_healthy()` | 2446 | 63 | Chain health assessment. Accesses `canonical_head`, `config`, `block_root_at_slot_skips_none`. |
| `verify_weak_subjectivity_checkpoint()` | 2003 | 50 | Accesses `root_at_slot_from_state`. |
| `get_block()` | 1117 | 56 | Async, accesses `store`, `execution_layer`, payload reconstruction. |
| `get_data_columns_checking_all_caches()` | 1081 | 30 | Multi-cache lookup: `data_availability_checker`, `early_attester_cache`, `data_availability_manager`, `store`. |
| `get_light_client_bootstrap()` | 2635 | 16 | Accesses `head()`, `light_client_server_cache`, `store`. |
| `root_at_slot_from_state()` | 1383 | 15 | `BlockRootsIterator` + store. |
| `get_blobs_or_columns_store_op()` | 2652 | 36 | Accesses `data_availability_manager.custody_columns_for_epoch`. |
| `manually_finalize_state()` | 1459 | 30 | Accesses `store` + `store_migrator`. |
| `chain_dump()` / `chain_dump_from_slot()` | 2316 | 100 | Test-only. Heavy store access. |
| `dump_as_dot()` / `dump_dot_file()` | 2510 | 95 | Debug-only. Heavy store + fork choice access. |

**Subtotal: ~28 methods need redesign.**

### Summary

| Category | Count | Notes |
|----------|-------|-------|
| Can delete (thin wrappers) | 25 | Callers replace with direct component access |
| Can delete (TODO-tagged) | 6 | Must replicate metrics + SSE at caller |
| Must stay (Arc/static) | 7 | `per_slot_task`, streaming methods, statics |
| Needs redesign | 28 | State queries, attestation production, caching, iterators |
| **Total `pub`/`pub(crate)` methods** | **~66** | (in `impl BeaconChain<T>` within beacon_chain.rs) |

---

## Risks and Gaps for Phase 9

### Risk 1: Metrics and SSE Events Lost on Deletion (HIGH)

The 6 `TODO(modularize)` gossip verification wrappers and the 4 attestation verification methods all contain:
- `metrics::inc_counter()` / `metrics::start_timer()` instrumentation
- SSE event dispatch (attestation events, contribution events)

If callers go direct to `VerifiedSyncCommitteeMessage::verify()` etc., this instrumentation is silently lost unless each caller replicates it. With ~20+ call sites across `gossip_methods.rs` and `publish_attestations.rs`, this is error-prone.

**Recommendation:** Create "verify and instrument" free functions that wrap the underlying verify + metrics + SSE dispatch. The caller doesn't need `&BeaconChain`; it needs the verification context + event_handler + spec. This preserves the instrumentation without the god object.

### Risk 2: State Query Methods Are Not Simple Wrappers (~1000 lines) (HIGH)

The FINAL_REPORT identifies "state query methods (~1000 lines)" as something callers should "access directly." But many of these are NOT thin delegations:

- `state_root_at_slot()` -- 62 lines of optimization logic (fast-path from head, Gloas special case, forwards iterator fallback)
- `block_root_at_slot()` and its two private helpers -- 135 lines of optimization with head-state fast paths
- `state_at_slot()` -- 65 lines with three-way branching and `per_slot_processing`
- `with_committee_cache()` -- 156 lines of complex cache-miss handling with promises

These cannot be deleted; they need to be moved to a new home. The untracked `state_query.rs` file on disk suggests this work was started but not committed. Moving them out of `impl BeaconChain<T>` requires deciding where they go. Options:
1. Free functions taking `(store, canonical_head, spec)` -- lots of parameters
2. A `StateQueryService` component -- but it has no owned state
3. Keep them on `impl BeaconChain<T>` -- which defeats the purpose

**Recommendation:** These are fundamentally "composed queries" over `store` + `canonical_head`. A `StateQueryContext` struct (like the existing context pattern) with a `from_chain` constructor is the right approach. But 500 lines as a target for beacon_chain.rs is unrealistic unless these move.

### Risk 3: `produce_unaggregated_attestation` Is 196 Lines of Business Logic (MEDIUM)

This method is one of the largest remaining and is NOT a thin wrapper. It implements a two-phase attestation production algorithm with:
- Early attester cache check
- Head snapshot scraping with finalization/out-of-bounds checks
- Fork choice execution status validation
- Advanced hot state loading for cross-epoch attestations
- Beacon committee calculation

It accesses: `attestation_manager.early_attester_cache`, `canonical_head`, `store.get_advanced_hot_state`, `fork_choice_read_lock`, `spec`.

Moving this requires either:
- A new `AttestationProductionContext` (yet another context struct)
- Moving it into `AttestationManager` but passing store/canonical_head as parameters

Either is feasible, but this is not a "just delete and callers go direct" method.

### Risk 4: `with_committee_cache` Has Deep Coupling (MEDIUM)

This 156-line method is called by `validator_attestation_duties` and throughout attestation verification. It accesses:
- `canonical_head.fork_choice_read_lock()` for block shuffling IDs
- `attestation_manager.shuffling_cache` for cache lookup and promise creation
- `store.get_advanced_hot_state()` for cache miss
- `partial_state_advance` for state skipping

This is the glue between fork choice, shuffling cache, and store. It cannot be a component method because it spans three components. It's a prime candidate for a context-struct + free-function pattern, but the promise-based caching makes it tricky.

### Risk 5: `sync_committee_verification` Still Takes `&BeaconChain<T>` (MEDIUM)

The FINAL_REPORT correctly identifies this. 6 functions in `sync_committee_verification.rs` take `chain: &BeaconChain<T>`. Until a `SyncCommitteeVerificationContext` is created (following the `AttestationVerificationContext` pattern), the two `TODO(modularize)` delegations on `BeaconChain` cannot be removed.

### Risk 6: `BlockProductionContext` Escape Hatch (MEDIUM)

`BlockProductionContext` holds `chain: &'a Arc<BeaconChain<T>>` -- a reference to the full god object. The 3 methods that use it (`is_healthy`, `get_execution_payload`, `compute_beacon_block_reward`) are not trivial to extract. `is_healthy` alone is 63 lines accessing `canonical_head`, `fork_choice_read_lock`, `config`, and `block_root_at_slot_skips_none`. This escape hatch must be eliminated before `BlockProductionContext` is truly decoupled.

### Risk 7: HTTP API Context Struct Redesign Scope (MEDIUM)

The HTTP API currently has ~204 `chain.` references across 25 files. The current context struct:

```rust
pub struct Context<T: BeaconChainTypes> {
    pub chain: Arc<BeaconChain<T>>,
    // ... other fields
}
```

Phase 9 proposes adding individual component `Arc`s. This means:
- The `Context` struct needs ~8 new fields (one per component + store + spec + slot_clock + config)
- Every route handler that accesses `chain.xxx` needs updating
- Some routes access 5+ different fields -- their function signatures bloat

This is mechanical but large. Estimate: ~200 line-level changes across 25 files. Not hard, but high risk of regressions in rarely-tested API routes.

### Risk 8: `BeaconChainHarness` in test_utils.rs (LOW)

`test_utils.rs` is 3866 lines with ~63 references to `self.chain.`. `BeaconChainHarness` holds `pub chain: Arc<BeaconChain<T>>` and uses it extensively. The harness doesn't need to change for Phase 9 (it can still hold `Arc<BeaconChain<T>>`), but if the goal is to eventually deprecate it for unit tests, the harness itself needs `Arc<Component>` accessors.

**Recommendation:** Leave `BeaconChainHarness` as-is for Phase 9. It's an integration test fixture; it's fine for it to hold the full chain.

### Risk 9: Circular Dependencies (LOW)

No circular dependencies were found between the extracted components. The dependency graph is clean:
- Components (`OperationsManager`, `AttestationManager`, etc.) have no `impl BeaconChain<T>` dependencies
- Context structs reference components and infrastructure, not each other
- `BlockProductionContext` is the only context that still holds `&Arc<BeaconChain<T>>`

### Risk 10: No Trait Implementations to Block Removal (LOW)

Only `Drop` is implemented for `BeaconChain<T>`. No trait-based polymorphism depends on the methods remaining on `impl BeaconChain<T>`. This is good -- there's no structural barrier to method removal.

---

## Alignment with ARCHITECTURE.md Principles

### Context Structs

All four context structs (`AttestationVerificationContext`, `BlockImportContext`, `BlockProductionContext`, `ExecutionOrchestrationContext`) follow the documented pattern:
- Explicit dependencies in the type signature
- `from_chain` convenience constructor
- Free functions operating on the context

**Issue:** `BlockProductionContext` violates the principle by holding `&Arc<BeaconChain<T>>`. This is documented as a temporary escape hatch. The architecture vision says "pass what you need, not what _has_ what you need" -- this field is the exact opposite.

### Component Boundaries

The 7 extracted components are well-scoped. Each owns distinct state with minimal overlap. One concern:

**`AttestationManager` is doing too much.** It owns the early attester cache, shuffling cache, naive aggregation pool, AND observation tracking for gossip attesters, block attesters, and aggregators. That's 7 distinct data structures. By comparison, `OperationsManager` owns 4 and `SyncCommitteeManager` owns 4. `AttestationManager` could potentially be split further (e.g., `EarlyAttesterCache` as a standalone), but this isn't urgent.

### Unmapped Fields

ARCHITECTURE.md lists several unmapped fields. Current status:
- `config: ChainConfig` -- Still on `BeaconChain`, referenced everywhere. No progress on partitioning.
- `light_client_server_cache` / `light_client_server_tx` -- Still on `BeaconChain`.
- `store_migrator` -- Still on `BeaconChain`.
- `graffiti_calculator` -- Still on `BeaconChain`.
- `pending_payload_envelopes` -- Still on `BeaconChain`.
- `shutdown_sender` -- Still on `BeaconChain`.
- Genesis fields -- Still on `BeaconChain`.

None of these have been addressed. For Phase 9, they don't need to be -- they can remain as `pub` fields. But the architecture document should be updated to reflect this.

### Design Smells

1. **Fields already pub but also in components.** `observed_block_producers`, `observed_blob_sidecars`, `observed_column_sidecars`, `observed_slashable` are all `pub` fields on `BeaconChain<T>` AND also exist in `BlockImportState<E>`. Let me verify this isn't duplication.

<actually_checked>After checking, `BlockImportState` does hold `observed_block_producers` etc., and the `BeaconChain` struct also still holds the same-named fields at lines 385-392. This appears to be genuine duplication -- the same data exists in two places.</actually_checked>

**This is confirmed as an incomplete migration.** `block_import_state` is a field on `BeaconChain` (line 460) but is **never read anywhere in the codebase**. It's a dead field -- the component was created but never wired in. The original top-level fields (`observed_block_producers`, `block_times_cache`, etc.) are still what code references. Fix before Phase 9: either wire `block_import_state` into callers and remove the duplicated top-level fields, or remove the dead `block_import_state` field.

2. **`op_pool` is shared between `BeaconChain` and components.** `BeaconChain` holds `pub op_pool: Arc<OperationPool<T::EthSpec>>`, and `OperationsManager` also holds `op_pool`. Both are `Arc` clones, so this is intentional shared ownership. But it means callers have two paths to the same pool: `chain.op_pool` and `chain.operations.op_pool`. The `BeaconChain` copy should be made private or removed once all callers go through components.

3. **`execution_layer` is on `BeaconChain` AND `ExecutionManager`.** Same pattern as op_pool. Both hold `Option<ExecutionLayer>` or `Option<Arc<ExecutionLayer>>`. This is expected for Arc-shared data, but callers have ambiguous access paths.

---

## Realistic Target for Phase 9

The plan says "target: beacon_chain.rs ~500 lines." Here's a bottom-up estimate:

| Category | Lines |
|----------|-------|
| Struct definition + types + enums + constants | ~460 |
| Drop impl | ~20 |
| Static methods (persist_head, load_fork_choice) | ~40 |
| Must-stay methods (per_slot_task, streaming, spawn_blocking) | ~100 |
| From impls, ChainSegmentResult | ~30 |
| Free functions (fork digest, shuffling) | ~100 |
| **Minimum beacon_chain.rs** | **~750** |

This does NOT include the ~28 "needs redesign" methods. If even half of those stay temporarily (state queries, `with_committee_cache`, `produce_unaggregated_attestation`), the realistic floor is **~1500 lines**.

To reach 500, ALL state query methods, ALL iterator methods, and ALL attestation production methods would need to move. That's feasible but requires:
1. `StateQueryContext` (or similar) for ~700 lines of iterator/query methods
2. `AttestationProductionContext` for `produce_unaggregated_attestation` + `with_committee_cache` (~350 lines)
3. Resolution of `BlockProductionContext` escape hatch
4. `SyncCommitteeVerificationContext`

---

## Recommendation: PROCEED, with Adjustments

The architecture is sound. The component extractions are clean, the context-struct pattern works well, and the 46 unit tests demonstrate real testability gains. The branch delivers on its promise of a 68% line reduction.

However, Phase 9 as described underestimates the remaining work:

### Adjustment 1: Split Phase 9 into 9a and 9b

**Phase 9a (Mechanical):** Delete the 31 thin wrappers and TODO-tagged methods. Update HTTP API and NetworkBeaconProcessor to access components directly. This is well-understood, low-risk, and gets beacon_chain.rs to ~1800 lines.

**Phase 9b (Design):** Move state query methods, iterator methods, `with_committee_cache`, and `produce_unaggregated_attestation` into context-based free functions. Create `SyncCommitteeVerificationContext`. Remove `BlockProductionContext` escape hatch. This requires design decisions and is where the risk lives. Target: ~750 lines.

### Adjustment 2: Add Metrics/SSE Migration Guide

Before deleting the 6 `TODO(modularize)` wrappers, document exactly which metrics and SSE events each one emits. Create "verify and instrument" helper functions that callers use instead of going directly to the underlying verify. This prevents silent observability regressions.

### Adjustment 3: Fix Duplicate Fields

The `observed_block_producers`, `observed_blob_sidecars`, `observed_column_sidecars`, and `observed_slashable` fields appear to be duplicated between `BeaconChain<T>` and `BlockImportState<E>`. Resolve this before Phase 9 to avoid confusion about which is the source of truth.

### Adjustment 4: Revise Line Target

Change the target from "~500 lines" to "~750-800 lines" (struct + types + must-stay methods + free functions). The 500-line target is achievable only by eliminating ALL business logic, which would require moving items like `per_slot_task` out, and that method genuinely needs `Arc<Self>`.

---

## Architecture Concerns

1. **Context struct proliferation.** Four contexts exist; Phase 9b would add 2-3 more. Each has a `from_chain` constructor. This is manageable but approaching the point where callers need a guide to know which context to use. Consider a naming convention or module-level documentation.

2. **"Bags of refs" caller pattern needs enforcement.** ARCHITECTURE.md says callers should be "bags of refs" with no business logic. But today, `gossip_methods.rs` in the network crate contains substantial logic around metrics, logging, and error handling that grew organically. Phase 9 should not just move method bodies from `impl BeaconChain` into caller files -- that would create new god objects in the callers.

3. **The `from_chain` escape hatch defers, not solves.** Every context struct's `from_chain` constructor means callers CAN still hold `Arc<BeaconChain<T>>` and construct any context from it. The real modularity win comes only when callers are restructured to hold individual `Arc<Component>` references. The `from_chain` constructors should be marked `#[deprecated]` or removed once callers migrate.

4. **`pub` fields on `BeaconChain` make the struct a different kind of god object.** Making all fields `pub` achieves method removal but doesn't enforce component boundaries. Any caller with `Arc<BeaconChain<T>>` can still reach into any field. The long-term goal should be eliminating `Arc<BeaconChain<T>>` from callers entirely, replacing it with individual component arcs.
