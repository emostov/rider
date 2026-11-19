# Rider milestone 1: the daemon

Status: draft for approval, 2026-11-19.

"Decided" marks what Zeke chose in conversation. "Call" marks a choice made
while writing this, open to correction. This document sits under the overall
design, `2026-11-19-rider-design.md`, and replaces its milestones 1 to 3 with
one milestone.

## Intent

Milestone 0 proved the pieces: a pool that runs role-based agents through the
board, and a lead that plans. Three things still need a person. Someone starts
the lead and keeps its session open. Someone watches a terminal to know what is
running. And a crash, a long sleep, or a stuck `vb` call costs up to an hour or
hangs a slot for good.

Milestone 1 is one long-running process, `rider daemon`, that removes all
three. It runs workers, runs the lead one turn at a time, answers a CLI over a
socket, and rebuilds itself from the board and git after a crash.

## Goals

1. `rider up` starts the swarm and `rider down` stops it. (Decided)
2. A ready task with a `role` is claimed within seconds while a slot of its
   role is free, by following the board's event feed, not by polling.
3. The lead runs under the daemon, woken with one message when something needs
   judgment, one turn at a time. (Decided)
4. `rider status`, `agents`, `logs`, `kill` and `chat` work from any terminal.
5. After a crash, the daemon rebuilds from the board and git and resumes or
   re-claims what it held. A restart is not a failed attempt.
6. The core is one pure function, `step(state, event) -> actions`, with every
   collection bounded, tested by a seeded simulator that injects faults.
7. Each build step leaves something usable, and Rider builds itself with the
   daemon from step 2. (Decided)

## Non-goals

- A config file for roles and limits. Room is left for it (see "Room left").
- launchd, or starting at login. `rider daemon` is the form a launch agent
  would run later.
- Keeping the laptop awake. Sleep is handled as a lost claim, not prevented.
- Anything from v2 or the backlog: remote use, other people's agents, tk
  signing, shared memory, a board of our own, a GUI.
- Defences against a hostile worker. Agents are honest and sometimes wrong.

## Anti-goals

- The daemon making a judgment call. It never plans, rewrites, reopens, or
  retries a task on its own.
- State that exists only in the daemon. Everything it keeps on disk is a cache:
  deleting it loses convenience, never work.
- A second way to run the tests. Scenarios use the harness from issue #14.
- Two leads, or two daemons, writing to one board.

## Approaches

Each was put to Zeke with options and a recommendation.

**1. Reaching the board. Decided: the `vb` CLI for everything.** The feed is
a supervised `vb tail --json` child; claims, renewals and settles are `vb`
calls with a time limit. Considered: HTTP for reads with `vb` for writes, and
HTTP for everything. Rejected because the CLI keeps each claim's token in
`~/.config/board/claims/`, one file per board, task and profile. That is what
lets a worker block its own task with `vb`, and what lets a restarted daemon
renew a claim it held. Over HTTP the board issues the token once to the caller,
so the daemon would have to store tokens and workers could no longer block.
HTTP also needs a bearer token whose local storage is undocumented. Everything
stays behind the `Board` trait, so this can change later.

Checked on a scratch board (`@vercel/board` 0.52.0, a second `vb serve` on
another port), since `docs/board-observed.md` did not cover it:

- `vb tail --after <cursor> --json`, with no channel, prints every event on
  the board, one JSON object per line, with the message's channel, labels and
  task state. It resumed from its cursor after the board was restarted.
- Seen in the feed: `task.created`, `labels.changed`, `task.claimed`,
  `task.released`, `task.expired`, `task.blocked`, `task.canceled`,
  `task.completed`. `task.renewed` is hidden, as documented.
- `vb tasks claim <id> --lease <t> --as <profile>` answers `{task, claim:
  {epoch, until}}`. `vb tasks renew <id>` answers `{task, claim}` with the new
  `until`. `vb tasks release <id>` answers outcome `released`, state `ready`.
- Every `vb` call is a new process, so a renewal after a daemon restart is the
  same call as any other renewal.
- A fenced write after the task was canceled, or after the lease ran out, exits
  1 with reason `claim_lost`. After a release, or as another profile, it exits
  1 with reason `claim_not_held`. The daemon treats both as a lost claim.
