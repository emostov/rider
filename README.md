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

Clone this repository, open your agent in it, and paste:

```
Set up Rider from this checkout by following docs/manual.md: check the
prerequisites, install the Board CLI and its skill, run `cargo install --path .`,
start the board with `rider serve` in the background, and register the two
schemas in schemas/. Then tell me the commands to start the worker pool and the
lead in my own terminals. Do not start any agents yourself.
```

Then tell the lead what you want:

```
You are the lead for project `demo`. The repository is /abs/path/to/repo, base
branch `main`. Goal: <what you want built, and how you will know it is done>.
Plan it with a review of each implementing task, a polish pass, and one
security review of the stack. I have agreed the goal and the channel
`work/demo`; decide everything else yourself. Workers are already running.
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

Using Rider:

- [Manual](./docs/manual.md): prerequisites, roles, labels, stopping, where things live, troubleshooting
- [The first run](./docs/milestone-0-run.md): one goal from prompt to stacked branches, with times, costs, and what went wrong

Design:

- [Rider design](./docs/superpowers/specs/2026-11-19-rider-design.md): the parts, the roles, and the task lifecycle
- [Daemon design](./docs/superpowers/specs/2026-11-19-rider-daemon-design.md): the next milestone, with its scenarios

Foundations:

- [Board, as observed](./docs/board-observed.md): how the local Vercel Board behaves
- [Agent CLIs, as observed](./docs/harness-observed.md): how each harness runs headless
- [Rules for changing Rider](./AGENTS.md): TigerStyle, tests, and how work is planned
