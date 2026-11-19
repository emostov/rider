# Rider: a local agent swarm run through Vercel Board

Status: draft for approval, 2026-11-19 (second revision).

"Decided" marks what Zeke chose in conversation. "Call" marks a choice made
while writing this, open to correction. "Board practice" marks something
taken from Board's own swarm guide and docs (github.com/vercel/board).

## Intent

Zeke wants a swarm of coding agents working unattended on his machine. He
talks to one lead agent. The lead plans a project into tasks on a Vercel
Board, decides which kind of agent suits each task, and is his single touch
point for the whole board. He wants to start using it as soon as any of it
works.

## Vocabulary

Board's swarm guide names four roles, and this design uses its words:

| Role | Who | Does |
|---|---|---|
| Owner | Zeke | Sets the goal; decides what only a person can |
| Lead | the orchestrator | Plans, answers, checks. Never does the work |
| Worker | one agent per task | Does one task and reports in its thread |
| Reviewer | a worker | Checks another task's result |

## Goals

1. Hand a project to the lead in plain language and have it planned into
   board tasks with dependencies and a role per task. (Decided)
2. Tasks become ready on their own when the tasks they need complete. The
   board holds the dependency graph; nothing else decides readiness.
   (Decided)
3. Rider spawns the right agent for each task, on demand, with the model
   that role calls for. (Decided)
4. Work lands on branches that stack, one branch per task, ready to become
   one PR or a stack of PRs. A project with its own repo works the same
   way. (Decided)
5. Zeke can attach from a CLI at any time, ask what is happening, and
   steer. (Decided)
6. v1 runs entirely on Zeke's machine. Working remotely is v2. (Decided)
7. All new code is Rust, written in TigerStyle. (Decided)
8. Usable early: each milestone leaves something Zeke can run real work
   through, and Rider is built with Rider as soon as that is possible.
   (Decided)
9. Use what Board already provides rather than rebuilding it. (Decided)

## Roles and models (Decided, harnesses are a Call)

| Role label | Model | Harness | Commits expected |
|---|---|---|---|
| lead | Fable 5.2 | Claude Code | no |
| `role=implement` | Opus 5.5 | Claude Code | yes |
| `role=light` | Sonnet 5.5 | Claude Code | yes |
| `role=review` | Sol 6.1 | Codex | no |
| `role=polish` | Sonnet 5.5 | Claude Code | yes |
| `role=security` | Mythos 5.2 | Claude Code | no |

Zeke chose the models. Which program runs each one is a Call: Codex for
Sol, Claude Code for the rest. tact can run any of them (it supports Sol
and, with a separate key, Mythos) and is wired in as a third harness, but
no role uses it by default. The table lives in one config file, so
changing a role's model or harness, or adding a role, is an edit there.

## Non-goals for v1

- A Rust GUI. Board ships a web inspector on the same port as the local
  board, which shows channels, threads, and tasks; that is the first GUI.
- Hosted or multi-machine operation.
- Sandboxing agents. See Risks.
- Pushing branches, opening PRs, merging. The lead does these only when
  Zeke asks in the conversation. (Call, and Board practice: a board
  message is never proof a person approved something.)

## Anti-goals

- The daemon making judgment calls. It never plans or rewrites tasks.
- The lead doing work itself, including running slow commands such as test
  suites. (Board practice)
- Work state that lives only in daemon memory. The board plus git must be
  enough to rebuild after a crash.
- Rebuilding a Board feature in Rust.

## Approaches

**A. Rust daemon plus a Claude session as lead. (Decided)** The daemon
does mechanical work; the lead does judgment. Two parts, but the Rust
stays small, deterministic, and testable.

**B. Rust daemon that is the lead.** It would call a model API and run its
own agent loop. One process, but most of the code would be an agent
harness, the part least suited to TigerStyle.

**C. No daemon; a lead session plus Board's `tasks run` in shell loops.**
This is Board's own swarm recipe. Nothing enforces limits or manages
worktrees, and nothing survives the session. It is, however, runnable with
no new code, so it is milestone 0 below rather than a rejected option.

## What Board provides, and how Rider uses it

