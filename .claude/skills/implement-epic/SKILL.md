---
name: implement-epic
description: Implement an entire epic from a plan file under docs/fork/plans/ by driving its tasks to completion — repeatedly launching subagents (each running the implement-task skill on opus) for every task whose dependencies are met, in dependency-ordered waves, until every task is ✅ and merged into master. Then validate the epic works end to end against real herdr servers (fleet lab), and mark the epic done in docs/fork/ROADMAP.md. Use when the user says things like "implement epic E1", "finish the e1-fleet-core plan", "do the whole gateway plan", or when invoked by implement-roadmap.
user-invocable: true
model: fable
argument-hint: '<plan-name-or-path> [--auto]'
---

# Implement a whole epic

You take a single plan file in `docs/fork/plans/` and drive **every** task in
it to a merged state on `master`, then prove the epic actually works end to
end. You do this by orchestrating — you do **not** implement tasks yourself.
Each task is implemented by a fresh subagent that runs the **`implement-task`**
skill on **`opus`**. You compute which tasks are ready, launch agents for them,
wait, reconcile from `origin/master`, and repeat until the plan is complete.

Read `AGENTS.md` and `.claude/rules/fork.md` first. This is a long-running
orchestration; stay methodical and keep the user informed between waves.

## Autonomous mode (`--auto`)

If `$ARGUMENTS` contains `--auto`, run the **whole epic unattended**:

- **Never call `AskUserQuestion`.** Every question becomes a deterministic
  default or a **defer-and-continue**: mark the affected task ⛔ blocked, keep
  driving the rest of the plan, collect everything into the final report. A
  stuck task must never freeze tasks that don't depend on it.
- **Pass `--auto` down to every subagent** so each `implement-task` run is
  itself unattended.
- **Validation degrades gracefully** (§6): prefer CLI/JSON/`curl` checks and
  PTY-driven TUI checks that run unattended; treat interactive browser checks
  as best-effort and never block on them.

Autonomous mode only removes the skill's own questions, not tool-permission
prompts; the session must already be permission-free for a walk-away run. If
you find you're blocked on permission prompts, say so in the report.

## 1. Resolve the plan

`$ARGUMENTS` is `<plan-name-or-path> [--auto]`.

- Accept a bare name (`e1-fleet-core`, `e1`) or a path
  (`docs/fork/plans/e1-fleet-core.md`); resolve to a file in `docs/fork/plans/`.
- **If no plan is given:** list `docs/fork/plans/*.md`. If exactly one has
  un-started or in-progress tasks, use it. Otherwise ask with
  `AskUserQuestion`. **In `--auto`:** never ask — if zero or more than one are
  plausible, stop and report.

Read the whole plan. Note its **status legend**, the **PR map table** (columns
`#`, `Depends on`, `Status`, and `Group`), the **wave preview and model
assignment**, the **Sequencing hazards**, and the **End-to-end epic
validation** section — that last one is your acceptance criterion.

Also read the epic's entry in `docs/fork/ROADMAP.md` and confirm its
dependency epics are ✅ in the Epic status table. If one is not, stop and
surface it (in `--auto`: report and stop — building on a missing epic is a
blocker, not a deferral). Then flip this epic's row to 🔨 (commit that on a
`docs/…` branch and merge it, or fold it into your first wave's reconciliation
if `implement-roadmap` told you it owns the roadmap table).

## 2. Build the task graph and the wave schedule

- Parse every PR-map row into `{ number, title, group, dependsOn: number[], status }`.
  `—` / empty = no dependencies. **Done** = ✅. **Ready** = not done and every
  dependency ✅.
- Sanity-check: dependencies pointing at missing numbers, cycles. If the graph
  is broken, stop and surface it.
- **Then schedule for conflicts, not just dependencies.** Read each ready
  task's **Files** list:
  - Two tasks editing the same upstream file (`src/config/model.rs`,
    `src/main.rs`, `src/cli/spec.rs`, `Cargo.toml`) go in different waves
    unless their regions are clearly disjoint and additive — and then each
    agent's prompt names the region it owns.
  - A `Cargo.toml`/`Cargo.lock`-touching task runs **alone** in its wave.
  - Front-load the task the most others depend on, even if it means a solo wave.
- **Concurrency cap: 2 agents per wave** (a Rust build plus nextest per agent
  is heavy; the gate wrapper serialises the gates, but builds and reviews still
  overlap). Raise to 3 only when the machine has ≥ 32 GiB and the tasks are
  small; never more.
