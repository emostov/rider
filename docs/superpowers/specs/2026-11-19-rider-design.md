# Rider: a local agent swarm run through Vercel Board

Status: draft for approval, 2026-11-19.

"Decided" marks what Zeke chose in conversation. "Call" marks a choice made
while writing this, open to correction.

## Intent

Zeke wants Claude Code, Codex, and tact working unattended on his machine as
a swarm. He talks to one orchestrator. The orchestrator plans a project into
tasks, decides which agent type suits each task, and is his single touch
point for the whole board. The swarm coordinates through a Vercel Board
running locally.

## Goals

1. Hand a project to the orchestrator in plain language and have it planned
   into board tasks with dependencies and an agent type per task. (Decided)
2. Tasks start on their own when their dependencies finish. The board holds
   the dependency graph; nothing else decides readiness. (Decided)
3. The system spawns agents of the right type per task, on demand.
   (Decided)
4. Work lands on branches that stack, one branch per task, ready to become
   one PR or a stack of PRs. A project with its own repo works the same way.
   (Decided)
5. Zeke can attach from a CLI at any time, ask what is happening, and steer.
   The same interface can back a Rust GUI later. (Decided)
6. Everything runs locally. (Decided)
7. All new code is Rust, written in TigerStyle. (Decided)

## Non-goals for v1

- The GUI. Only the socket protocol it would use.
- Hosted or multi-machine operation.
- Sandboxing agents. See Risks.
- Pushing branches or opening PRs automatically. The orchestrator does this
  only when Zeke asks it to. (Call)
- Forking tact to serve as the orchestrator. Zeke offered this as a later
  option; v1 uses a normal Claude session. (Decided)
- Cost or token accounting.

## Anti-goals

- The daemon making judgment calls. It never plans, retags, or rewrites
  tasks.
- Workers that need to understand the board, leases, or claiming.
- Work state that lives only in daemon memory. If the daemon dies, the board
  plus git must be enough to rebuild.

## Approaches

**A. Rust daemon plus a Claude session as orchestrator. (Decided)**
The daemon does mechanical work; the orchestrator session does judgment.
Two moving parts, but the Rust stays small, deterministic, and testable.

**B. Rust daemon that is the orchestrator.** It would call a model API and
run its own agent loop. One process, but most of the code would be an agent
harness, which is the part least suited to TigerStyle.

**C. No daemon; an orchestrator session with a skill.** Quick to stand up,
but nothing enforces limits, nothing survives the session ending, and there
is nothing to attach a GUI to.

## Parts

```
          you
           | rider chat / status / logs
           v
   +----------------+   wake-ups (resume with a message)   +--------------+
   |    riderd    | -----------------------------------> | orchestrator |
   |  (Rust daemon) | <----------------------------------- | (claude -p)  |
   +----------------+   rider CLI (process control)      +--------------+
     |    |     |                                                 |
     |    |     +--- spawns: claude | codex | tact                | plans:
     |    +--------- git worktrees and branches                   | tasks, needs,
     +-------------- claims, renews, completes  --->  BOARD  <----+ agent tags
```

### The board

One local board, served by Board's local mode, for all projects. (Call)
Each project is a channel. A task is a Board task: a thread with managed
state, dependencies, and labels.

Labels the system uses (Call):

| Label | Set by | Meaning |
|---|---|---|
| `agent` | orchestrator | `claude`, `codex`, or `tact` |
| `repo` | orchestrator | absolute path of the target git repo |
| `base` | orchestrator | task id whose branch this task stacks on, or absent to use the project's base branch |
| `onto` | orchestrator | task id whose existing branch this task works on directly, instead of getting a branch of its own (restacks and fix-ups) |
| `branch` | daemon | branch name the task works on |
| `attempt` | daemon | failed attempts so far |

Rules (Call):

- `base`, when present, must be one of the task's needs. So a branch is
  only ever cut from a finished task's branch.
- Stacks are linear. A task sees only the commits in its base's ancestry.
  The orchestrator plans accordingly: if a task needs the code of two
  sibling branches, the orchestrator either sequences the siblings into
  one stack or creates an explicit merge task. The daemon never merges.
- `base` and `onto` are exclusive. A task with `onto` must need every
  unfinished task that works on that branch, so two agents never share a
  branch at once. The daemon checks this before spawning and blocks the
  task if it is violated.

### riderd (the daemon)

Responsibilities, all mechanical:

1. Start and supervise the local board server.
2. Follow the board event log.
3. When a task with an `agent` label is ready and a slot is free: claim it,
   create its worktree and branch, spawn the agent.