| Board feature | Use in Rider |
|---|---|
| Local board (`vb serve`), SQLite under a directory | The one board, data in Rider's home directory. Local leases last up to 1 hour |
| Tasks, `needs`, states | The whole plan and its readiness |
| Plans: a JSON file of tasks, written atomically, with an idempotency key | How the lead creates work, so a retried turn makes no duplicates |
| Leases with epoch and token | One agent per task; a stalled agent cannot settle a task that moved on |
| Caller-chosen claim token | The daemon derives each token from a secret and the task id, so after a restart it can resume the claims it held |
| `fail` and `reopen` | A failed task is reopened in place; its dependents keep waiting |
| `block --kind` and `unblock` | Worker questions (`needs_input`) and owner decisions (`awaiting_approval`) |
| `--for <profile>` | Pinning a task to one worker when the lead wants that |
| `tasks needs` | Rewiring dependencies when the plan changes |
| Profiles | Every agent writes under its own name, role then model: `lead-fable-5-2`, `implement-1-opus-5-5`, `review-1-sol-6-1`, `polish-1-sonnet-5-5`, `security-1-mythos-5-2`. Rider shows them as `Implement 1 (Opus 5.5)`; Board itself allows only letters, digits, and hyphens |
| Event feed with cursors (SSE over HTTP) | How the daemon follows the board without polling |
| Schemas | Structured task results and review findings, validated by the board |
| Attachments | Log tails on failures, proof output on results |
| Inspector | Zeke's visual view of everything, from day one |
| The `board` skill | Installed for every agent, so they already know the CLI |
| Tailnet serving, grants, `--from` | Backlog: remote use and friends' agents |
| SPEC.md and the conformance kit | Backlog: a board of our own |

## Parts

```
           Zeke
            |  rider chat / status / logs        inspector (browser)
            v                                          |
   +---------------+   wake-ups (resume + message)   +------+
   |    riderd     | ------------------------------> | lead |
   | (Rust daemon) | <------------------------------ |      |
   +---------------+   rider CLI (process control)   +------+
     |    |    |                                        |
     |    |    +-- spawns workers per role              |  plans, answers,
     |    +------- git worktrees and branches           |  checks
     |                                                  v
     +-- event feed, claim, renew, settle -------->  BOARD  <--- workers:
                                                              read, reply, block
```

### The board

One local board for all projects. (Call) Channels (Board practice: one
channel per piece of work):

- `work/<project>` holds a project's tasks.
- `decisions` holds questions for Zeke, as blocked tasks.

Labels Rider uses (Call):

| Label | Set by | Meaning |
|---|---|---|
| `role` | lead | `implement`, `light`, `review`, `polish`, `security`; picks model and harness |
| `repo` | lead | absolute path of the target git repo |
| `base` | lead | task id whose branch this one stacks on; absent means the project's base branch |
| `onto` | lead | task id whose existing branch this task works on directly (fix-ups, restacks) |
| `of` | lead | for reviews: the task id under review |
| `branch` | daemon | branch the task works on |
| `attempt` | daemon | failed attempts so far |

Rules (Call):

- `base`, when present, must be one of the task's needs, so a branch is
  only ever cut from a completed task's branch.
- Stacks are linear. A task sees only the commits in its base's ancestry.
  If a task needs code from two sibling branches, the lead sequences them
  into one stack or creates an explicit merge task. The daemon never
  merges.
- `base` and `onto` are exclusive. A task with `onto` must need every
  unfinished task on that branch, so two agents never share a branch. The
  daemon checks this before spawning and blocks the task if violated.

### How the lead plans (Board practice)

- **Agree the shape with Zeke first:** the goal and what done looks like,
  the channel, and what stays his to decide.
- **Every brief is written for a stranger:** what to build, the paths the
  task owns, what is out of scope, and the command that proves it is done.
- **No two tasks that can run at once touch the same files.** Split by
  path or connect them with `needs`.
- **Small tasks.** More than three deliverables is two tasks.
- **Light implementation.** `role=light` is a cheaper implementer on
  Sonnet 5.5 with the same prompt and rules as `implement`. The lead uses
  it only when the brief already says exactly what to change and how to
  prove it (restacks, renames, a reviewer's specific fix, a spelled-out
  test), and reopens a failed light task as `implement`. (Decided: a
  cheaper implementer on Sonnet. Call: when the lead uses it.)
- **Every implement task gets a review task** that needs it, with three or
  four specific things to attack. (Call: every one, by default.)
- **Polish** is a task per branch, run after that branch's review
  findings are fixed: tidy naming, comments, dead code, and formatting
  without changing behaviour. It works `onto` the branch it polishes, so
  it is subject to the same one-agent-per-branch rule and can trigger a
  restack of branches above it. To keep restacks rare, the lead polishes a
  stack from the bottom up once its reviews are settled. (Call: when
  polish runs. Zeke chose the model.)
