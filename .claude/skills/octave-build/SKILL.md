---
name: octave-build
description: Implementation rules for every Octave step — plan-driven, smallest viable diff, tests in same commit, both-facades-or-explicit, commit-message format. Invoked from octave-cycle step 3. Also invoked directly when starting to write code for a scoped step.
---

# Octave build rules

## Plan check

Before any code change to a module:

1. `docs/modules/<slug>.md` must exist with `status: approved` and `approvedDate`.
2. The change must fit the plan's `In scope` table.
3. If it doesn't, STOP — invoke `Skill(module-plan)` to extend or write a new plan.

For cross-cutting / infrastructure changes (build system, CI, test harness, the scaffolding skills themselves) the plan check is waived — declare it in the scope statement.

## Smallest viable diff

- One affordance per commit. "Affordance" = one user-experienceable thing.
  Examples:
  - "list output devices" — one commit
  - "click play" — one commit
  - "click pause" — one commit
  - "see waveform of recorded take" — one commit (waveform render is one
    affordance even if it touches engine + UI)
- Refactors that don't add an affordance ship as their own commit, with
  a justification line in the commit message.
- Two affordances in one diff = split before reviewer.

## Both-facades-or-explicit

When a step touches the engine API:

| Surface | Action |
|---|---|
| Rust crate (`octave-recorder` / `_player` / future) | always — this is where the real change lives |
| `tauri::command` wrapper in the Tauri app | usually — adds UI access path |
| `playback_*` / `recording_*` MCP tool in `octave-mcp` | usually — adds agent access path |
| UI affordance in React | usually — gives user a click to verify |

If any surface is **omitted**, declare it in the commit message footer:
```
Skipped facades: <facade> — <one-line reason>
```

Pure-engine changes (e.g., RT-path performance tweak with no API change)
omit all facades — that's fine, no declaration needed.

## Tests in same commit

- Unit tests for new logic, in the same crate / file pattern as existing tests.
- Integration test (probe script or Tauri test) for cross-facade changes.
- `cargo test --workspace` must pass before reviewer.
- Failing tests blocking land = NO, fix or revert.

## Commit message format

```
<imperative one-line summary, ≤ 72 chars>

<wrapped paragraph(s) explaining the WHY. Reference the plan section
that authorises this change.>

<bullet list of what changed and why, when non-trivial>

Verified: <one-line test statement>
Plan: docs/modules/<slug>.md §<N>
Skipped facades: <if any>
```

Do NOT add Co-Authored-By unless the user asks.

## File and crate conventions

- New crate? Add to workspace, mirror an existing crate's layout
  (`Cargo.toml` workspace inheritance, `src/lib.rs` with module
  declarations alphabetically sorted, `examples/` for demos,
  `#[cfg(test)] mod tests` per module).
- New module in existing crate? Same alphabetical sort in `lib.rs`.
- Tauri commands live in `app/src-tauri/src/commands/<topic>.rs` and
  re-export from `commands/mod.rs`.
- React components live in `app/src/components/<feature>/<Component>.tsx`,
  shadcn primitives in `app/src/components/ui/`.
- Agent scripts live in `agent/<topic>.mjs`, shared utilities in
  `agent/lib/`.

## Linting and formatting

- `cargo fmt` before commit.
- `cargo clippy --workspace -- -D warnings` (workspace lints already
  configured — float_cmp, cast_*, etc.). Don't `#[allow]` to silence;
  fix the underlying issue or document why with `// SAFETY:` or
  `// reason:`.
- TypeScript: `pnpm lint` (ESLint + Prettier, set up by Tauri create).

## Anti-patterns to refuse

- Bundling refactor + new affordance in the same commit.
- Adding a tauri::command that the UI doesn't call yet.
- Adding an MCP tool the agent script doesn't probe yet.
- Disabling a test to land a change.
- "I'll add the test in the next commit."
- Long-running TODOs in code comments without a tracked issue.
