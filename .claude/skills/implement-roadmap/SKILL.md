---
name: implement-roadmap
description: Implement the entire fork roadmap in docs/fork/ROADMAP.md end to end — for every epic in dependency order, plan it (plan-epic, fable) if no plan exists, then build it (implement-epic, fable, which runs implement-task agents on opus), validate it, and flip its row in the roadmap's Epic status table, continuing until every epic is ✅ or blocked. Use when the user says things like "implement the roadmap", "build everything", "do all the epics", or "run the whole plan".
user-invocable: true
model: fable
argument-hint: '[--auto] [--from <epic-id>] [--only <epic-id,...>] [--epics-parallel <1|2>]'
---

# Implement the whole roadmap

You drive **every epic** in `docs/fork/ROADMAP.md` to ✅. You do not plan or
implement anything yourself: for each epic you launch a **`plan-epic`** agent
(when no plan exists) and then an **`implement-epic`** agent — both on
**`fable`** — and `implement-epic` in turn launches **`implement-task`** agents
on **`opus`**. You own two things: the **Epic status** table in the roadmap,
and the machine's sanity (build concurrency, upstream drift, leftover
processes).

Read `AGENTS.md` and `.claude/rules/fork.md` first. This runs for hours; be
methodical, keep the user informed at every epic boundary, and never let a
single stuck epic freeze the ones that don't depend on it.

## Arguments

- `--auto` — the whole run is unattended (see below). This is the normal way to
  run this skill; without it you ask at each epic boundary whether to continue.
- `--from <id>` — skip epics before `<id>` in dependency order (they must
  already be ✅; otherwise stop and say so).
- `--only <ids>` — comma-separated epics to drive (dependencies must be ✅).
- `--epics-parallel <n>` — how many independent epics may run at once.
  **Default 1.** `2` is allowed only when the machine has ≥ 32 GiB RAM and ≥ 8
  cores, because each epic wave already runs up to 2 Rust builds plus reviewers.

## Autonomous mode (`--auto`)

- **Never call `AskUserQuestion`.** Pass `--auto` to every `plan-epic` and
  `implement-epic` you launch.
- Open decisions resolve to the roadmap's bolded defaults (that is what
  `plan-epic --auto` does).
- A blocked epic becomes ⛔ in the status table with the reason; its
  dependents are skipped and listed; everything else continues.
- Tool-permission prompts are a launch-flag matter; if you see them, say so in
  the report — you cannot work around them.

## 0. Preflight (once)

1. **Repo and remotes:** `git remote -v` shows `origin` =
   `vinceseguin/herdr` and `upstream` = `herdrdev/herdr`; `gh auth status` is
   logged in; `git fetch origin upstream`.
2. **Toolchain:** `just --version`, `cargo nextest --version`, `zig version`
   (0.15.2), `bun --version`, `python3 --version`. If any is missing and
   `scripts/fork/dev-setup.sh` exists, run it; if it does not exist yet, that
   is E0's first task — proceed only with E0 and let it install them.
3. **Machine budget:** note RAM and cores (`nproc`, `free -g`) and set the
   concurrency accordingly (§ Arguments). Check `df -h` on the repo's
   filesystem — each worktree gets its own `target/`; below 20 GiB free, warn
   and set `--epics-parallel 1`.
4. **Leftovers:** `pgrep -af "herdr.*lab-"`, `git worktree list`,
   `ls .claude/worktrees/`. Report leftovers; remove only worktrees whose
   branches are already merged into `origin/master` (`git branch --merged
   origin/master`). Never kill the user's real herdr.