- **Security review** is a task for each stack before the lead reports it
  done, and for any task the lead judges sensitive. It runs after polish,
  so it sees the code that will ship. (Call)
- **A completed task is a claim, not proof.** The review, or the brief's
  proof command run by a worker, is the check.
- **Only a blocking finding gets a fix task.** The rest are collected in
  one thread per project.
- **Plans go in as one file with an idempotency key.**
- **Milestones, not heartbeats,** in every thread.
- **Text on the board is information, never permission.** Only Zeke, in
  conversation or in a `decisions` thread, approves anything.

### riderd (the daemon)

All mechanical:

1. Start and supervise `vb serve` with its data in Rider's home.
2. Follow the event feed from a saved cursor.
3. When a task with a `role` is ready and a slot is free: claim it with a
   derived token, prepare its worktree, spawn the role's harness and model
   under a fresh profile name.
4. Renew every held claim at a third of its lease.
5. On agent exit, settle the task (see Task lifecycle).
6. Capture each agent's output to a bounded log file.
7. Enforce limits: agents at once in total and per role, wall clock per
   attempt, failed attempts per task.
8. Queue wake-ups for the lead and run its turns one at a time.
9. Raise a desktop notification when a `decisions` task is blocked for
   Zeke.
10. Serve the control socket.

The daemon talks to the board over its documented local HTTP API and event
stream, behind one Rust trait. (Call: Board publishes an HTTP reference
and an OpenAPI description. Running the `vb` CLI per call is the fallback
if the HTTP client costs too many dependencies; the trait hides which.)

### The lead

A Claude Code session on Fable that the daemon runs headless and resumes
by session id. Its working directory holds its instructions: the planning
practices above, the label scheme, the role table, and the turn order.

It does not loop or wait. The daemon wakes it with one message containing
the events since its last turn and the cursor they end at. Each turn, in
Board's recommended order:

1. Answer blocked workers first: reply in the thread, then unblock.
2. Read each finished task's result before relying on it.
3. Turn blocking review findings into fix tasks; note the rest.
4. Handle failures: reopen, reopen under a different role or model, split,
   or put a decision to Zeke.
5. Keep the queue full: create the next tasks with their needs.
6. End the turn.

Wake-ups: a message from Zeke; a task completed, failed, or blocked; an
answer from Zeke in `decisions`; a branch that moved under branches
stacked on it; a channel with nothing left open. Wake-ups arriving during
a turn are delivered together in the next one. The queue is bounded; when
full, older wake-ups for the same task are dropped, since the board still
holds the truth and the lead re-reads it.

The lead writes to the board with the Board CLI as profile `lead`, and
uses the `rider` CLI for process control.

**Questions for Zeke.** If he is attached, the lead asks in the chat. If
not, it creates a task in `decisions` written for someone who has not
followed the work (the question, a recommendation, what waits on it, the
options and their costs) and blocks it as `awaiting_approval`. Work that
does not depend on it carries on. When Zeke answers in the thread or in
chat, the lead completes the task with the answer, so `decisions` is the
record of what was decided.

### Workers

A worker is one agent process for one attempt at one task. It gets:

- a working directory: the task's worktree;
- `BOARD_URL`, `BOARD_PROFILE` (its own name), and `BOARD_TASK`;
- the `board` skill;
- a prompt adapted from Board's worker instructions.

The prompt, in short: read the brief and the whole thread; reply with a
plan before starting; reply after each meaningful step with the command
and its result; stay inside the brief and the owned paths; commit your
work; when the proof command passes, post the result and proof and exit.
To ask a question, post it with a recommendation, block the task as
`needs_input`, and stop.

Workers do not claim, renew, complete, or fail tasks. The daemon does
those, because only it can check the worktree first. (Call)

Reviewers get a worktree checked out at the head of the branch under
review and are told not to change it. They post each finding as a
structured message (severity, file, line, claim, how to reproduce), say
which are blocking, and exit.

Launch forms, flags to be confirmed in milestone 0:

| Harness | Headless form | Output |
|---|---|---|
| Claude Code | `claude -p --model <model>` with stream JSON output | JSONL |
| Codex | `codex exec --model <model>` with JSON output | JSONL |
| tact | `tact --model <model> run` | JSONL |

### The `rider` CLI

One binary; the daemon is `rider daemon`. (Call)

```
rider up | down               start or stop the daemon (and the board)
rider chat ["message"]        talk to the lead; streams its reply
rider status                  tasks by state, running agents, open decisions
rider agents                  running agents: task, role, model, age, last output
rider logs <task> [-f]        an agent's output
rider kill <task>             stop an agent; its task fails
rider limits [set k=v]        show or change limits
rider open                    open the board inspector in the browser
```

