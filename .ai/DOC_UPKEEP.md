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
