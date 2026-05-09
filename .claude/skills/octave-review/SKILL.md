---
name: octave-review
description: Strict reviewer-agent that gates every Octave commit before user handoff. Spawned via the Agent tool with a fixed brief. Reads the current diff, evaluates it against plan-alignment / smallest-diff / test coverage / no-defects-allowed / both-facades / code quality, and returns either APPROVE or a structured fix-list. Loop until APPROVE before posting the handoff message. Invoked from octave-cycle step 5.
---

# Octave reviewer-agent

> Strict gate. Cannot be skipped. Loop until APPROVE.

## Spawning the reviewer

Use the `Agent` tool with `subagent_type: "general-purpose"` and the
prompt template below. The reviewer:

- Reads the diff (HEAD vs. last committed).
- Reads the relevant module plan(s) in `docs/modules/`.
- Reads the cycle and build skills (it knows the rules).
- Returns one of two strict shapes.

## Reviewer prompt template

```
You are the Octave reviewer. Read the current uncommitted diff in
/media/extra/Developer/octave/ and evaluate it against the rules below.

WORKFLOW DOCS (read first):
- /media/extra/Developer/octave/.claude/skills/octave-cycle/SKILL.md
- /media/extra/Developer/octave/.claude/skills/octave-build/SKILL.md
- /media/extra/Developer/octave/.claude/skills/octave-test/SKILL.md
- /media/extra/Developer/octave/PLAN.md  (project-wide vision)
- relevant docs/modules/<slug>.md (must be `status: approved`)

CRITERIA (each must pass):

1. PLAN-ALIGNMENT
   - Module plan exists and is `status: approved`.
   - The change fits the plan's "In scope" table.
   - Any new types / methods / tools are named in the plan's API or MCP
     section. If a name diverges, that is a defect unless the diff also
     updates the plan.

2. SMALLEST-DIFF
   - One user-experienceable affordance per commit.
   - Refactors not bundled with new affordances.
   - Diff size proportional to the affordance.

3. TEST COVERAGE
   - New logic has unit tests.
   - Cross-facade changes have a probe (MCP) and a documented UI path.
   - `cargo test --workspace` passes (run it; do not trust the diff).
   - `cargo clippy --workspace -- -D warnings` clean.

4. NO DEFECTS ALLOWED
   - No silent error swallowing (`.ok()`, `let _ =`, `unwrap_or_default`)
     unless justified inline with `// reason:`.
   - No `unwrap` / `expect` on user input or external data.
   - No allocations on RT path (audio callback). If the diff touches
     `process_*_buffer` or anything inside `assert_no_alloc`, verify by
     reading the code.
   - No race conditions visible in atomic ordering choices.
   - No regressions: previously-fixed bugs (e.g., the cpal pause trap,
     the duration over-count, the cancel empty-path) must not return.

5. BOTH-FACADES (when applicable)
   - Engine API change → `tauri::command` updated.
   - Engine API change → MCP tool updated.
   - If either is omitted, the commit message must declare
     "Skipped facades: <name> — <reason>".

6. CODE QUALITY
   - Names match the rest of the crate (snake_case, no abbreviations
     unless idiomatic — `dbfs`, `rms`, `rt` allowed).
   - No comments that explain WHAT the code does (the code does that);
     comments only for WHY when WHY is non-obvious.
   - Doc comments on public APIs.
   - Error variants have human messages, not enum names.

OUTPUT — return EXACTLY one of these two shapes:

APPROVE
```
APPROVE
Notes: <one short paragraph of what looked good — informational only>
```

FIX (when any criterion fails)
```
FIX
1. [criterion: <PLAN-ALIGNMENT|SMALLEST-DIFF|TESTS|DEFECTS|FACADES|QUALITY>]
   file: <path>:<line>
   issue: <one-line description>
   suggested fix: <one-line concrete change>
2. ...
```

Do NOT include praise in FIX. Do NOT include suggestions in APPROVE
beyond the brief notes line.

Iteration cap: if you've reviewed 5 iterations of the same change and
the same defect persists, escalate by adding "ESCALATE" as the last line
of your FIX response.
```

## Fix-loop discipline

1. Receive FIX list.
2. Address every item. If an item is mistaken (reviewer hallucinated),
   reply to the reviewer's findings in a code comment and explain in
   your re-spawn prompt; don't silently ignore.
3. Re-run self-tests (`octave-test` skill).
4. Re-spawn the reviewer with the new diff.
5. Loop until APPROVE.

## When to escalate to the user mid-loop

- Reviewer escalates (5 iterations, same defect).
- Reviewer asks for a change that conflicts with the approved plan.
- Reviewer asks for a change that breaks an unrelated facade.

In those cases, post a "REVIEW STUCK" message with:
- The current diff status
- The reviewer's last FIX list
- Your reasoning for why the conflict exists
- A proposed resolution (revert? extend plan? override?)

Wait for user direction. Do not push past the reviewer without explicit
user permission.

## What the reviewer does NOT decide

- UX taste (button placement, color choice). That's the user's call.
- Performance optimization that isn't a defect (e.g., "this could be
  faster" without a measured threshold).
- Future-feature suggestions ("you should also add X"). Out of scope.

The reviewer's job is to verify the rules. Anything beyond is noise.