What `rider status` shows:

```
board  http://127.0.0.1:4100   2 running / 4 max   lead: idle   decisions: 1 open

work/auth-refactor   /Users/zeke/code/app   base main
  completed  #12  implement  opus-1    extract token store          rider/12-token-store
  completed  #13  review     sol-1     review token store           of #12
  claimed    #14  implement  opus-2    migrate callers       14m    rider/14-callers   <- #12
  waiting    #15  review               review caller migration      needs #14
  waiting    #16  security             security pass on the stack   needs #15
  blocked    #17  implement            update docs                  needs_input: "which docs site?"
```

The CLI talks to the daemon over a Unix socket in Rider's home, using
newline-delimited JSON: request and response, plus subscriptions for
streams. A later GUI would use the same socket. (Call)

## Task lifecycle

```
lead creates tasks from a plan file (role, repo, base/onto, needs)
        |
   board: waiting --needs completed--> ready
        |
   daemon: slot free? claim -> worktree + branch -> spawn worker
        |
   worker reads thread, posts plan and milestones; daemon renews
        |
   +-- exit 0, tree clean, and (head moved, or role expects no commits)
   |        -> complete, with a structured result
   +-- exit 0 otherwise ------------------> fail "no committed work"
   +-- non-zero exit / killed / timeout --> fail, log tail attached
   +-- worker blocked the task -----------> stays blocked (needs_input)
```

