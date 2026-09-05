---
name: plan-epic
description: Turn one epic from docs/fork/ROADMAP.md into a dependency-ordered, PR-by-PR plan saved under docs/fork/plans/, in the exact format implement-epic / implement-task consume. Grounds the plan in AGENTS.md, .claude/rules/fork.md and the real current state of the Rust codebase, resolves the epic's open decisions (with the user, or from the roadmap's defaults in --auto), and sizes each task so it is buildable and validatable locally against real herdr servers. Use when the user says things like "plan epic E1", "break down the fleet core epic into PRs", "make an implementation plan for E3", or when invoked by implement-roadmap.
user-invocable: true
model: fable
allowed-tools: Read, Grep, Glob, Agent, Write, Edit, Bash, AskUserQuestion
argument-hint: '<epic-id-or-title> [--auto]   (e.g. E1, or "fleet core")'
---

# Plan an epic

You take **one epic** from `docs/fork/ROADMAP.md` and turn it into a concrete
plan — a dependency-ordered list of PRs with enough detail that
**`implement-epic`** can drive it to completion and each **`implement-task`**
run can ship a single PR without re-deriving the design. You **plan only**: you
write one Markdown file under `docs/fork/plans/` and stop. You do **not**
implement code, open PRs, or commit (unless the user explicitly asks, or you
were launched by `implement-roadmap`, which tells you to commit the plan).

The output must be consumable by `implement-epic` / `implement-task`, so it must
match their expected shape exactly (status legend + PR-map table + per-PR detail
— see §6). Read `AGENTS.md` (Universal Project Rules, Testing, Code
Conventions) and `.claude/rules/fork.md` first; they override defaults and
often *already decide the architecture* the epic must implement.

## Autonomous mode (`--auto`)

If `$ARGUMENTS` contains `--auto`, never call `AskUserQuestion`. Every open
decision is resolved with the **bolded default** written in the epic's "Open
decisions" line of the roadmap; if a decision has no default there, pick the
option that is smallest, most additive to upstream, and locally validatable,
and record it under "Locked decisions" with the words *(auto default)*. If a
decision is genuinely blocking and has no sane default, write the plan anyway
with that PR marked `⛔ needs decision` in its title and list the question in
the plan's Context and in your report.

## 1. Resolve the epic

`$ARGUMENTS` is `<epic-id-or-title> [--auto]` (e.g. `E1`, or `fleet core`).

- Read `docs/fork/ROADMAP.md` in full. Match the argument to an epic by id
  (`E0`…`E8`) or title. Capture that epic's **Goal**, **Why**, **Deliverables**,
  **Depends on**, **Open decisions**, and any **Constraint downstream epics
  must honor** verbatim — they are the scope contract.
- **If no argument is given, or it's ambiguous:** list the epics with a one-line
  goal each and ask which one with `AskUserQuestion`. In `--auto`, stop and
  report the ambiguity instead.
- **Check the dependency chain** in the roadmap's "Epic status" table. If a
  dependency epic is not ✅, say so. Plan anyway, but list in the plan's Context
  exactly which upstream-epic contracts you assume (module names, types, CLI
  commands) and where they must come from.

## 2. Ground the plan in the binding context

Before designing anything, read what constrains the work:

- **`AGENTS.md`** — Universal Project Rules (state/runtime separation, pure
  render, no god objects, platform isolation, multiplicative performance paths,
  runtime/client boundary guardrail, **stable client endpoint contract**),
  Testing, Code Conventions. For this fork these are the binding architecture.
- **`.claude/rules/fork.md`** — what applies from upstream and what does not,
  branch/commit conventions, where fork code lives, the gate, real-server
  validation, safety.
- **The roadmap's guiding principles** — servers stay stock, SSH transport,
  loopback-first gateway, additive/mergeable code, locally validatable, read is
  safe / control is explicit, host failure is local. Every relevant one becomes
  a constraint in the plan.
- **`docs/fork/decisions/*.md`** ADRs if present, and earlier plans in
  `docs/fork/plans/` for contracts this epic builds on (read their
  "Downstream" sections).

## 3. Explore the real current state

Never plan against an imagined codebase. Launch **up to 3 `Explore` agents in
parallel** (single message, multiple tool calls), scoped to the areas the epic
touches — e.g. one for the client/shell path (`src/client/`, `src/client/shell/`,
`src/protocol/wire.rs` message and snapshot shapes), one for the transport and
server surface the epic consumes (`src/remote/`, `src/ipc.rs`,
`src/server/socket_paths.rs`, `src/session.rs`, `src/api/`), one for
tooling/tests (`justfile`, `.github/workflows/`, `tests/support/mod.rs`,
`tests/multi_client.rs`, `scripts/fork/`, `Cargo.toml` dependencies). Each
agent must report **file paths + real excerpts**, the exact function/type names
to reuse, and what already exists vs. what is missing.

Wait for the agents, then read the handful of **critical files** yourself
(the ones the plan will modify most) so your task specs name real symbols.

## 4. Resolve the open decisions

Collect the forks that change the PR breakdown:

- The epic's own **"Open decisions"** line (each has a bolded default).
- **Scope/size** — a large epic may warrant "core now, X as a follow-up".
- **Dependencies** — any new crate needs a reason (`AGENTS.md`); name the crate,
  version range, features, and why an existing dependency cannot do it.
- **Local validatability** — anything that would need a second machine, a
  phone, a VPN, or a browser must have a local stand-in: named sessions via
  `scripts/fork/fleet-lab.sh`, SSH to `localhost`, `curl`/`websocat`-style
  clients, headless browser. Name the stand-in per PR.

