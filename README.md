<h1 align="center">Rider</h1>

<h4 align="center">
    Ride a swarm of coding agents on your own machine.
</h4>

Rider is for agent swarms. A rider is a swarm rider: the person commanding the
swarm. You tell one lead agent what you want. It plans the goal into tasks on a
local board, and Rider runs an agent for each task in a git worktree of its
own, with the model that suits the work.

- **One conversation**: you talk to the lead. It plans, answers the workers'
  questions, checks their results, and tells you where things stand.
- **The right agent for each task**: every task has a role, and each role has
  its own model. A different model reviews what another one built.
- **Stacked branches**: each task's work lands on its own local branch, cut
  from the task it builds on, ready to become one pull request or a stack.

## The lead

The lead is a Claude Code session on Fable 5.2. It never does the work itself:

- **Plans**: it turns your goal into tasks, each with a role, the paths it
  owns, what it needs first, and the command that proves it is done.
- **Checks**: every implementing task gets a review. Only a blocking finding
  becomes a fix task.
- **Asks**: what only you can decide waits in the `decisions` channel while
  the rest of the work carries on.

## The swarm

One agent per task, at most four at once:

- **`implement`** (Opus 5.5): builds a task and commits it.
- **`light`** (Sonnet 5.5): builds a task whose brief already says exactly
  what to change.
- **`review`** (Sol 6.1, in Codex): attacks another task's work and reports
  findings.
- **`polish`** (Sonnet 5.5): tidies a branch without changing behaviour.
- **`security`** (Mythos 5.2): reviews a finished stack.

A finished task is checked, not trusted. Rider completes a task only when the
agent exited cleanly, the worktree is clean, and the branch has commits of its
own.

## Getting started

Install the `ride-swarm` skill:

```bash
npx skills add emostov/rider --skill ride-swarm -g
```

Then open Claude Code in any repository and say what you want:

```
Ride a swarm to add a --json flag to the export command, with tests.
```

That session becomes the lead. It installs `rider` and the Board CLI if they
are missing, starts the board, asks you to run `rider pool <project>` in a
terminal of your own, agrees the goal with you, and plans it. Later, from any
session:

```
Check my swarm.
```

Watch the swarm work in the board's inspector at <http://127.0.0.1:4100>.

## Manual installation

Rider needs Rust, Node 22, `jq`, and the `claude` and `codex` CLIs, each
signed in.

```bash
npm install --prefix ~/.rider/node @vercel/board
npx skills add vercel/board --skill board -g
cargo install --path .
```

Then use three terminals:

```bash
rider serve              # the board
rider pool demo          # the swarm, for project "demo"
cd lead && claude        # the lead
```

The [manual](./docs/manual.md) has the one-time schema registration and the
rest of the setup.

## Safety

- **Agents run unsandboxed, with permission prompts skipped**: unattended work
  requires it. An agent can read and change anything your user can. Point
  Rider only at repositories you are prepared to have changed.
- **Nothing leaves your machine**: Rider never pushes, opens a pull request,
  or merges. Work stays on local branches named `rider/<task id>-<slug>`.

## Status

Rider is early. Today the swarm is `rider pool` in a terminal and the lead is a
session you start yourself. Next is a daemon: `rider up` to start everything,
`rider chat` to talk to the lead from any terminal, `rider status` to see the
swarm, and recovery after a crash or a long sleep.

## Docs

Getting set up:

- [Prerequisites](./docs/manual.md#prerequisites): what to install before Rider
- [Quickstart](./docs/manual.md#quickstart): the board, the swarm, and the lead, step by step

Running a swarm:

- [Roles](./docs/manual.md#roles): which model does which kind of task
- [Labels](./docs/manual.md#labels): how a task says what it is and which branch it builds on
- [Watching](./docs/manual.md#watching): the inspector and the commands that show progress
- [Stopping](./docs/manual.md#stopping): how to stop the swarm and what happens to running agents
- [Shipping](./skills/ship-swarm/SKILL.md): the lead pushes a finished stack, opens its PR, and gets CI green

When something is off:

- [Where things live](./docs/manual.md#where-things-live): the board, worktrees, logs, and prompts on disk
- [Troubleshooting](./docs/manual.md#troubleshooting): the problems seen so far and how to get past them