- `vb tasks <channel>/ --state all --json` carries `needs`, `epoch`, and
  `claim: {actor, until}` for each task.
- A worker with only `BOARD_PROFILE` set blocked a task the daemon's profile
  had claimed.
- In one of two runs the feed child had not printed the last three events when
  it was read some seconds later. The cause was not found. The design therefore
  does not trust the feed alone: see "Reconcile" below.

Step 1 records these in `docs/board-observed.md` with fixtures.

**2. Running the lead. Decided: one `claude -p --resume <session>` per
wake-up.** A turn is a process: it has a start, an end, and a time limit, and
it survives a daemon restart as nothing but a saved session id. Considered: one
long-lived lead process (its state lives in a process that can hang, and "one
turn" has to be inferred), and a fresh session per wake-up (it forgets what
Zeke agreed in conversation).

**3. `rider chat` during a turn. Decided: queue it.** The message goes first
in the next turn. Considered: interrupting the turn (wastes partial turns), and
answering in parallel (two leads on one session).

**4. Running the daemon. Decided: `rider up` starts it in the background;
`rider daemon` is the foreground form.** Considered: foreground only (dies
with the terminal), and launchd (macOS-only, untestable by scenarios, starts
unattended agents at login).

**5. Build order. Decided: workers first, the lead last.** See "Build
order". Considered: the lead first (two processes share the board for a
while), and the whole core before any I/O (nothing usable until late).

**6. Milestone 0 issues. Decided:** see "Issues absorbed".

## Shape

```
 feed reader ----\                         /--> board calls (vb, time-limited)
 timer ----------+--> bounded  --> core --+--> git and worktree work
 socket clients --+    queue      step()   +--> agent processes
 action results --/                         \--> lead turns, socket replies
```

- **One core thread** owns the state and calls `step`. It never blocks on I/O.
- **Sources** (feed reader, timer, socket acceptor, agent waiters) put events
  on one bounded queue.
- **Actions** are run by a bounded set of worker threads. Every action ends in
  exactly one result event, success or failure, so the core never waits on
  something that cannot answer.
- **One daemon serves every project.** It reads the whole board's feed and acts
  on `work/<project>` and `decisions`. (Call)
- **The daemon owns the board process.** If the board does not answer at start,
  the daemon starts it as its child, restarts it if it dies, and stops it on
  exit. A board that was already running is used and left running, and is not
  restarted by the daemon. `rider up` only starts the daemon. (Call)
- Role table and limits are passed into the core as data at startup. (Call)

## The state machine

### State

| Part | Holds | Bound |
|---|---|---|
| `tasks` | every open task the daemon has seen: id, channel, state, role, labels it uses, needs | `TASKS_MAX` |
| `runs` | one per task the daemon is working: phase, profile, epoch, lease end, attempt start time | `AGENTS_MAX` |
| `leads` | one per project with a lead session: phase, pending wake-ups, failures in a row | `PROJECTS_MAX` |
| `clients` | open socket connections and what each subscribes to | `CLIENTS_MAX` |
| `board` | `Up`, or `Down` since a time | one value |
| `stopping` | `No`, `Drain`, or `Now` | one value |
| `cursor`, `now` | last feed cursor handled; last clock reading | one value each |

A run's phase:

```
Claiming -> Preparing -> Running -> Judging -> Settling -> (removed)
               |            |                     ^
               |            +--> Stopping --------+   kill, time limit, down --now
               +--> Settling (blocked: invalid_brief, attempts_exhausted, ...)
any phase --> Abandoning -> (removed)   the claim was lost: stop, write nothing
```

A lead's phase: `Idle`, `Turn` (a process is running), `Backoff` (a turn
failed; retry after a pause), `Halted` (too many failures; `rider chat` or a
restart clears it).

### Events

