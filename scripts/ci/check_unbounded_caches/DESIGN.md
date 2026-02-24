# Unbounded Cache Detection Tool — Design (v2)

## Problem

Detect structs with `HashMap`/`BTreeMap`/`HashSet`/`BTreeSet` fields where:
1. No pruning/bounding logic exists, OR
2. Pruning methods exist but are never called from anywhere in the workspace

This requires **workspace-wide** analysis because pruning often happens in a
different crate than where the struct is defined.

## Current tool (v1) — limitations

- Single-file scope: only checks impl blocks in the same file as the struct
- String matching for method detection (`quote::quote!` → string contains)
- High false-positive rate (28 allowlist entries day one)
- Cannot detect "has prune method but nobody calls it"

## Proposed approach (v2) — three-pass workspace scan

### Pass 1: Collect struct fields with collection types

Walk all `.rs` files. For each struct, record fields whose type ends with
`HashMap`, `BTreeMap`, `HashSet`, `BTreeSet`.

Output: `Map<StructName, Vec<FieldInfo>>` keyed by fully-qualified struct name
(file_path::StructName).

### Pass 2: Collect pruning methods

Walk all `.rs` files. For each `impl` block:
- Identify the struct being implemented (by name)
- For each method, check if it performs pruning on collection fields:
  - **By method name**: method name contains `prune`, `shrink`, `evict`,
    `truncat`, `gc`, `purge`, `cleanup`, `expire`
  - **By AST**: method body contains a method call expression where the
    receiver is `self.<field_name>` and the method is one of: `remove`,
    `retain`, `clear`, `drain`, `pop`, `pop_first`, `pop_last`, `split_off`

Use proper `syn::visit::Visit` to walk method bodies instead of string
matching. Look for `ExprMethodCall` nodes where:
- The receiver resolves to `self.<field>` (direct or through references)
- The method name is a pruning method

Output: `Map<StructName, Vec<PruningMethod>>` where PruningMethod includes
the method name and which field it prunes.

### Pass 3: Verify pruning methods are called

Walk all `.rs` files again. For each function/method body, look for call
expressions that invoke any known pruning method from Pass 2.

This catches:
- `foo.cache.remove(...)` — direct field access
- `foo.prune()` — calling the wrapper method
- `Self::prune(&mut self)` — UFCS calls
- Method calls through trait objects (by name matching)

Output: `Set<(StructName, PruningMethod)>` — pruning methods that are
actually called somewhere.

### Final analysis

A struct field is flagged as "potentially unbounded" if:
- It has a collection-type field (Pass 1), AND
- Either:
  - No pruning methods exist for that field (Pass 2), OR
  - Pruning methods exist but are never called (Pass 3)

### Reducing false positives

Only scan these directories (long-lived services):
- `beacon_node/`
- `validator_client/`
- `slasher/`

Skip:
- `**/test_utils*`, `**/tests/`, `**/*_test.rs` — test code
- `**/src/bin/` — short-lived binaries
- Files containing `#[derive(Deserialize)]` on the struct — likely API
  request/response types, not caches

Additionally, ignore structs that are:
- Generic over all fields (likely container/wrapper types, not caches)
- Named with common non-cache suffixes: `Config`, `Request`, `Response`,
  `Params`, `Args`, `Options`, `Event`, `Message`

### Allowlist format

Same as v1 — `.github/custom/unbounded-cache-allowlist.toml`:

```toml
[[allowed]]
entry = "beacon_node/beacon_chain/src/foo.rs::MyStruct::my_field"
reason = "Bounded by validator set size"
```

### Dependencies

- `syn` 2.x with `full` and `visit` features — AST parsing
- `glob` — file discovery
- `toml` + `serde` — allowlist parsing

No `quote` dependency needed (v1 used it for string matching hack).

### CI integration

```yaml
- name: Check for unbounded caches
  run: cargo run --manifest-path scripts/ci/check_unbounded_caches/Cargo.toml -- .
```

### Output format

```
FAIL: beacon_node/beacon_chain/src/foo.rs::MyCache::entries (HashMap)
      No pruning logic found for this field.

WARN: beacon_node/store/src/bar.rs::BarCache::items (BTreeMap)
      Has prune() method but it is never called in the workspace.

To suppress, add to .github/custom/unbounded-cache-allowlist.toml:
[[allowed]]
entry = "beacon_node/beacon_chain/src/foo.rs::MyCache::entries"
reason = ""
```

## State of the codebase

The v1 tool exists at `scripts/ci/check_unbounded_caches/` and should be
rewritten in-place. The allowlist at `.github/custom/unbounded-cache-allowlist.toml`
should be regenerated after the rewrite (entries will change).

The rest of the PR (Parts 1 and 2) is complete and verified:
- Part 1: unwrap_used/expect_used two-pass lint — done, passes
- Part 2: arithmetic_side_effects for fork_choice — done, passes
- Makefile two-pass lint — done
- All cargo fmt / cargo check / clippy passes clean

## Open questions

1. Should "never called" be an error or a warning? (WARN vs FAIL)
2. Should we also detect bounded-but-no-eviction patterns (e.g., LruCache
   that grows but items are never accessed/expired)?
3. Name matching for pruning method calls (Pass 3) will have false negatives
   if the method is called through a trait or renamed variable. Is this
   acceptable?