5. **Upstream drift:** `git log --oneline origin/master..upstream/master | wc -l`.
   If large (> 100 commits) or older than two weeks, do one sync **before**
   starting: branch `chore/sync-upstream-<date>` off `origin/master`, `git merge
   upstream/master`, resolve (fork files under `docs/fork/`, `.claude/`,
   `scripts/fork/`, `src/fleet/`, `src/gateway/`, `web/` are ours; upstream
   files take upstream's side unless our minimal wiring must be re-applied),
   gate, PR, merge. In `--auto`, if the merge has conflicts you cannot resolve
   mechanically, skip the sync and record it.

## 1. Build the epic graph

Parse the **Epic status** table (`| Epic | Title | Depends on | Status | Plan |`)
into `{ id, title, dependsOn: [ids], status, plan }`. `—` = no dependencies.
Cross-check against each `### E<n> — …` section's **Depends on** line; if they
disagree, the table is the schedule and you fix the table to match the
section, committing the fix.

Sanity-check for missing ids and cycles; stop and surface if broken.

**Ready** = not ✅/⛔ and every dependency ✅. Tell the user: epics total, ✅
already, the ready set, and the order you intend (dependency order, lowest id
first among ties).

## 2. Drive epics

Loop until no epic is ready and none is running:

1. **Pick** up to `--epics-parallel` ready epics (lowest ids first). If two are
   picked together, they must not both edit the same upstream file heavily
   (compare their roadmap **Deliverables** and, if plans exist, their
   Sequencing hazards); otherwise run them one after another.
2. **Ensure a plan exists.** If the epic's **Plan** column is `—` or the file is
   missing: launch a `general-purpose` agent with `model: "fable"`,
   `mode: "auto"`, prompt per §3a. When it returns, verify
   `docs/fork/plans/<file>.md` exists on `origin/master` (it commits and merges
   the plan) and has a PR map with ≥ 1 row; set the **Plan** column and flip
   status to 🔨 if the agent did not already. If planning failed twice, mark
   the epic ⛔ with the reason and continue with others.
3. **Implement the epic.** Launch a `general-purpose` agent with
   `model: "fable"`, `mode: "auto"`, prompt per §3b. Wait for it. A silent
   agent is checked before it is judged: `gh pr list --state merged --search
   "<epic>"`, `git show origin/master:docs/fork/plans/<file>.md | grep -c ✅`,
   `git worktree list`. `SendMessage` it for a report before relaunching.
4. **Reconcile from `origin/master`:** the plan's PR-map rows all ✅ (or the
   report names ⛔ ones), the epic's end-to-end validation section reported
   with pasted evidence, and the roadmap row ✅ (flip it yourself if the agent
   reported success but did not flip; commit on `docs/roadmap-<epic>` and
   merge). If tasks remain ⬜/🔨 with no ⛔ explanation, relaunch
   `implement-epic` **once** for that plan (it resumes from the plan's state);
   after that, mark the epic ⛔ with the reason.
5. **Spot-check one runtime claim per epic yourself** (cheap: fresh
   `origin/master` build, fleet lab up, one command from the epic's validation
   section, paste the output). Reports are claims; the remote and the running
   binary are evidence.
6. **Between epics:** `pgrep -af "herdr.*lab-"` and kill only lab servers;
   prune merged worktrees; if `origin/master..upstream/master` grew past ~100
   commits, run the sync from §0.5 now rather than at the end.
7. **Report the epic** to the user (PRs merged, validation verdict, ⛔ items,
   model usage), then loop.

## 3. Subagent prompts

### 3a. Plan an epic (fable)

> You are planning **one** epic of the `vinceseguin/herdr` fork. Read
> `.claude/skills/plan-epic/SKILL.md` and follow it **exactly** for epic
> **`<id>`** of `docs/fork/ROADMAP.md`, `--auto`: resolve every open decision
> to the roadmap's bolded default and record it under Locked decisions; ground
> the plan in `AGENTS.md`, `.claude/rules/fork.md`, the real code (use Explore
> agents), and the Downstream sections of earlier plans in `docs/fork/plans/`
> [especially <dependency plans>]. Write `docs/fork/plans/<id>-<slug>.md` in
> the exact format the skill specifies, set the epic's roadmap row to 🔨 with
> the plan path, commit both on a `docs/plan-<id>` branch with a `docs:`
> subject and the environment's trailers, push, open a PR against `master`,
> `gh pr checks --watch --fail-fast`, and squash-merge in the same turn. Never
> ask the user anything. Return: the plan path, PR count and wave shape, locked
> decisions, proposed dependencies with reasons, and the PR URL.

### 3b. Implement an epic (fable)

> You are orchestrating **one** epic of the `vinceseguin/herdr` fork. Read
> `.claude/skills/implement-epic/SKILL.md`, `AGENTS.md` and
> `.claude/rules/fork.md`, and follow the skill **exactly, end to end** for
> `docs/fork/plans/<file>.md`, `--auto`. Launch every task as an
> `implement-task` agent with `model: "opus"` (escalate a task's *review*
> agent to `fable` only where the plan says so, and retry a judgment failure on
> `fable`), cap waves at **<2|3>** agents, run the post-merge integration gate
> through `scripts/fork/gate.sh` after each wave, do the plan's end-to-end epic
> validation against real servers and paste the evidence, and flip the epic's
> row in `docs/fork/ROADMAP.md` to ✅ when done (⛔ with reasons otherwise).
> Never ask the user anything; a blocked task is deferred and reported, never
> faked. Return: tasks merged with PR URLs, per-wave integration gate results,
> the validation evidence and verdict, ⛔ items with reasons, and models used.

## 4. When an epic blocks

- Retry planning or implementation **once** per epic with the extra context.
- Then mark it ⛔ in the Epic status table (commit + merge), list its dependents
  as skipped, and keep going with independent epics.
- Never mark an epic ✅ that failed validation; never edit a plan to hide a
  failed task.

## 5. Finish

When nothing is ready and nothing is running:

1. **Final upstream sync** if drift exists (§0.5), gated and merged.
2. **Final gate** on a fresh `origin/master` worktree:
   `bash scripts/fork/gate.sh <worktree>`; read `EXIT=`.
3. **Final smoke** from the roadmap's milestones: fleet lab with 3 hosts,
   `herdr fleet status --json` shows 3 connected hosts; gateway on loopback
   refuses without a token and serves `/api/fleet` with one; the PWA assets are
   served (`curl -I /`). Paste the output.
4. Clean up worktrees and lab processes.

## 6. Report

- **Roadmap outcome:** the final Epic status table, verbatim.
- **Per epic:** plan path, PR count and URLs, validation verdict with evidence
  pointers, models used, wall-clock time, ⛔ items with reasons.
- **Upstream syncs** performed and any conflicts resolved by hand.
- **Final gate and smoke** results, pasted.
- **Open decisions for the user:** everything resolved by default that they
  may want to revisit (list the default taken and where it is recorded), and
  every ⛔.
- **Next steps:** what to install where (`docs/fork/install.md` once E8 ships),
  and the exact commands to try MVP 1 and MVP 2.