4. Renew the lease of every running task well inside the 5 minute limit.
5. On agent exit, finish the task (see Task lifecycle).
6. Capture each agent's output stream to a bounded log file.
7. Enforce limits: agents at once in total, agents at once per type, wall
   clock per attempt, attempts per task.
8. Queue wake-ups for the orchestrator and run its turns one at a time.
9. Serve the control socket.

It talks to the board by running the Board CLI with `--json`, behind one
Rust trait. (Call: Board documents a CLI and a JS SDK, no HTTP reference.)

### The orchestrator

A Claude Code session that the daemon runs headless and resumes by session
id. It has a working directory of its own containing its instructions: how
to plan, the label scheme, how to choose an agent type, how to stack, and
what each wake-up means.

It writes to the board directly with the Board CLI, and uses the `rider`
CLI for process control. It does not loop. It runs one turn per wake-up:

| Wake-up | Typical response |
|---|---|
| Zeke sent a message | answer, plan, or replan |
| Task failed or hit its attempt limit | retry, retag for another agent, split, or ask Zeke |
| Task blocked by its agent | resolve or ask Zeke |
| A branch moved under branches stacked on it | create a restack task |
| A channel has no open tasks left | summarize for Zeke |

Wake-ups that arrive during a turn are queued and delivered together in the
next one. The queue is bounded; when full, the oldest of the same kind for
the same task is dropped, because the board still holds the truth.

### Workers

A worker is one agent process for one attempt at one task. The daemon gives
it a prompt containing the task text, the thread so far, and the rules; a
working directory that is the task's worktree; and three commands (Call):

- `rider task note "<text>"` posts progress to the task thread.
- `rider task block "<reason>"` marks the task blocked and ends the attempt.
- `rider task noop "<why>"` records that the task needed no changes.
- `rider task context` prints the task and the threads of the tasks it needs.

Workers never touch the board directly. This keeps the three agent types
interchangeable and keeps Board's experimental CLI out of their prompts.

How each is launched (flags to be confirmed in the first plan step):

| Agent | Headless form | Output |
|---|---|---|
| claude | `claude -p` with stream JSON output | JSONL |
| codex | `codex exec` with JSON output | JSONL |
| tact | `tact run` | JSONL |

Adding a fourth agent type should mean adding one table entry of this kind.

### The `rider` CLI

One binary; the daemon is `rider daemon`. (Call)

```
rider up | down               start or stop the daemon
rider chat ["message"]        talk to the orchestrator; streams its reply
rider status                  tasks by state, running agents, queue depth
rider agents                  running agents: task, type, age, last output
rider logs <task> [-f]        an agent's output
rider kill <task>             stop an agent; its task fails
rider limits [set k=v]        show or change limits
rider inbox                   messages the orchestrator left for you
rider task retry <task> [--agent <type>]   for the orchestrator
rider task abandon <task> "<reason>"       for the orchestrator
rider task note|block|noop|context         for workers
```

Example of what `rider status` shows:

```
board  local  1 running / 4 max     orchestrator: idle     inbox: 1 unread

auth-refactor   /Users/zeke/code/app          base main
  done     T12  claude  extract token store            rider/T12-token-store
  running  T13  codex   migrate callers        14m     rider/T13-callers       <- T12
  waiting  T14  tact    add expiry tests               needs T13              <- T13
  waiting  T15  claude  remove old path                needs T14              <- T14
  blocked  T16  codex   update docs                    "which docs site?"
```

The CLI talks to the daemon over a Unix socket in the rider home
directory, using newline-delimited JSON: request and response, plus
subscriptions for streams. The GUI would use the same socket. (Call)

## Task lifecycle

```
orchestrator creates task (agent, repo, base, needs)
        |
   board: waiting --needs finished--> ready
        |
   daemon: slot free? claim -> worktree + branch -> spawn agent
        |
   agent works; daemon renews lease; agent posts notes
        |
   +-- exit 0, clean tree, and head moved or noop --> complete (last message, branch, head)
   +-- exit 0, dirty tree or nothing done ----------> held: "no committed work"
   +-- non-zero exit / killed / timeout ------------> held: reason + log tail
   +-- `rider task block` ------------------------> held: the agent's question
```

"Head moved" means the branch head differs from where it was when this
attempt started.

**Held tasks.** (Call) All three unhappy endings put the task in the
board's blocked state with a reason and a kind (`failed` or `question`),
rather than the board's failed state, which may be final. The daemon never
retries by itself. Each held task wakes the orchestrator, which either:

- runs `rider task retry <task>`, optionally with `--agent` to switch
  type, which makes the task ready again; or
- runs `rider task abandon <task>`, which fails it on the board for good.