| Event | From |
|---|---|
| `Tick(now)` | timer, every `TICK_SECONDS` |
| `Feed(event)` | one parsed board event: its type, cursor, task id, and the message's labels and task state after it |
| `FeedLost` / `FeedResumed(cursor)` | the feed child exited / was restarted |
| `Snapshot(tasks, cursor)` | a full listing of open tasks, at start and after `FeedLost` |
| `Claimed(task, epoch, until)` / `ClaimRefused(task)` | claim result |
| `Prepared(task, start)` / `PrepareBlocked(task, kind, reason)` | brief checked, worktree ready |
| `AgentExited(task, exit)` | agent waiter |
| `Judged(task, verdict)` | worktree inspected, `settle::judge` applied |
| `Renewed(task, until)` / `ClaimLost(task)` | renewal, or any fenced write that the board refused with `claim_lost` |
| `Settled(task)` / `SettleFailed(task, tries)` | settle writes done / board error |
| `StackChecked(task, moved)` | which child branches no longer contain the parent's head |
| `BoardFailed` / `BoardAnswered` | a board call timed out or could not connect / one succeeded |
| `LeadExited(project, outcome, session)` | lead turn ended |
| `Client(id, request)` / `ClientGone(id)` | socket |
| `Signal(term)` | SIGTERM or SIGINT: same as `down` |

### Actions

`Claim(task, profile)`, `Prepare(task)`, `Spawn(task, profile)`,
`Renew(task)`, `StopAgent(task)`, `Judge(task)`, `Settle(task, verdict)`,
`Release(task)`, `CheckStack(task)`, `ListTasks`, `RestartFeed(cursor)`,
`RestartBoard`, `RunLead(project, session, message)`, `StopLead(project)`,
`Notify(text)`, `Reply(client, message)`, `SaveCursor(cursor)`, `Exit(code)`.

### Rules the core follows

- **When to claim.** A task is claimable when its board state is `ready`, it
  has a known `role`, no run exists for it, the board is `Up`, `stopping` is
  `No`, and a slot is free in total and for its role. The oldest claimable task
  goes first. Because the lead sets `role` last, `labels.changed` is checked as
  well as `task.ready`, `task.reopened`, `task.unblocked`, `task.released` and
  `task.expired`.
- **Claim by id** with a one-hour lease, as the slot's profile
  (`<role>-<n>-<model>`, as today). A refused claim removes the run; the next
  event or tick tries again.
- **Renew** when a third of the lease has passed. After a clock jump every
  claim is due at once, which is the wanted behaviour.
- **A lost claim** (from a renewal or any fenced write) moves the run to
  `Abandoning`: stop the agent, write nothing to the board, keep the worktree,
  remove the run. The board already decided the task belongs elsewhere.
- **Settle order.** The fenced write (`complete`, `fail`, or `block`) goes
  first. Only after it succeeds: the `attempt` label, the result record or
  failure account, the worktree release. A board error on the fenced write is
  retried up to `SETTLE_TRIES_MAX` with a pause, renewing meanwhile; past that
  the run is abandoned and the lease lapses.
- **`rider kill <task>`** stops the agent and fails the task with "killed by
  the owner". It counts as an attempt.
- **Draining** (`down`): claim nothing new; when no run and no lead turn is
  left, exit. **Now** (`down --now`): stop every agent and the lead, release
  each claim so the task is ready again, exit. Released attempts do not count.
- **Board down.** Any board call that times out or cannot connect sets `Down`.
  While down: no claims, no settles, no lead turns; agents keep running; the
  daemon restarts the board if it started it, and probes on each tick.
- **Feed lost.** Restart the feed from the saved cursor, then take a snapshot,
  so nothing is missed whichever of them is behind.
- **Reconcile.** Every `RECONCILE_SECONDS` the core asks for a snapshot and
  treats any difference from its picture as if the events had arrived. A
  stalled feed costs at most that long.
- **The daemon changes a task's state only through a claim it holds.** To block
  a task it could not prepare, it claims it first. After its own fenced write
  has settled a task, it may still add that attempt's `attempt` label and
  result record to the thread.

### Label rules (absorbs #4 and #20)

`Prepare` reads the task's `needs` and blocks it as `invalid_brief`, naming the
fix, when: `base` is not one of its needs; an `onto` task does not need every
unfinished task on that branch; a `review` or `security` task does not need its
`of` task. A task that sets `restack=1` may leave its starting commit behind;
any other `onto` task must keep it. (Call: the `restack` label.)

### Stack moved

