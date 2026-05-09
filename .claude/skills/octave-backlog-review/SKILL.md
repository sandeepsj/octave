---
name: octave-backlog-review
description: Retroactive reviewer pass over existing Octave code that pre-dates the octave-cycle skill. Same reviewer brief as octave-review but iterates one crate / module / topic at a time and produces a fix-list per pass. Used to bring legacy code (octave-recorder, octave-player, octave-mcp written before the cycle landed) up to the no-defects-allowed bar before new work starts.
---

# Octave backlog review

> One-time discipline pass. Run on every existing crate. Same bar as new
> work. Until each crate is APPROVE'd, no new affordances built on top
> of it.

## Scope

The crates that pre-date the cycle:

- `crates/octave-recorder` — recording engine
- `crates/octave-player` — playback engine
- `crates/octave-mcp` — MCP server exposing both

Three reviewer passes, one per crate. Each pass:

1. Reviewer reads the entire crate (not a diff — the WHOLE thing).
2. Returns FIX list against the same criteria as `octave-review`
   (PLAN-ALIGNMENT, SMALLEST-DIFF where applicable to historical commits,
   TESTS, DEFECTS, FACADES, QUALITY).
3. Author fixes per item. Each fix lands as its own commit per the
   normal cycle (build → self-test → reviewer → handoff for the *fix*,
   not the original code).
4. Re-spawn reviewer on the crate. Loop until APPROVE.

## Reviewer prompt template

```
You are the Octave backlog reviewer. The crate at <path> was written
before the octave-cycle skill was adopted. Bring it up to the no-defects
bar.

WORKFLOW DOCS (read first):
- /media/extra/Developer/octave/.claude/skills/octave-cycle/SKILL.md
- /media/extra/Developer/octave/.claude/skills/octave-build/SKILL.md
- /media/extra/Developer/octave/.claude/skills/octave-test/SKILL.md
- /media/extra/Developer/octave/.claude/skills/octave-review/SKILL.md  (criteria)
- /media/extra/Developer/octave/docs/modules/<slug>.md  (the plan)

SCOPE: <crate path>

Read every .rs file in the crate. Apply the criteria from
octave-review/SKILL.md, with these adjustments for historical code:

- SMALLEST-DIFF criterion is N/A for already-landed commits — skip it.
- Don't ask for refactors that are stylistic only; flag only defects,
  test gaps, plan drift, or code-quality issues that materially affect
  correctness or readability.
- Cross-reference against the module plan. Names that diverge from the
  plan (without an updated plan) are defects.

OUTPUT: same shape as octave-review (APPROVE or FIX-list). Group FIX
items by file in the FIX list so the author can address one file at a
time.

Iteration cap: 5 rounds per crate. If the same defect persists past 5
rounds, ESCALATE.
```

## Order

Suggested order (from foundational to dependent):

1. `octave-recorder` — bottom of the dependency chain.
2. `octave-player` — depends on recording's WAV format.
3. `octave-mcp` — depends on both.

Each pass is self-contained — finish one crate before starting the next.

## Done definition

A crate's backlog review is done when:

- Reviewer returns APPROVE.
- All fix-list items have landed as commits.
- `cargo test --workspace` passes.
- The crate's module plan reflects any name changes / new fields the
  fix-list introduced.

After all three crates are APPROVE'd, the backlog review skill is
shelved (it can re-run if a future audit is requested) and new work
proceeds under `octave-cycle` only.

## Tracking

Per crate, append to `docs/backlog-review.md`:

```
## <crate>
Started: YYYY-MM-DD
Completed: YYYY-MM-DD
Rounds: <N>
Fix commits: <comma-separated short shas>
Final status: APPROVE
```

This file is the audit log of "we did the discipline pass".