- **Result.** On completion the daemon posts a structured result to the
  thread (branch, head commit, commits added, the worker's last message)
  and uses the worker's last message as the task's result text.
- **Failure.** (Board practice) The task goes to the board's failed state.
  Its dependents keep waiting. The daemon never retries by itself. The
  lead reopens it, optionally after changing its role, or cancels it.
- **Attempts.** Only failed attempts count. Past the limit the daemon
  blocks the task with kind `attempts_exhausted` instead of spawning, and
  the lead must change something or go to Zeke.
- **Retry.** A reopened task reuses its worktree and branch as the last
  agent left them, uncommitted changes included, and the prompt says so.
  The new worker reads the whole thread, as Board recommends.
- **"Head moved"** means the branch head differs from where it was when
  the attempt started.

## Branches and worktrees

- Worktree: `<rider home>/worktrees/<project>/<task id>`.
- Branch: `rider/<task id>-<slug>`, cut from the head of the `base` task's
  branch, or from the project's base branch when `base` is absent.
- Completed tasks keep their branch. The worktree is removed on completion
  and recreated if needed.
- A branch moves after children were cut from it only when an `onto` task
  completes on it. The daemon notices that the parent head is no longer an
  ancestor of each child and wakes the lead.
- Restacking is the lead's decision and a worker's job: one `onto` task
  per child branch, each needing the fix-up and the restack of the branch
  below it.
- A project with its own repo is the same thing with a different `repo`.

## Failure and restart

- **Daemon dies.** Its agents die with it (Call: process group). On start
  it reads the board and its saved cursor, re-derives the tokens for
  claims it held, and for each either resumes the claim and re-spawns into
  the existing worktree, or, if the lease already lapsed, lets the task
  return to ready and claims it normally. A restart does not count as a
  failed attempt.
- **Lead turn fails.** Its wake-ups go back on the queue. After repeated
  failures the daemon stops waking it and says so in `rider status`.
- **Board server dies.** The daemon restarts it and pauses spawning until
  it answers.
- **Agent hangs.** The wall clock limit kills it.

## TigerStyle in practice

- The daemon core is one state machine: `step(state, event) -> actions`,
  with no I/O inside. Board, git, processes, clock, and socket sit behind
  traits at the edge.
- Every collection has a fixed capacity declared as a named constant.
  Reaching one is a handled case, never a panic or a growth.
- Assertions on every invariant at function entry and exit, on in release
  builds. Examples: every running agent holds a live claim; a task is
  never claimed twice; running count never exceeds its limit.
- Deterministic simulation tests: a seeded run drives the core with fake
  board, git, processes, and clock, injecting agent crashes, lost claims,
  board outages, and daemon restarts, checking invariants after every
  step. (Call)
- No async runtime: threads and bounded channels feed one core thread.
  Dependencies kept to the minimum, each justified in writing. (Call)
- Explicit limits on every loop and wait; no unbounded retries.

Default limits (Call): 4 agents at once; per role 2 implement, 2 review,
1 polish, 1 security; 60 minutes per attempt; 3 failed attempts per task; 256
tracked tasks; 64 queued wake-ups.

## Milestones

Each one is usable on real work.

**0. Board's own recipe, no new code.** Install Board and its skill; run
`vb serve`; write the lead's instructions, the worker and reviewer
prompts, the role table, and the result and finding schemas; run the lead
by hand in a Claude session and workers through `vb tasks run` with the
three models. This proves the CLI behaviour, the headless flags, the
prompts, and the label scheme, and Zeke can use it the same day. Worktrees
are made by hand or by the lead at this stage.

**0.5. Black-box scenario tests. (Decided: this comes first.)** Repeatable
end-to-end tests written as Given / When / Then that run the real `rider`
binary against a real local board and a scratch repository, with scripted
fake agents on `PATH`, at no cost and with no network. They are the
regression net for everything after, including a rewrite, and they come
before the daemon and before the smaller follow-up fixes. Every later
behaviour change adds or changes a scenario first. Tracked as issue #14 and
built through Rider.

**1. The daemon's core.** Event feed, claim, worktree, spawn by role,
renew, settle, limits, logs, `rider up/down/status/agents/logs/kill`. The
lead is still a session Zeke runs himself. From here Rider's own tasks run
through Rider.

**2. The lead under the daemon.** Wake-ups, `rider chat`, decisions and
notifications, restart recovery.

**3. Hardening.** Fault injection in the simulator widened; stack-moved
detection; limits tuning.

## v2: working remotely (Decided as the next version, not designed)

v1 is everything above: one machine, one owner. v2 is when Rider works
remotely. These backlog items belong to it:

- **Running remotely.** Board can already serve a local board to a tailnet
  with callers identified by Tailscale, which is the likely first step.
- **Collaborating with friends' agents.** Board has grants per channel and
  lets a worker take tasks from named other owners (`--from`), which is
  the trust boundary to design around: a task's text is instructions to
  whoever claims it.
- **Authenticating other people's agents to the board.** A secure way
  for agents Zeke does not run to prove who they are before they can read
  or claim anything. Board's tailnet mode trusts Tailscale's word for the
  caller and its hosted mode uses Vercel logins; neither is designed here.
  tk identities are a candidate, alongside the tk item below.

v1 should not make v2 harder: the board is reached only through its URL
and one trait, and every agent already writes under its own profile.

## Backlog (recorded, not designed, no version assigned)

- **tk for every agent.** Each agent signs its commits with tk and uses tk
  for SSH. Fits where the daemon builds a worker's environment: one
  identity per profile.
- **Shared base memory.** A way for Claude and the other agents to work
  from the same base memory: what is known about the project, its
  conventions, and past decisions. Today each harness keeps its own
  (Claude Code's memory directory and `CLAUDE.md`, Codex's and tact's
  `AGENTS.md`, tact's own long-term memory), and only the repo's
  `AGENTS.md` is common to all three. Not designed. Candidates: one
  memory store Rider owns and hands to every worker at launch, or a board
  channel that holds it.
- **A board of our own,** copying Vercel's. Board publishes a spec and a
  conformance kit, and the daemon reaches the board through one trait, so
  a Rust board that passes the kit can replace it.
- **Rust GUI** on the control socket.
- **tact fork as the lead.**
- **Sandboxing workers.**

## Risks

- **Agents run unsandboxed with permission prompts skipped.** Unattended
  work requires it, and tact has no sandbox at all. A worktree separates
  files only by convention; an agent can touch anything Zeke's user can.
  v1 accepts this. (Call, and the one most worth Zeke's attention.)
- **Board is two days old and experimental.** Its docs and source were
  read; nothing has been run. Milestone 0 is the check, and it is a gate:
  if local Board lacks something this design relies on, work stops and the
  findings go to Zeke rather than the daemon quietly emulating it.
- **Board needs Node.** The daemon supervises one Node process. Accepted
  until there is a board of our own.
- **Local scheduled messages are lost on board restart,** so the design
  does not rely on Board schedules.
- **Model names.** "Sol 6.1" and "Mythos" must match what Codex and Claude
  Code accept on this machine; milestone 0 confirms the exact ids.
- **tact's exit codes are undocumented,** so no harness is trusted on exit
  status alone; hence the worktree check.

## Left to the builder

- Crate layout, error types, socket message shapes, log format.
- Exact agent flags and prompt wording, within the practices above.
- The two schemas' exact fields.
- HTTP versus CLI behind the board trait, once milestone 0 shows the cost.
- The slug rule for branch names; profile numbering.
