# Interface Design Guide

Read this before adding or changing structs, enums, traits, function signatures,
config fields, APIs, events, database-facing types, or cross-subsystem
interfaces.

The goal is to make correct usage easy and incorrect usage hard.

## Do Not Copy Bad Precedent Blindly

Prefer local patterns by default, but do not preserve a pattern just because it
exists nearby.

Pause and reassess when existing code uses:

- Boolean parameters whose meaning is unclear at call sites
- Multiple `Option` parameters where only some combinations are valid
- Parallel arrays or loosely related parameters that must stay index-aligned
- Stringly typed state with a closed set of values
- Generic wrappers, traits, or `Arc<Mutex<_>>` without a clear ownership need
- Runtime panics in non-startup paths
- Broad mocks that do not protect behavior
- Helpers shared across forks or subsystems with undocumented assumptions

If following the existing pattern would make the interface easier to misuse,
prefer a better local design and explain the reason.

## Make Invalid States Hard to Represent

Use named types to encode meaning:

- Prefer enums over booleans when there are named modes or future variants.
- Prefer a struct over several parameters that are always passed together.
- Prefer typed IDs, roots, slots, epochs, and fork names over raw primitives
  where the codebase already has domain types.
- Prefer explicit error variants over strings when callers need to react.
- Use `Option<T>` only when absence is a real valid state.

Avoid APIs where callers can pass contradictory combinations and rely on the
callee to sort them out.

## Keep Boundaries Honest

Interfaces should match real ownership and responsibility:

- Consensus rules belong in consensus/state-transition code, not API or sync
  convenience layers.
- Storage interfaces should expose domain operations, not backend quirks.
- HTTP APIs should not leak internal helper shapes unless they are the intended
  response contract.
- Validator-client interfaces should not depend on beacon-node internals unless
  the existing architecture already requires it.

If an interface crosses subsystem boundaries, identify which subsystem owns the
contract and keep conversion code at the boundary.

## Avoid Premature Abstraction

Do not add a trait, generic parameter, wrapper type, or new module just because
two call sites look similar.

Add abstraction only when:

- There are real call sites with the same contract.
- Tests need a narrow seam around external behavior.
- The abstraction names a domain concept, not just an implementation step.
- It reduces invalid states or repeated logic in a meaningful way.

Three clear lines at two call sites are often better than a vague shared helper.

## Signature Checklist

Before changing a signature, check:

- Are parameter names and types clear at every call site?
- Would an enum or options struct be clearer than positional booleans/options?
- Does the return type distinguish success, absence, invalid input, and external
  failure correctly?
- Are errors typed enough for callers to make the right decision?
- Is the interface stable for older forks or existing users?
- Does the new shape require migration, config updates, docs, or CLI changes?

Run `rg` for all call sites and inspect the non-obvious ones.

## Compatibility and Migration

For public APIs, config, CLI, stored data, and wire formats:

- Prefer additive changes when compatibility matters.
- Document breaking changes clearly.
- Update migrations, config examples, CLI docs, or API tests in the same change
  when the interface requires it.
- Keep serde defaults and skipped fields deliberate.
- Do not change response status codes or error shapes accidentally.

## Review Questions

Before finalizing interface work, answer:

- What invalid usage does this interface prevent?
- What important behavior is now explicit in the type system?
- Which caller is the hardest to reason about, and does the design still fit it?
- Did I choose a better pattern where local precedent was weak?
- Did I update `.ai/` docs if this decision should guide future work?
