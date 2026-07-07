# Lighthouse AI Assistant Guide

This file provides guidance for AI assistants (Claude Code, Codex, etc.) working with Lighthouse.

## CRITICAL - Always Follow

- After completing ANY code changes, **MUST** run `cargo check` before considering the task complete.
- Run `make install-hooks` if you have not already. Never skip git hooks. If cargo is not available, install the toolchain.
- Branch from `unstable` and target `unstable` for PRs.
- Update the relevant `.ai/` guide in the same change when work reveals a reusable lesson.

## Quick Reference

```bash
# Build
make install                              # Build and install Lighthouse
cargo build --release                     # Standard release build

# Test (prefer targeted tests when iterating)
cargo nextest run -p <package>            # Test specific package
cargo nextest run -p <package> <test>     # Run individual test
make test                                 # Full test suite (~20 min)

# Lint
make lint                                 # Run Clippy
cargo fmt --all && make lint-fix          # Format and fix
```

## Before You Start

Read the relevant guide for your task:

| Task | Read This First |
|------|-----------------|
| **Code review** | `.ai/CODE_REVIEW.md` |
| **Creating issues/PRs** | `.ai/ISSUES.md` |
| **Changing behavior or shared code paths** | `.ai/CHANGE_SAFETY.md` |
| **Development patterns** | `.ai/DEVELOPMENT.md` |
| **Designing interfaces or APIs** | `.ai/INTERFACE_DESIGN.md` |
| **Writing or modifying tests** | `.ai/TESTING.md` |
| **Updating AI docs** | `.ai/DOC_UPKEEP.md` |

Read only the guides relevant to the task. Do not load every `.ai/` file by default.

## Always-On Rules

- No runtime panics: avoid `.unwrap()`, `.expect()`, and unchecked indexing outside startup/config validation.
- In `consensus/` excluding `types/`, use saturating or checked arithmetic.
- Never block the async runtime. Use the repository's blocking helpers for CPU-heavy work.
- Document lock ordering when touching code that takes multiple locks.
- Use scoped rayon pools from beacon processor, not the global rayon pool.
- All `TODO` comments must link to a GitHub issue.
- Avoid ambiguous abbreviations. Use names like `beacon_block` and `blob`.

## Extra PR Guidelines

- Run `cargo sort` when adding dependencies
- Run `make cli-local` when updating CLI flags

## Project Structure

```
beacon_node/           # Consensus client
  beacon_chain/        # State transition logic
  store/               # Database (hot/cold)
  network/             # P2P networking
  execution_layer/     # EL integration
validator_client/      # Validator duties
consensus/
  types/               # Core data structures
  fork_choice/         # Proto-array
```

See `.ai/DEVELOPMENT.md` for detailed architecture.