When an `onto` task completes and its branch head changed, `CheckStack` finds
every task whose `base` names a task on that branch and whose own branch does
not contain the new head. The result record says so, and the lead gets a
`stack moved` wake-up.

## Bounds

Every one is a named constant. Values are Calls; the first three exist today.

| Constant | Value | When reached |
|---|---|---|
| `AGENTS_MAX` | 4 | ready tasks wait |
| per-role `slots` | 2, 2, 2, 1, 1 | ready tasks of that role wait |
| `ATTEMPT_SECONDS_MAX` | 55 min | the agent is stopped; the task fails |
| `ATTEMPTS_MAX` | 3 | blocked as `attempts_exhausted` |
| `TASKS_MAX` | 256 | further tasks are not tracked or claimed; `rider status` says how many; they are picked up as others finish |
| `PROJECTS_MAX` | 8 | `rider chat` for a ninth project is refused |
| `WAKEUPS_MAX` | 64 per lead | the oldest wake-up for the same task is dropped; if none, the oldest of any; the next message says "some events were dropped: re-read the channel" |
| `EVENT_QUEUE_MAX` | 1024 | sources block until the core catches up; the feed is not lost because `vb tail` is read only as fast as it is queued |
| `CLIENTS_MAX` | 8 | a new connection is answered `busy` and closed |
| `REQUEST_BYTES_MAX` | 64 KiB | the request is refused and the connection closed |
| `CLIENT_BACKLOG_MAX` | 256 lines | a slow subscriber is dropped with `lagged` |
| `BOARD_CALL_SECONDS_MAX` | 30 | the `vb` child is killed; `BoardFailed` |
| `SETTLE_TRIES_MAX` | 5, 10 s apart | the run is abandoned |
| `FEED_LINE_BYTES_MAX` | 1 MiB | the event is skipped and a snapshot taken |
| `FEED_RESTARTS_MAX` | 10 in a row | the board is treated as down |
| `BOARD_RESTARTS_MAX` | 5 in a row | spawning stays paused; `rider status` says the board is down |
| `LEAD_TURN_SECONDS_MAX` | 15 min | the turn is stopped and counts as failed |
| `LEAD_FAILURES_MAX` | 3 in a row | the lead is `Halted`; its wake-ups are kept |
| `LEAD_BACKOFF_SECONDS` | 60 | fixed pause before a retry |
| `DRAIN_SECONDS_MAX` | `ATTEMPT_SECONDS_MAX` + 5 min | `down` becomes `down --now` |
| `TICK_SECONDS` | 5 | not a limit; the timer's period |
| `RECONCILE_SECONDS` | 60 | not a limit; how often a snapshot is taken |

## Invariants the simulator checks after every step

1. Runs never exceed `AGENTS_MAX`, nor any role its `slots`.
2. At most one run per task, and at most one run per branch.
3. A run in `Running` holds a claim whose lease end is after `now`, or a
   `Renew` or `StopAgent` action for it is outstanding.
4. No board write is emitted for a task after `ClaimLost` for that run.
5. `Spawn` is only ever emitted after `Claimed` and `Prepared` for that run.
6. Every `Settle` follows exactly one `Judged`; no task is settled twice in
   one epoch.