Interactive: ask with **`AskUserQuestion`** (batch 2–4 per call), offering the
roadmap default as the recommended option. `--auto`: take the defaults (§ above).
Fold the answers into a **"Locked decisions"** subsection of the plan's Context.

## 5. Design the PR breakdown

Decompose the epic into **cohesive, dependency-ordered PRs**:

- **One PR = one shippable, reviewable unit** that passes the gate and can be
  validated end to end against real servers on its own. Prefer a foundational
  PR first (config struct + pure state + tests), then vertical slices. Group
  PRs (`A · Foundations`, `B · Connector`, `C · CLI`) for readability.
- **Explicit dependencies.** Every PR lists the PR numbers it depends on (`—`
  if none). The graph must be acyclic and every referenced number must exist —
  `implement-epic` schedules on it. Sketch the waves.
- **File-collision awareness.** Two PRs that edit the same upstream file
  (`src/config/model.rs`, `src/main.rs`, `src/cli/spec.rs`, `Cargo.toml`)
  should be sequenced by dependency or given disjoint regions; call this out in
  a **Sequencing hazards** subsection. A PR that changes `Cargo.toml`/
  `Cargo.lock` runs alone in its wave.
- **Honor the rules.** Additive modules; minimum upstream wiring; no wire or
  endpoint-contract change; pure state testable without sockets; no `unwrap()`;
  `tracing`; platform gating; perf discipline in render/fanout loops.
- **Right-size.** A PR should be finishable by one focused agent including its
  review and real-server validation. Split sprawling tasks; merge trivial ones.
  Let the epic's real surface decide the count (a foundation epic may be ~10
  PRs; a docs-heavy one may be 3).
- **Each PR names its real-server validation** (what to boot, what command to
  run, what output proves it) — this is `implement-task` §7.

## 6. Write the plan file (the deliverable — exact format)

Write **one** file to `docs/fork/plans/<epic-id-lowercase>-<kebab-slug>.md`
(e.g. `docs/fork/plans/e1-fleet-core.md`). It **must** contain, in this order:

1. **`# Epic <id> — <title>`** heading.
2. **`## Context`** — why this epic now (Goal/Why from the roadmap), the **real
   current state** from §3 (what exists, with paths and symbol names), the
   upstream-epic contracts assumed, a **Locked decisions** subsection from §4,
   and a **Sequencing hazards** subsection.
3. **`## Status legend`** — `✅ merged · 🔨 in progress · ⬜ not started · ⛔ blocked`.
4. **`## PR map`** — a Markdown table whose columns are, in order:
   **`#` · `Title` · `Group` · `Depends on` · `Status`**. One row per PR, all
   `⬜` at first. `implement-epic` parses `#`, `Depends on`, and `Status` —
   get them exactly right (use `—` for no deps, comma-separated numbers
   otherwise). Follow it with a one-line **wave preview** and the **model
   assignment**: tasks run on `opus` by default; name any PR whose review agent
   must be `fable` (touches auth/tokens, the SSH transport refactor, anything
   that could silently mis-route input to the wrong host or pane).
5. **`## Verification (the gate — every PR)`** — exactly:
   `bash scripts/fork/gate.sh <worktree>` and read the `EXIT=` line
   (`just ci` = fmt check, clippy `-D warnings`, nextest, maintenance tests,
   bun suites), plus the real-server validation expectation, plus any CI
   gotcha you foresee (e.g. Zig needed for libghostty-vt, `web/dist` freshness
   check, feature flags to test both with and without `gateway`).
6. **`## Cross-cutting constraints (all PRs)`** — the rules from §2 that apply
   throughout, stated concretely for this epic.
7. **`## Per-PR detail`** — one `### PR <n> — <title> · deps: <…>` section per
   row, each with: **Goal**, **Files** (concrete paths to create/modify; mark
   upstream files being edited with *(upstream file — minimal wiring)*),
   **Shapes/approach** (key types, functions, CLI flags, config keys, message
   flow — reference real existing symbols to reuse, don't paste full
   implementations), **Tests** (unit + integration, happy + failure paths),
   **Real-server validation** (what to boot, the exact commands, the expected
   evidence), and **Downstream** (constraints later PRs must honor).
8. **`## Critical files referenced (reuse, don't reinvent)`** — key existing
   files/patterns and the binding rules, with paths and symbol names.
9. **`## End-to-end epic validation`** — how `implement-epic` proves the whole
   epic works after every PR is ✅: fleet lab up with N hosts, the real user
   path (commands, TUI interactions via PTY harness, HTTP/WS calls), and the
   assertions that matter. This is the acceptance criterion.

Keep prose tight. Name representative paths for repeated patterns rather than
enumerating every file.

If you were launched by `implement-roadmap`, also: update the epic's row in the
roadmap's **Epic status** table to `🔨` with the plan path in the **Plan**
column, and commit both files on a branch `docs/plan-<epic-id>` with a
`docs: plan <epic id> <title>` commit, then open and squash-merge a PR (fork CI
is docs-only fast) — or, if the orchestrator told you it will commit, leave the
files in the working tree and say so.

## 7. Report

Tell the user concisely:

- Which epic, and the **path** to the plan file you wrote.
- The **PR count**, the groups, the critical-path / wave shape, and the model
  assignment.
- The **decisions** you locked (and from where — roadmap default, rules, or
  their answers) and any you deliberately left open.
- New **dependencies** proposed, each with its reason.
- The suggested next step: `implement-epic <plan-name>` to build it, or
  `implement-task 1 <plan-name>` for a single PR — and that they should skim
  the plan first. Do not start implementing; this skill plans only.