- **Model per task:** `opus` for every task (the user's standing instruction).
  For tasks the plan flags for a `fable` **review** (auth/tokens, the SSH
  transport refactor, cross-host input routing), tell the agent to launch its
  review-and-harden agent with `model: "fable"`. If a task fails once and the
  failure reads as judgment rather than environment, retry it with
  `model: "fable"` and say so in the report.

Tell the user the starting state: tasks total, already ✅, the wave schedule
with the reasoning, and the model assignment.

## 3. Run the plan in waves

Loop until **every** task is ✅:

1. **Compute the ready set** from the current plan. Empty with tasks remaining
   → §5.
2. **Launch one subagent per scheduled task, in parallel, in a single message**
   (multiple `Agent` calls). `subagent_type: "general-purpose"`, `mode: "auto"`,
   `model: "opus"`, prompt per §4.
3. **Wait for the whole wave.** A silent agent is not necessarily stalled —
   check its worktree's `git -C .claude/worktrees/<slug> log --oneline` and the
   fork's PR list (`gh pr list --state all --head <branch>`) before concluding,
   and `SendMessage` it for a report rather than relaunching.
4. **Reconcile from `origin/master` directly** — never by switching the root
   checkout: `git fetch origin`, then `git log --oneline origin/master`,
   `git show origin/master:docs/fork/plans/<plan>.md` (grep the PR-map rows),
   and `gh pr view <url> --json state,mergedAt`. The remote is the source of
   truth; a report is a claim.
5. **Verify runtime claims cheaply where you can.** If an agent claims
   `herdr fleet status --json` showed two hosts, you can boot the fleet lab
   from a fresh `origin/master` build and run it yourself in minutes. Do this
   for at least one task per wave, and for every task whose report lacks
   pasted evidence.
6. **Propagate corrections.** Agents record what they learned in their own PR
   section; when it changes a later task's Files/Shapes, edit that later
   section too (commit on a `docs/…` branch, merge) so the next agent sees it.
7. **Report the wave** (tasks merged, PR URLs, findings worth surfacing,
   anything deferred), then loop.

**The one shared file — the plan itself.** Every task flips its own row and
adds prose; parallel branches will diverge on it. `implement-task` rebases onto
`origin/master` before merge; instruct each agent that a conflict in the plan
file is resolved by **keeping both sides** — its own ✅ flip and the sibling's —
never clobbering another row.

**Post-merge integration gate.** After each wave lands, run the gate once on a
fresh checkout of `origin/master` (`git worktree add .claude/worktrees/<epic>-integration origin/master`
then `bash scripts/fork/gate.sh <that worktree>`; read `EXIT=`). Two
individually green tasks can break each other; per-PR CI only tested each in
isolation against the master of its time. If red, treat it as a new fix task
(one agent, `fable` if judgment is involved) before the next wave.

## 4. The subagent prompt (per task)

Launch each agent with a prompt of this shape. **In `--auto`, include the
bracketed autonomous instruction.**

> You are implementing **one** task from a plan in the `vinceseguin/herdr`
> fork. Read `.claude/skills/implement-task/SKILL.md`, `AGENTS.md` (Universal
> Project Rules, Testing, Code Conventions) and `.claude/rules/fork.md`, and
> follow the skill **exactly, end to end**, for **task `<N>`** of
> `docs/fork/plans/<plan>.md` [`--auto`]. That means: create your own worktree
> off the latest `origin/master` (`git worktree add .claude/worktrees/<epic>-pr<N> -b <type>/<epic>-pr<N>-<slug> origin/master`),
> implement per the task's detail section, pass the gate through the wrapper
> (`bash scripts/fork/gate.sh <worktree>`, read `EXIT=`), run the skill's
> ultrathink review-and-harden agent [with `model: "fable"` because <reason>],
> validate against real herdr servers under an isolated `XDG_CONFIG_HOME`
> (fleet lab or `--session` servers) and paste the evidence, update the plan
> (flip task `<N>` to ✅ + fold in downstream notes), commit with conventional
> subject and the environment's trailers, push, open the PR against `master`,
> `gh pr checks --watch --fail-fast`, fix until green, and **squash-merge in the
> same turn**. [Run unattended: never ask the user anything — if you hit a
> blocker, stop cleanly and return it in your report; do not freeze.]
>
> You run in parallel with sibling agents: <siblings and the files they own>.
> Stay inside your worktree (`git -C <worktree> …`, `cd <worktree> && …` in the
> same call), commit early and often, never run bare `cargo` outside the gate
> wrapper while a sibling may be building, and never touch the root checkout or
> another worktree. [Your region in the shared file <file> is <region>; the
> sibling owns <region>.] If rebasing onto `origin/master` conflicts in the plan
> file, keep both sides. Do not touch tasks other than `<N>`.
>
> Return a short structured report: task number, branch, PR URL, gate command
> and `EXIT=`, CI/merge outcome, the review agent's findings (and model), the
> real-server evidence (pasted, not described), and anything you consciously
> deferred with a reason.

## 5. When a wave stalls

- **An agent failed to merge a task** (CI could not be made green, a spec
  ambiguity, a conflict it couldn't resolve): read its report and either
  (a) relaunch **one** agent for that task with the extra context (on `fable`
  if the failure was judgment), or (b) if it needs a human decision, ask with
  `AskUserQuestion`. Try a task at most **twice** before escalating.
  **In `--auto`:** retry at most twice, then mark the task ⛔ in the plan,
  record exactly why, and keep driving every task that doesn't depend on it.
- **Ready set empty but tasks remain:** surface which tasks are blocked and on
  what. **In `--auto`:** proceed to §6 with what did merge and list the blocked
  tasks in the report.
- **Never** force-merge, weaken the gate, delete tests, edit endpoint fixtures,
  or mark a task ✅ that wasn't implemented — in `--auto` too. Deferring is
  allowed; faking completion is not.

## 6. End-to-end epic validation

Once every task is ✅ and merged, prove the epic works as a whole on a fresh
build of `origin/master`. Drive the plan's **own "End-to-end epic validation"
section** — that, not a generic smoke test, is the acceptance criterion.

1. **Fresh checkout and build:** `git worktree add .claude/worktrees/<epic>-validate origin/master`,
   `bash scripts/fork/gate.sh <that worktree>` (read `EXIT=`), then
   `cargo build` there.
2. **Bring the lab up** under an isolated `XDG_CONFIG_HOME`:
   `scripts/fork/fleet-lab.sh up <n>` (or hand-started `--session` servers),
   `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH` on every call.
   Audit for leftovers first (`pgrep -af "herdr.*lab-"`; stale servers from a
   task's §7 have shadowed validations before).
3. **Exercise the real user path** the epic promised: e.g. `herdr fleet status
   --json` across N hosts and assert counts; a PTY-driven `herdr fleet` session
   asserting the sidebar lists every host and input lands on the right server;
   `curl`/WebSocket calls to the gateway asserting auth refusal without a
   token, a snapshot with it, and a terminal frame; the built PWA loaded
   against the gateway in the browser (best-effort in `--auto`). Assert on
   outputs and on server-side state (`herdr --session lab-a pane read …`), not
   on "it printed something".
4. **Capture evidence** (JSON, status codes, frame counts, screenshots) for
   the report.
5. **If validation reveals a defect, the epic is not done.** Treat it as a new
   task: add a row to the plan, launch one `implement-task` agent for it, get
   it merged, re-validate.
6. **Tear down** everything you started and remove the validation worktree.

## 7. Close the epic

- Flip the epic's row in `docs/fork/ROADMAP.md` **Epic status** to ✅ with the
  plan path (unless `implement-roadmap` told you it owns that table — then
  report the flip). Record any constraint later epics must honor in the
  roadmap epic's text if it changed materially. Commit on a `docs/…` branch,
  PR, merge.
- Consider an upstream sync if `git log origin/master..upstream/master` is
  long: `git fetch upstream`, merge on a branch, gate, PR, merge — or, in
  `--auto`, just report the drift and let `implement-roadmap` decide.

## 8. Report

Summarize the whole run:

- **Plan + scope:** which plan, tasks total, implemented this run vs already
  done.
- **Per wave:** the schedule you chose and why (conflict resequencing), tasks
  merged with PR URLs, review findings worth surfacing, models used, the
  post-merge integration gate result, anything deferred and why.
- **Plan file:** every task ✅, prose kept in sync with what shipped.
- **End-to-end validation:** exactly what you exercised, the evidence, and the
  verdict — does the epic work end to end, yes or no.
- **Roadmap:** the Epic status flip, and any downstream constraint recorded.
- **Anything still open:** ⛔ tasks, deferred items, decisions the user must
  make.