7. At most one `RunLead` is outstanding per project.
8. No collection exceeds its bound.
9. While the board is `Down` or `stopping` is not `No`, no `Claim` is emitted.
10. Every emitted action is answered by exactly one result event (checked by
    the simulator's fakes), and a run never stays in one phase past that
    phase's time limit.
11. Liveness, checked at the end of a run once faults stop: every claimable
    task is claimed or waiting only on a full slot, and every queued wake-up
    has been delivered or its lead is `Halted`.
12. Replaying the same seed gives the same sequence of actions.

## The lead

- **Where it lives.** The lead's instructions are embedded in the binary and
  written to `<home>/lead/` at start, as the worker prompts are. Each project
  has `<home>/leads/<project>.json` holding its session id. (Call)
- **How a project gets a lead.** The first `rider chat <project> "<goal>"`
  starts a session with no `--resume` and saves the id from the stream's first
  event. A project with tasks but no session gets no wake-ups, so a lead run by
  hand still works. (Call: one session per project.)
- **A turn** is `claude -p --resume <id>` with the lead's model, stream JSON
  output, permission prompts skipped, the message on standard input, and the
  output written to `<home>/logs/lead/<project>/<n>.jsonl`. Exit 0 with a
  successful result ends the turn. Anything else puts the wake-ups back and
  counts a failure.
- **The lead no longer waits.** Its instructions lose the `vb wait` step and
  end each turn after creating the next tasks.
- **Decisions** stay in the one `decisions` channel. The lead adds the label
  `project=<name>` to each, so the daemon knows whose it is. A decision
  without the label still raises the notification but wakes no lead, which is
  how a lead run by hand works today. (Call)
- **What does not wake the lead.** An event whose actor is that project's lead
  profile raises no wake-up, so a lead's own writes never start its next turn.
  Tasks in `decisions` raise only `answered`, and do not count as open work
  for `idle`. `idle` is raised once, when the count of open tasks in
  `work/<project>` reaches zero.

### Wake-up kinds

| Kind | Raised when |
|---|---|
| `chat` | `rider chat` sent text |
| `completed` | a task in the project completed |
| `failed` | a task failed |
| `blocked` | a task was blocked, any kind but `awaiting_approval` |
| `answered` | the owner, as a person, replied in the thread of a `decisions` task labelled with the project |
| `stack moved` | `CheckStack` found children behind |
| `idle` | the project has no task that is waiting, ready, claimed or blocked |
| `restarted` | the daemon started and this project has open tasks |

Wake-ups for one project are delivered together. Two of the same kind for the
same task collapse into one. `chat` always goes first.

### Message shape

Pointers, not content: the lead reads the board for the rest. (Call)

```
Wake-up for project rider. Board events 171 to 184.
From Zeke: <chat text>
completed: 147, 152
failed: 149 (no committed work)
blocked: 150 needs_input, 155 invalid_brief
answered: decisions 160
stack moved: 147 moved under 151, 153
idle: nothing is open in work/rider
restarted: the daemon restarted; re-read work/rider before acting
dropped: some events were dropped; re-read work/rider
```

Only the lines that apply are present. The first line is always present.

### Notification

When a task in `decisions` is blocked as `awaiting_approval`, the daemon
raises one desktop notification with the task id and its first line. A failure
to notify is logged and ignored.

## The socket and CLI

`<home>/rider.sock`, mode 0600.  Newline-delimited JSON, one object per line.

```
-> {"v":1,"id":7,"cmd":"status"}
<- {"id":7,"ok":true,"data":{...}}
<- {"id":7,"ok":false,"error":{"code":"unknown_task","message":"..."}}
```

A streaming command answers `ok` once, then lines `{"id":7,"stream":...}`,
then `{"id":7,"end":true}` or an error. A client may send one command at a
time per connection. Unknown fields and an unknown `v` are refused.

| CLI | `cmd` | Answer |
|---|---|---|
| `rider up` | none | starts `rider daemon` detached, waits for the socket, prints the status line |
| `rider down [--now]` | `down` | stream of one line per agent as it ends, then `end` |
| `rider status` | `status` | board state, running count and limit, each lead's phase, open decisions, tasks by project as in the overall design |
| `rider agents` | `agents` | each run: task, role, profile, phase, age, time of last output |
| `rider logs <task> [-f]` | `logs` | the newest attempt's log; with `-f`, a stream. Works without the daemon by reading the file |
| `rider kill <task>` / `rider kill lead <project>` | `kill` | the agent is stopped and the task failed / the turn is stopped |
| `rider chat <project> ["text"]` | `chat` | stream: `queued` if a turn is running, then that turn's text, then the reply's text, then `end`. Without text, reads lines from the terminal, one turn each. Ctrl-C detaches and leaves the lead running |
| `rider daemon` | none | the daemon in the foreground; this is what scenarios run |

Error codes: `busy`, `bad_request`, `unknown_task`, `unknown_project`,
`not_running`, `too_many_projects`, `stopping`, `lagged`.

`rider limits` and `rider open` from the overall design wait for the config
file and are not in this milestone. (Call)

## Files the daemon keeps

| Path under `<home>` | What | If deleted |
|---|---|---|
| `daemon.lock` | pid and start time; taken at start, also by `rider pool` | a second daemon could start |
| `rider.sock` | the control socket | recreated at start |
| `daemon.log` | timestamped lines: every claim, spawn, settle, wake-up | history only |
| `cursor` | last feed cursor handled | the feed starts at the head; the snapshot covers the gap |
| `agents.json` | process group and start time of each running agent | leftover agents are not stopped at restart; their claims lapse |
| `leads/<project>.json` | lead session id | the next chat starts a new session, which re-reads the board |
| `worktrees/<repo>/<id>/` plus a `.rider-board` file beside the git link | the board's identity, taken from where its data lives and when it was created | the worktree is treated as foreign and not reused (absorbs #23) |

## Restart recovery, step by step

1. Take `daemon.lock`. If a live process holds it, say who and exit.
2. Stop leftover agents and lead turns named in `agents.json`, but only where
   the process group still exists and its start time matches. (Call: agents do
   not outlive the daemon; they are stopped, then re-run in the same worktree.)
3. Make sure the board answers; start it if not.
4. Take a snapshot: every open task under `work/` and in `decisions`.
5. For each task in state `claimed` whose holder is one of the daemon's own
   profiles, try to renew it as that profile.
   - Renewed: the daemon holds it. Create a run in `Preparing`. The worktree is
     reused as it was left, the prompt carries the retry note, and `attempt` is
     not changed.
   - `claim_lost` or `claim_not_held`: leave it. It becomes ready when its
     lease lapses and is claimed in the ordinary way.
6. Start the feed from the saved cursor, or from the snapshot's cursor when
   there is none.
7. For each project with a session and open tasks, queue a `restarted`
   wake-up. Wake-ups queued before the crash are not recovered; `restarted`
   tells the lead to re-read.
8. Open the socket. Begin claiming.

Worktrees whose task is no longer open are left alone. Removing them stays
with the completion path and its safety checks.

## Scenarios

Each is a black-box test on the #14 harness: a real board from a temporary
directory, a temporary `RIDER_HOME`, a scratch repository, fake `claude` and
`codex` on `PATH`, and the real `rider` binary. The harness is being built now
(issue #14, on a branch, not yet on `main`), so step 2 depends on it landing.
The 12 pool scenarios listed in #14 must also pass against `rider daemon`,
with "the pool" read as "the daemon"; the ones below are new.

**Running workers**

1. *Claims at once.* Given a daemon and no tasks, when a task with `role` is
   created, then it is claimed within 5 seconds.
2. *Role set last.* Given a ready task without `role`, when `role` is set,
   then it is claimed within 5 seconds.
3. *Per-role limit.* Given 3 ready `polish` tasks on different branches, when
   the daemon runs, then never more than 1 polish agent runs at once.
4. *Renews.* Given an agent that runs longer than a third of a shortened
   lease, then the board's `until` for the task moves forward while it runs.
5. *Time limit.* Given a shortened attempt limit and an agent that never
   exits, then the agent is stopped, the task is failed, and `attempt` is 1.
6. *Lost claim.* Given a running agent, when its task is canceled on the
   board, then the agent is stopped within one renewal, the daemon writes
   nothing more to the thread, and the worktree is kept.
7. *Settle retry.* Given an agent that commits and exits, and a `vb` that
   fails `complete` twice, then the task completes and the agent ran once.
8. *Stuck `vb`.* Given a `vb` that hangs on `claim`, then after the call limit
   the daemon reports the board down in `rider status` and no slot is stuck.
9. *Board restart.* Given a running agent, when the board process is killed,
   then the daemon restarts it, the agent's task still completes, and new
   tasks are claimed afterwards.
10. *One daemon.* Given a running daemon, when a second `rider daemon` or a
    `rider pool` starts on the same home, then it exits non-zero naming the
    holder.

**Label rules and stacks**

11. *Base not needed.* Given a task with `base=A` that does not need A, then it
    is blocked as `invalid_brief`, the reason names `base` and the need, and no
    agent runs.
12. *Onto without needs.* Given an `onto` task that does not need an unfinished
    task on that branch, then it is blocked as `invalid_brief`.
13. *Review without need.* Given a `review` task that does not need its `of`
    task, then it is blocked as `invalid_brief`.
14. *Fix-up drops commits.* Given an `onto` task without `restack=1` whose
    agent rebases away the branch's earlier commits, then it fails. With
    `restack=1` and a rebase onto the named commit, it completes.
15. *Stack moved.* Given task B with `base=A`, both completed, when an `onto=A`
    task commits and completes, then its result record names B as behind.

**The CLI**

16. *Up and down.* When `rider up` runs, it returns with the daemon running in
    the background; `rider status` answers; `rider down` exits 0 and the
    daemon is gone.
17. *Drain.* Given a running agent, when `rider down` runs, then the agent
    finishes, its task completes, nothing new is claimed, and the daemon exits.
18. *Down now.* Given a running agent, when `rider down --now` runs, then the
    agent is stopped, the task is `ready` with `attempt` unchanged, and the
    daemon exits.
19. *Status and agents.* Given one running and one waiting task, then
    `rider status` shows both with their states and `rider agents` shows the
    running one with its role and profile.
20. *Logs.* Given a completed task, then `rider logs <task>` prints what the
    fake agent wrote; given a running one, `rider logs -f` prints lines as
    they are written and ends when the agent exits.
21. *Kill.* Given a running agent, when `rider kill <task>` runs, then the
    agent is stopped and the task is failed with "killed by the owner".

**Restart**

22. *Crash and resume.* Given a running agent, when the daemon is killed with
    SIGKILL and started again within the lease, then the leftover agent is
    stopped, the task is run again in the same worktree under the same claim
    epoch, it completes, and `attempt` is unchanged.
23. *Crash past the lease.* Given the same with a shortened lease that lapses
    before the restart, then the task is claimed afresh and completes.
24. *Board wiped.* Given a leftover worktree from a previous board, when a new
    board reuses its task id, then the task is blocked as `rider_error`
    naming the worktree, and the worktree is untouched.

**The lead** (a fake `claude` plays the lead: it records the message it was
given and runs scripted `vb` commands)

25. *First chat.* When `rider chat demo "<goal>"` runs on a project with no
    session, then the lead is started without `--resume`, its reply is
    printed, and a session id is saved.
26. *Woken on completion.* Given a project with a session, when a task
    completes, then the lead is resumed with that session and a message
    containing `completed: <id>`.
27. *Woken on failure and on block.* Likewise for `failed:` and `blocked:`.
28. *One turn at a time.* Given a lead turn that takes 5 seconds, when three
    tasks complete during it, then exactly one more turn runs, and its message
    lists all three.
29. *Chat queues.* Given a running turn, when `rider chat demo "x"` runs, then
    it prints `queued`, the running turn's text, then the reply to "x"; and
    the turn that carried "x" lists it first.
30. *Answered decision.* Given a blocked task in `decisions` labelled
    `project=demo`, when the owner replies in its thread, then the lead is woken with `answered:`.
31. *Idle.* When the last open task in `work/demo` completes, the message
    contains `idle:`.
32. *Failing lead.* Given a lead that exits non-zero three times, then
    `rider status` shows it halted, no further turns run, and the next
    `rider chat` starts one that carries the kept wake-ups.
33. *Restarted.* Given a project with a session and an open task, when the
    daemon restarts, then the lead's next message contains `restarted:`.
34. *No session, no wake-up.* Given tasks in a project that has no lead
    session, when one completes, then no lead process is started.
35. *The lead does not wake itself.* Given a lead whose turn cancels a task and
    completes a decision, then no further turn runs until something else
    happens.

### Covered only by the simulator, and why

- **Clock jumps** (a sleep longer than the lease): the real clock cannot be
  moved from a test.
- **Orderings of racing events**: a claim lost in the same moment the agent
  exits; `down --now` during `Settling`; a kill during `Claiming`; a feed loss
  between `task.ready` and the claim. A scenario cannot place two real
  processes' events in a chosen order.
- **Bounds that need volume**: `TASKS_MAX`, `WAKEUPS_MAX`, `EVENT_QUEUE_MAX`
  and `CLIENTS_MAX` at their limits, and the drop rule for wake-ups.
- **Long fault sequences**: repeated board outages, restarts in every phase,
  and failures of every action in turn, across thousands of seeded runs.
- **Invariants over time**: 3, 4, 10 and 11 above are statements about every
  step, not about one outcome.

The simulator drives `step` through its events only, with a fake board, git,
process table and clock that it also uses to decide each action's result. A
restart is simulated by discarding the state and rebuilding it from the fakes,
as recovery does.

## Reused, changed, deleted

| Module | In milestone 1 |
|---|---|
| `settle.rs` | reused as is: the verdict table |
| `brief.rs`, `worktree.rs`, `git.rs` | reused; `brief` gains the label rules; `worktree` gains the board identity file |
| `harness.rs`, `roles.rs` | reused; the role table is passed to the core as data |
| `board.rs` | kept behind the `Board` trait; gains time limits, the feed reader, claim by id, renew, release, list, and task views with `needs`, epoch and holder |
| `run_task.rs` | split along its existing seams into the `Prepare`, `Spawn`, `Judge` and `Settle` actions; the settle order changes |
| `pool.rs`, and `pool` and `run-task` in `cli.rs` | deleted in the last step; until then `rider pool` stays as the fallback and takes the daemon's lock |
| `rider serve` | kept; `rider up` uses the same code to start the board |
| `lead/CLAUDE.md` | the `vb wait` step goes; label rules move from "yours to keep" to "Rider checks"; the `project` label on decisions; `restack=1` |

## Issues absorbed (Decided)

Closed or re-scoped with a comment once this design is approved: #4, #5, #21,
#22, #23, #25, #27, #28, and #20 (folded into the label work). #11 and the
`pool.rs` and `cli.rs` parts of #12 wait until the daemon lands, because it
rewrites that code. #19 stays separate. Every other open issue stays a
milestone 0 follow-up; #14 and #15 must land before step 2.

## Build order (Decided)

Each step is one to three issues, and each leaves something usable.

1. **Board plumbing.** Time limits on `vb` calls; the feed reader; richer task
   views. *The pool stops hanging on a stuck `vb`.*
2. **`rider daemon` in the foreground.** The core and the simulator begin
   here, with claim, spawn, renew, settle, limits and the lock file. Because
   Rider builds itself on this step, it also includes: a lost claim stops the
   agent and writes nothing; SIGTERM drains and a second SIGTERM stops now,
   releasing claims; reconcile. A crash at this step costs what it costs
   today: the lease lapses within the hour. Scenarios 1 to 6 and 10; 17 and 18
   are first written here with SIGTERM in place of `rider down`.
   *Replaces `rider pool`; Rider builds itself with it from here.*
3. **Socket and CLI.** `up`, `down`, `status`, `agents`, `logs`, `kill`.
   Scenarios 16 to 21.
4. **Recovery.** Restart resume, settle retries, time limits surfacing as
   board down, board restart, the board identity on worktrees. Scenarios 7 to
   9, 22 to 24.
5. **Label rules and stack-moved detection.** Scenarios 11 to 15.
6. **The lead.** Wake-ups, turns, `rider chat`, the notification, and the
   change to `lead/CLAUDE.md`. Scenarios 25 to 35.
7. **Close out.** Widen the simulator's faults, delete `rider pool`, update
   the README.

## Room left

- **Config file** (wanted by Zeke, later): roles and limits already reach the
  core as data, so a TOML file replaces `roles.rs` as their source and nothing
  in the core changes.
- **HTTP or a board of our own:** the `Board` trait.
- **A GUI:** the socket protocol carries a version.
- **launchd:** `rider daemon` is the foreground form.
- **tk identities:** `Spawn` is the one place a worker's environment is built.

## Left to the builder

- Module layout, error enums, and thread structure inside the shape above.
- Which crates to use. Dependencies are now allowed where they save code;
  `serde` derive for the socket and board shapes and a seeded random number
  crate for the simulator are the expected ones. (Call)
- The exact fields of `status` and `agents` data, and their terminal layout.
- How the lead's stream is turned into chat text.
- How the notification is raised, with text never passed as a bare argument.
- How scenarios shorten the lease and time limits (a test-only setting read
  from the environment is expected).
- The exact content of the board identity file.
- Log line format in `daemon.log`.
