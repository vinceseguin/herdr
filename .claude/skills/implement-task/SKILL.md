---
name: implement-task
description: Implement a single numbered task (PR) from a plan file under docs/fork/plans/, run the fork gate (just ci under scripts/fork/gate.sh), launch a review agent that hardens the code (ultrathink), boot real herdr servers locally and validate the change end to end, mark it done in the plan's status table, open a pull request on the fork, watch fork CI, and squash-merge. Use when the user says things like "implement task 3 of e1-fleet-core", "do PR 2 of the plan", "ship the next task in the fleet plan", or when invoked by implement-epic.
user-invocable: true
model: opus
argument-hint: '<task-number> [plan-name-or-path] [--auto]'
---

# Implement a plan task

You implement **one** numbered task (a "PR" in the plan's terminology) from a
plan file in `docs/fork/plans/`, take it all the way through the fork's
verification gate and real-server validation, then update the plan: flip the
task's status to ✅ and revise any part of the plan that the implementation
changed.

This is a real implementation task, not a review. You will write Rust (and
possibly TypeScript for `web/`), run tests, edit the plan file, and merge a PR.
Read `AGENTS.md` (Universal Project Rules, Testing, Code Conventions) and
`.claude/rules/fork.md` before touching code — they override defaults, and
`fork.md` says which parts of `AGENTS.md` do not apply here.

## Autonomous mode (`--auto`)

If `$ARGUMENTS` contains `--auto`, run **unattended** — the user expects to walk
away and come back to a merged PR. Never call `AskUserQuestion`; every point
where the interactive flow would ask becomes a **deterministic default +
continue, or a clean stop with a report** (the per-step rules say which).
Collect anything you'd have asked into the final report.

Autonomous mode only removes the skill's own questions. It does not remove
tool-permission prompts (`git push`, `gh pr merge`); for a true walk-away run
the session must already be permission-free. If you detect you're blocked on
permission prompts in `--auto`, say so plainly in the report.

## 1. Resolve the task and the plan

`$ARGUMENTS` is `<task-number> [plan-name-or-path] [--auto]`.

- **Task number** — the first token (e.g. `3`). Matches the `#` column in the
  plan's "PR map" table.
- **Plan** — the second token, if present. Accept a bare name (`e1-fleet-core`,
  `e1`) or a path (`docs/fork/plans/e1-fleet-core.md`); resolve to a file in
  `docs/fork/plans/`.
- **If no plan is given:** list `docs/fork/plans/*.md`. If exactly one plan has
  un-started (⬜) or in-progress (🔨) tasks, use it. If more than one is
  plausible, ask with `AskUserQuestion`. **In `--auto`:** never ask — if zero or
  more than one are plausible, stop and report that the plan was ambiguous.

Read the whole plan file before touching code. Note its status legend and find
the row for the task number.

## 2. Sanity-check before implementing

- **Already done?** If the task is ✅, do not re-implement. Show the row and ask
  whether they meant the next un-started task. **In `--auto`:** stop cleanly and
  report it (never silently jump to a different task).
- **Dependencies met?** Check "Depends on". If a dependency is still ⬜/🔨,
  surface it and ask whether to proceed or switch. **In `--auto`:** treat it as
  a blocker — stop and report which dependency is missing.
- **Read the task's detail section in full** — file paths, symbols to reuse,
  tests, real-server validation recipe, and constraints earlier PRs imposed.
- **Toolchain present?** `just --version`, `cargo nextest --version`, `zig
  version` (0.15.2), `bun --version`. If missing and E0's
  `scripts/fork/dev-setup.sh` exists, run it; otherwise stop and report (this
  is a machine-setup blocker, not something to work around).

## 3. Branch and worktree

**Always branch off the latest `origin/master`** (the fork's default branch is
`master`): `git fetch origin`, then `git worktree add .claude/worktrees/<epic>-pr<N> -b feat/<epic>-pr<N>-<slug> origin/master`.
Use the type prefix that fits (`feat/`, `fix/`, `docs/`, `ci/`). Never commit
straight to `master`.

Always work in that **isolated worktree**, even when invoked alone — sibling
agents and other sessions share the root checkout, and a `git switch`/`reset`
there would drop your uncommitted files. Every `git` call is
`git -C <worktree> …`; every `cargo`/`just` call is `cd <worktree> && …` in the
same Bash invocation (cwd does not reliably persist between calls). Commit early
and often. Remove the worktree after merge.

If your branch falls behind `origin/master` before merge, `git -C <worktree>
rebase origin/master` and re-run the gate.

## 4. Implement

- Explore the surrounding code first; reuse existing utilities, types and
  patterns the plan's "Critical files referenced" names rather than inventing
  new ones.
- Honor every rule: additive modules under `src/fleet/`, `src/gateway/`,
  `web/`; minimal wiring in upstream files; no change to `src/protocol/wire.rs`
  or the endpoint contract; pure state testable without sockets; no `unwrap()`
  in production code; `tracing`, not `eprintln!`, for logs (CLI user-facing
  output excepted); `#[cfg(unix)]`/`#[cfg(windows)]` gating for OS code; no
  new dependency without the plan's reason; perf discipline in render and
  fanout loops.
- Write the tests the task calls for: unit tests next to the code
  (`#[cfg(test)] mod tests`), integration tests under `tests/` using
  `tests/support/mod.rs` when servers are involved. Cover the failure path
  (host unreachable, bad token, incompatible generation) not just the happy one.
- Match the plan's scope; correctness and the conventions win over LOC targets.

## 5. Gate (must pass before marking done)

Run the fork gate from your worktree, through the wrapper — never a bare
parallel `cargo` run while another agent may be building:

```bash
bash scripts/fork/gate.sh <worktree>
echo "EXIT=$?"          # the only thing that decides green/red
```

The wrapper runs `just ci` (fmt check, clippy `-D warnings`, `cargo nextest`,
python maintenance tests, bun suites) under a machine-wide lock and prints
`EXIT=<code>` last. Never pipe it into `tail`/`head`/`grep`. While iterating,
`bash scripts/fork/gate.sh <worktree> "test-one <filter>"` runs one nextest
filter under the same lock.

All green, no weakening: no `#[allow]` without a justifying comment, no deleted
or `#[ignore]`d tests, no fixture edits under `tests/fixtures/endpoint-*`. If
anything fails, fix it before proceeding. (This gate proves the code compiles
and tests pass; the real runtime validation is §7.)

## 6. Review and harden (agent — must run)

The intent is that this code is **done for good**. Before opening the PR, hand
the work to a fresh reviewer agent and let it fix what it finds — even if the
gate is green.

Launch **one** `general-purpose` agent with the `Agent` tool, `mode: "auto"`.
**Model:** `opus` by default. Use `model: "fable"` when the task touches auth
or tokens (gateway), the SSH transport refactor, input/command routing across
hosts (a bug there types into the wrong machine), or anything the plan flagged
for a `fable` review. Instruct it to:

- **Think with `ultrathink`** — put the literal word in its prompt.
- **Review the full diff of this task against `origin/master`**
  (`git -C <worktree> diff origin/master...HEAD` plus uncommitted changes), not
  the whole repo. Judge it against `AGENTS.md`, `.claude/rules/fork.md`, and
  the task's spec in the plan.
- **Hunt for real problems:** correctness and edge cases (reconnect races,
  partial frames, snapshot `boot_id` changes, id collisions across hosts),
  security (token handling, origin checks, path traversal in static serving,
  anything that lets a read-scoped client write), silent mis-routing of input
  to the wrong host/pane, `unwrap()`/panics in production paths, blocking calls
  on render or fanout paths, platform-gating gaps, missing failure-path tests,
  drift from the plan or from the endpoint-contract rules.
- **Fix what it finds** in the worktree — it has edit and Bash access — and
  keep the gate green (through the wrapper, one gate at a time). Report each
  issue and how it was resolved or consciously deferred.
- **Not weaken the build to pass.**

When it returns, read the report. If it deferred anything material or a fix
looks wrong, do another pass. Re-run §5 yourself to confirm green after its
edits.

## 7. Validate end to end against real servers (must pass before the PR)

A green unit gate on code that does not work at runtime is not done. Exercise
the change against **real running herdr servers**, isolated from the user's
live herdr:

1. **Build** the debug binary: `cd <worktree> && cargo build` (under the gate
   lock if another agent may be building: `bash scripts/fork/gate.sh <worktree> build`
   is acceptable, or just accept cargo's own directory lock).
2. **Isolate:** `export XDG_CONFIG_HOME=/tmp/herdr-<epic>-pr<N>` and run every
   herdr command with `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH`.
   Use `scripts/fork/fleet-lab.sh up <n>` when it exists (E0); otherwise start
   servers by hand: `target/debug/herdr --session lab-a server` in the
   background, then `target/debug/herdr --session lab-a pane run …` to give it
   something visible.
3. **Wait until they're up** (`herdr --session lab-a status server`, or poll the
   client socket path) before probing.
4. **Exercise the real change** as the plan's "Real-server validation" says:
   `herdr fleet status --json` and assert the JSON; `curl -i` the gateway and
   assert status codes and bodies; observe a terminal stream and assert frame
   records; drive the TUI through a PTY harness (see `tests/multi_client.rs`)
   for sidebar/host-switch behaviour; for `web/`, run the built assets against
   the gateway and check the browser console with the Chrome tools when
   available.
5. **Capture the evidence** — real command output, status codes, JSON, frame
   counts, screenshots — for the §11 report. Claims are not evidence.
6. **Tear down:** `fleet-lab.sh down` or `herdr --session lab-a server stop`;
   make sure no `target/debug/herdr` you started is left running
   (`pgrep -af "herdr.*lab-"`).
7. **If it fails at runtime, it is not done** — fix, re-run §5, re-validate.
   **In `--auto`:** only skip a check that genuinely cannot run here (needs a
   phone, a second physical machine, a real tailnet) and say so explicitly in
   the report, naming the local stand-in you used instead.

## 8. Update the plan file

1. **Flip the status** of this task's row in the PR map table to ✅.
2. **Update anything the implementation changed** — a discovered constraint, a
   renamed symbol, an added dependency, a decision that affects a later task.
   Put it in this PR's section and, if it changes what a later task must do,
   also in that later task's **Files**/**Shapes** so the next agent sees it.
   Fold in anything the review agent changed.
3. Note briefly any scope difference future readers need.

If you were launched by `implement-epic`, it may instead tell you it owns the
status table — then edit only your own `### PR <N>` section's prose and report
the flip for the orchestrator to make.

## 9. Commit, push, and open the PR

1. **Commit** all the work on the task branch with a lowercase conventional
   subject (`feat(fleet): merged fleet state and host ids (PR-3)`), a body
   summarising what and why, and the trailer lines the environment specifies
   (`Co-Authored-By: …` and `Claude-Session: …`). The `.githooks/commit-msg`
   hook validates the subject if hooks are installed.
2. **Push** the branch: `git -C <worktree> push -u origin <branch>`.
3. **Open the PR** with `gh pr create --base master`. Title = the commit
   subject (fork CI validates it as a conventional-commit title). Body: what
   was implemented, the review agent's findings and fixes, the gate result,
   the real-server evidence, and the `🤖 Generated with [Claude Code]` line the
   environment specifies. Capture the PR URL.

## 10. Watch CI and merge

1. **Watch the checks synchronously and merge in the same turn:**
   `gh pr checks <pr> --watch --fail-fast`, then step 2 immediately. Do not
   hand off to a background watcher and end your turn — your turn ends when the
   PR is **merged** or genuinely blocked.
2. **If green**, `gh pr merge <pr> --squash --delete-branch`. Confirm.
3. **If red**, `gh run view <run> --log-failed`, fix in the worktree, re-run §5,
   commit, push, and watch again.
4. **Loop** until green. If a failure is clearly outside the task (fork CI
   itself broken, runner outage) or you're stuck after two honest attempts,
   stop and surface it rather than force-merging.

Then `git -C <root> worktree remove <worktree> --force && git worktree prune`,
and remove `/tmp/herdr-<epic>-pr<N>`.

## 11. Report

Summarize concisely:

- What you implemented and the key files touched (new modules vs upstream
  files wired).
- The gate result (the exact wrapper command, `EXIT=0`) both after
  implementation and after the review agent's fixes.
- **The real-server validation (§7):** what you booted, the exact commands, and
  the evidence (JSON, status codes, frames, screenshots) — or, in `--auto`, an
  explicit note of what could not run here and the stand-in used.
- **The review agent's findings** (model used) and how each was resolved.
- What you changed in the plan (status flip + prose), and anything that affects
  later tasks.
- The branch, the **PR URL**, the CI result, and confirmation that it was
  squash-merged into `master`.
