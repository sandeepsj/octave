---
name: octave-cycle
description: The mandatory development cycle for every change to Octave. Enforces approved-plan → scope smallest user-experienceable affordance → implement (engine + Tauri command + MCP tool + UI) → self-test → strict reviewer-agent → fix-loop → handoff message → user test → fix-loop → commit. Skipping the reviewer or the handoff message is forbidden. Invoke at the start of every Octave work session and re-invoke before each new step.
---

# Octave development cycle (mandatory)

> **No defects allowed.** No code lands without an approved plan, a passing reviewer-agent, and a structured handoff message. The user is the final tester; the reviewer-agent is the first gate.

## When to invoke

- At the start of every Octave work session.
- Before starting each new step inside a session.
- When the user says "next step", "keep going", "what's next", or similar.
- Whenever about to write production code.

## Do NOT invoke when

- Answering a quick question.
- Editing docs that don't ship.
- Replying conversationally with no code change.

## The 10 steps

```
1. plan? ─────► no  → Skill(module-plan); STOP. Plan must be approved
   │                  before continuing.
   │ yes
   ▼
2. scope step ──────► one user-experienceable affordance
                      ("can play a file", "can pause", "can seek").
                      Smallest possible. NOT a coherent "feature".
   │
   ▼
3. implement ───────► invoke Skill(octave-build) for the rules.
                      Engine + Tauri command + MCP tool + UI affordance.
                      Tests in same commit. Smallest viable diff.
   │
   ▼
4. self-test ───────► invoke Skill(octave-test) for the rules.
                      cargo test + manual probe (UI click OR agent
                      prompt OR direct MCP probe). Must pass before
                      reviewer.
   │
   ▼
5. reviewer ────────► invoke Skill(octave-review). Strict. Spawned
                      via Agent tool. Reads diff, evaluates plan-
                      alignment, smallest-diff, test coverage,
                      defect-free, both-facades, code quality.
                      Returns approve OR fix-list.
   │
   ▼
6. fix loop ────────► fix items, re-run tests, re-spawn reviewer.
                      Loop until approve. Capped at 5 iterations
                      before flagging to user.
   │
   ▼
7. HANDOFF ─────────► strict format (see below). NO other text
                      between this and step 9.
   │
   ▼
8. user test ───────► user clicks / prompts / inspects.
   │
   ▼
9. user verdict ────► approved → step 10
                      changes → fix list → loop back to step 3
   │
   ▼
10. commit + next ──► commit (per Skill(octave-build) commit format).
                      Optionally re-invoke this skill for the next step.
```

## Handoff message format (mandatory)

After reviewer-agent approves and before user testing, post EXACTLY this:

```
READY: <one-line affordance, present tense — "you can list output devices">

UI:    <one concrete action → one expected outcome>
       Example: open the app → click "List Output Devices" → see your
       Focusrite + the default device appear

AGENT: <one concrete prompt or call → one expected outcome>
       Example: run `node agent/probe.mjs list-devices` → JSON array
       containing your Focusrite

Files: <changed files, comma-separated, max 8 lines>
Commit: <git rev-parse --short HEAD>
```

If a step does not have a UI manifestation (e.g., engine-internal change), state `UI:    n/a — <one-line reason>` explicitly. Same for AGENT. Skipping a row is forbidden.

## Forbidden moves

- Skipping the reviewer-agent.
- Posting "ready to test" without the handoff format above.
- Calling something "done" before user verdict in step 9.
- Pitching the next module/feature before the deferred list is drained
  (carries the [Finish before next](memory/feedback_finish_before_next.md)
  rule from auto-memory).
- Bundling multiple affordances into one step. If the diff has two
  user-visible behaviors, split into two steps.

## Allowed moves

- Defer a UI affordance to a follow-up step **with explicit reason**
  (e.g., "Tauri scaffolding doesn't exist yet"). Document in handoff.
- Skip the reviewer for pure-doc edits / typo fixes (declare in your
  scope statement, not silently).

## Memory hooks

If the user redirects the cycle (e.g., "skip the reviewer for this one,
it's just a typo"), record that as a feedback memory so future
exceptions don't surprise.
