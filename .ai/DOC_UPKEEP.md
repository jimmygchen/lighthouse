# AI Documentation Upkeep

Read this when implementation, testing, review, or PR work reveals a reusable
lesson for future AI assistants.

## Default Behavior

Update the relevant `.ai/` guide automatically in the same change when the lesson
affects future code, tests, reviews, or PRs.

Ask first only when:

- The lesson is subjective or uncertain.
- The correction is one-off and may not generalize.
- The doc change would broaden the scope of the task.
- The lesson conflicts with existing guidance.

## Where to Put Lessons

- `.ai/TESTING.md`: test helpers, fixture choices, fork-specific commands,
  weak generated-test patterns, and invariant coverage.
- `.ai/CODE_REVIEW.md`: review heuristics, recurring maintainer feedback, and
  issues that reviewers should prioritize or ignore.
- `.ai/ISSUES.md`: issue/PR format, labels, phrasing, and examples.
- `.ai/DEVELOPMENT.md`: architecture, implementation patterns, commands, and
  subsystem-specific conventions that do not fit a more specific guide.
- `CLAUDE.md`: only always-on rules and routing. Keep it short because it is
  loaded for every task.

## Prefer Automation for Mechanical Rules

Do not solve every recurring problem with prose. If a rule is objective and
cheap to check, prefer enforcing it with existing tooling or a small script.

Good candidates for automation:

- Disallowed functions, methods, or unsafe patterns. Prefer clippy config or a
  focused CI check.
- Required formatting, dependency sorting, generated files, or lockfile updates.
- Markdown or documentation structure that can be checked mechanically.
- Repeated `rg`-style stale-reference checks after removing a feature, flag, or
  type.
- Required command coverage for a touched subsystem when the mapping is stable.

Keep these as docs instead:

- Judgment calls about interface shape, abstraction, or ownership.
- Test quality expectations that require understanding behavior.
- Review heuristics and examples of subtle regressions.
- Domain decisions whose correctness depends on protocol context.

When adding automation, document the command in `.ai/DEVELOPMENT.md` or the
relevant focused guide, and make sure the final response reports whether it ran.

## Lesson Format

```markdown
### Lesson: [Brief Title]

**Context:** [What task were you doing?]
**Issue:** [What went wrong or was corrected?]
**Learning:** [What to do differently next time]
```

Use this format for review lessons and larger corrections. For small testing or
development discoveries, a concise bullet in the relevant section is enough.

## When Not to Update

- Minor preference differences.
- One-off edge cases unlikely to recur.
- Guidance already covered by an existing `.ai/` file.
- Information that belongs in production docs, comments, or tests instead of AI
  guidance.

## Context Window Discipline

Keep `CLAUDE.md` compact. New detailed guidance should usually live in `.ai/`
and be linked from the routing table, so agents load it only when the task needs
it.