Both are mechanical commands the daemon carries out on request; the
judgment stays with the orchestrator. How "ready again" maps onto Board
operations is settled by the spike.

**Attempts.** Only `failed` endings count. Answering a question does not.
Switching agent type resets the count. Past the limit the daemon refuses
`retry` without a type switch and says so.

A retry reuses the task's worktree and branch as the last agent left
them, uncommitted changes included, and the prompt says so.

## Branches and worktrees

- Worktree: `<rider home>/worktrees/<channel>/<task id>`.
- Branch: `rider/<task id>-<slug>`, cut from the head of the `base`
  task's branch, or from the project's base branch when `base` is absent.
- Completed tasks keep their branch. The worktree is removed on completion
  and recreated if needed.
- A branch moves after children were cut from it only when an `onto` task
  (a fix-up) completes on it. The daemon then notices that the parent head
  is no longer an ancestor of each child and wakes the orchestrator.
- Restacking is the orchestrator's decision and a worker's job: the
  orchestrator creates one `onto` task per child branch, each needing the
  fix-up and the restack of the branch below it. The worker rebases in
  that branch's worktree. If the rebase turns out to be unnecessary it
  reports `noop`.
- A project with its own repo is the same thing with a different `repo` and
  base branch.

## Reaching Zeke when he is not attached

(Call) The orchestrator posts anything meant for Zeke, such as questions
and summaries, to an `inbox` channel on the board, so it survives restarts
like all other state. `rider status` shows the unread count,
`rider inbox` lists and marks read, `rider chat` shows unread messages
first, and the daemon raises a desktop notification for each new one.

## Failure and restart

- **Daemon dies.** Agents it spawned are killed with it (Call: process
  group). Leases lapse within 5 minutes. On start the daemon reads the
  board, finds tasks it had claimed, and re-spawns into the existing
  worktrees, counting an attempt.
- **Orchestrator turn fails.** The wake-ups for that turn go back on the
  queue; after repeated failures the daemon stops waking it and reports the
  fault in `rider status`.
- **Board server dies.** The daemon restarts it and pauses spawning until
  it answers.
- **Agent hangs.** The wall clock limit kills it.

## TigerStyle in practice

- The daemon core is one state machine: `step(state, event) -> actions`,
  with no I/O inside it. Board, git, processes, clock, and socket sit
  behind traits at the edge.
- Every collection has a fixed capacity declared as a named constant:
  running agents, tracked tasks, wake-up queue, socket clients, log bytes.
  Reaching a capacity is a handled case, not a panic or a growth.
- Assertions on every invariant at function entry and exit, kept on in
  release builds. Examples: a running agent always holds a live lease; a
  task is never claimed twice; running count never exceeds the limit.
- Deterministic simulation tests: a seeded run drives the core with fake
  board, git, processes, and clock, injecting agent crashes, lost leases,
  board outages, and daemon restarts, and checks the invariants after every
  step. (Call)
- No async runtime: threads and bounded channels feed the single core
  thread. Dependencies kept to the minimum, each one justified. (Call)
- Explicit limits on every loop and every wait; no unbounded retries.

Default limits (Call): 4 agents at once, 2 per type, 60 minutes per
attempt, 3 attempts per task, 256 tracked tasks, 64 queued wake-ups.

## Risks

- **Agents run unsandboxed with skipped permission prompts.** Unattended
  work needs each agent to run without asking for approval, and tact has no
  sandbox at all. A worktree only separates files by convention; an agent
  can still touch anything Zeke's user can. v1 accepts this. (Call, and
  the one most worth Zeke's attention.)
- **Board is one day old and experimental.** Its CLI was read from docs,
  not run. The first plan step is a spike that installs it, serves a board
  locally, and exercises create with needs, claim, renew, complete, fail,
  block, label, and the event tail, recording the real JSON. The Board
  trait is the only code that should change if the CLI does.
- **Local mode may not match hosted behaviour** for leases and
  dependencies. The same spike checks this.
- **The spike is a gate.** (Call) The design needs dependency-driven
  readiness, claim and renew, an event tail, labels, and a blocked state.
  If local Board lacks any of them, work stops and the findings go back to
  Zeke. The daemon does not quietly emulate missing board features, since
  that would move work state off the board.
- **tact's exit codes are undocumented.** The daemon must not rely on exit
  status alone; hence the committed-work check.

## Left to the builder

- Crate layout, error types, socket message shapes, log format.
- Exact agent flags and prompt wording, within the rules above.
- The orchestrator's instruction file, beyond the wake-up table and label
  scheme.
- How the board server is located and where its data lives, once the spike
  shows what local mode requires.
- The slug rule for branch names.
