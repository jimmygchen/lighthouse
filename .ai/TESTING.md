# Lighthouse Testing Guide

Read this before writing, modifying, or reviewing tests. Lighthouse tests should
prove protocol behavior and user-visible invariants, not just exercise new code.

## Start With the Behavior

Before editing tests, identify the behavior being protected:

- What invariant, error path, fork rule, or API contract can regress?
- Which existing test helper already builds the required world?
- What would fail if the implementation were accidentally reverted?
- Does the behavior differ by fork, network, store backend, or sync state?

If the answer is unclear, inspect nearby tests before adding a new pattern.

## Choose the Smallest Useful Test Layer

- **Unit tests**: Pure functions, small state machines, serialization helpers,
  error mapping, and edge cases that do not need a `BeaconChain`.
- **Beacon chain tests**: Use `BeaconChainHarness` for block import, fork choice,
  canonical head, state transition, execution payload, validator, or store
  behavior that depends on chain context.
- **Network/sync tests**: Use the existing `TestRig` pattern for event-driven
  sync behavior. Prefer asserting emitted events and peer actions over internal
  bookkeeping.
- **HTTP API tests**: Use existing HTTP test utilities and mock external HTTP
  calls with the repository's established mocking pattern.
- **EF/spec tests**: Use Ethereum Foundation vectors when validating consensus
  spec behavior or fork-specific state transition rules.

Do not write a broad integration test when a focused unit test proves the same
contract. Do not write a narrow unit test when the bug depends on chain state,
fork choice, persistence, or async scheduling.

## Use Existing Fixtures and Helpers

Prefer the repository's test builders over hand-rolled fixtures:

- `BeaconChainHarness` from `beacon_node/beacon_chain/src/test_utils.rs`
- Fork-specific `FORK_NAME` tests where the crate supports `fork_from_env`
- Existing store, sync, and HTTP API test utilities near the code under test
- Adapter structs for `BeaconChain`-dependent components, following nearby
  patterns such as `fetch_blobs/tests.rs`

Generated tests often become weak when they create custom miniature worlds. If a
helper exists, use it. If a helper is missing and multiple tests need it, add a
small reusable helper next to the relevant tests.

## Assert Invariants, Not Implementation Details

Good Lighthouse tests usually assert one or more of:

- Accepted vs rejected block, attestation, blob, data column, or payload
- Canonical head, finalized checkpoint, justified checkpoint, or fork choice
  outcome
- Store persistence and reload behavior
- Exact error variant for a consensus, validation, or API failure
- Peer action, sync event, or request/response contract
- Fork-specific behavior remaining unchanged for other forks

Avoid tests that only assert a mock was called, a wrapper was constructed, or a
private helper returned the same value it was given. Those tests usually survive
broken behavior.

## Cover Failure Paths

For non-trivial changes, include at least one negative or boundary case when it
is meaningful:

- Missing data: no blobs, no data columns, absent payload, unknown parent
- Invalid data: wrong root, wrong slot, wrong fork digest, bad signature
- Boundary values: genesis, finalized boundary, first slot of a fork, empty
  validator set, maximum committee or blob counts
- Retry/fallback behavior: external failure, unavailable peer, missing store row

Consensus and fork-choice changes need especially careful failure-path coverage.

## Fork-Specific Changes

When behavior changes for one fork:

- Test the fork where behavior changes.
- Verify earlier production forks still use the previous behavior when relevant.
- Use `FORK_NAME=<fork> cargo nextest run ...` when the crate supports it.
- Keep SSZ field ordering and default values in mind when touching types.

Do not assume a test on the latest fork proves older forks are unaffected.

## Async and Concurrency Tests

- Prefer event-based assertions over sleeps.
- If a timeout is necessary, keep it short and explain what progress signal is
  expected.
- Avoid blocking the async runtime. Use the repository's executor or blocking
  helpers when a test needs CPU-heavy work.
- When locks are involved, test the observable behavior and document any lock
  ordering requirement in the production code.

## Test Naming and Documentation

- Use descriptive test names that state the condition and expected outcome.
- Add a short comment only when the scenario is not obvious from the setup.
- Do not reference PRs or issues as the only explanation. Describe the invariant
  directly so the test remains useful outside GitHub context.
- Keep setup compact. If setup hides the assertion, extract a helper.

## Validation Commands

During iteration, prefer the narrowest command that proves the change:

```bash
cargo nextest run -p <package> <test_name>
cargo nextest run -p <package>
FORK_NAME=electra cargo nextest run -p beacon_chain <test_name>
```

Before considering code changes complete, run the required project check from
`CLAUDE.md`:

```bash
cargo check
```

For larger or cross-crate changes, also run the relevant broader command:

```bash
make test-release
make lint
make test-ef
```

Record any skipped validation and the reason in the final response.

## Keep These Docs Current

AI assistants should update the `.ai/` docs automatically when implementation or
review work reveals a reusable lesson.

Update `.ai/TESTING.md` in the same change when:

- A maintainer corrects a generated test pattern.
- You discover the right helper, fixture, or command for a test category.
- A generated test passed but failed to protect the intended behavior.
- A recurring edge case or fork-specific testing rule is not documented.

Ask before updating only when the lesson is subjective, uncertain, or outside
the scope of the current task. Otherwise, treat the doc update like a test fix:
small, specific, and committed with the change that taught the lesson.
