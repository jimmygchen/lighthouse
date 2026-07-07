# Change Safety Guide

Read this before changing behavior, shared code paths, fork logic, storage,
networking, sync, consensus, validator duties, or any interface with multiple
call sites.

The goal is to prevent regressions. Do not only prove the new behavior works;
prove important existing behavior still holds.

## Before Editing

Write down the behavioral contract in concrete terms:

- What currently happens?
- What should change?
- What must remain unchanged?
- Which forks, networks, store backends, sync states, or validator states are
  affected?
- Which public API, internal interface, metric, log, or persisted data shape can
  callers depend on?

If the answer is uncertain, inspect callers and nearby tests before editing.

## Trace the Real Path

Before changing a function or type, find the real consumers:

```bash
rg "function_or_type_name"
```

For each important caller, check whether it depends on:

- Error variants or fallback behavior
- `None` vs empty collection vs missing store row
- Fork-specific behavior
- Timing, async scheduling, or event ordering
- Persistence across restart
- API response shape or status code
- Metrics, logs, or peer scoring side effects

Do not rely only on the file being edited. Lighthouse regressions often happen
because a helper is shared by import, sync, API, and validator paths with subtly
different expectations.

## Preserve Existing Behavior Deliberately

For every non-trivial change, identify at least one unchanged behavior that
could regress and make sure it is covered by a test or existing validation.

Examples:

- A new rejection path should not reject valid blocks from an earlier fork.
- A new fallback should not hide a store or execution-layer error.
- A new cache or shortcut should not change canonical head, finalization, or
  fork choice results.
- A new API field should not change existing response fields or status codes.
- A refactor should keep the same error semantics unless the task explicitly
  changes them.

If no test is practical, explain the residual risk in the final response.

## Fork and Consensus Changes

Consensus and fork-specific changes need extra care:

- Check whether the change belongs in `consensus/`, `beacon_chain`, HTTP API, or
  validator logic. Do not move protocol rules into convenience layers.
- Test the fork where behavior changes.
- Test or inspect an unaffected fork when production behavior should remain the
  same.
- Preserve SSZ field ordering, defaults, and serde behavior when touching types.
- Use safe arithmetic in `consensus/` excluding `types/`.
- Avoid runtime panics and unchecked indexing.

When in doubt, compare the implementation against the consensus specs in
`./consensus-specs/`.

## Async, Network, and Store Changes

- Prefer event-based tests over sleeps.
- Keep lock scopes narrow and document multi-lock ordering.
- Do not block the async runtime with CPU-heavy work.
- Treat store miss, corrupt data, and backend errors differently when callers do.
- Preserve peer scoring, request limits, and retry/fallback semantics unless the
  task explicitly changes them.

## Regression Test Shape

Good regression tests:

- Fail if the old bug reappears.
- Assert externally visible behavior or domain invariants.
- Cover negative and boundary cases when meaningful.
- Use existing harnesses and fixtures instead of custom miniature worlds.

Weak regression tests:

- Only assert that a mock was called.
- Only construct the new type.
- Assert private helper behavior without checking the caller-visible outcome.
- Duplicate the implementation logic in the assertion.

Read `.ai/TESTING.md` before adding or modifying tests.

## Final Review Questions

Before reporting a change as complete, answer these:

- Did I search for all important call sites?
- Did I preserve behavior that should not change?
- Did I cover the changed behavior and at least one meaningful regression risk?
- Did I check fork-specific impact where relevant?
- Did I update `.ai/` docs if this work revealed a reusable lesson?

Record skipped validation and residual risk clearly in the final response.
