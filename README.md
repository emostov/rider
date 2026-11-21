<h1 align="center">Rider</h1>

<h4 align="center">
    Ride a swarm of coding agents on your own machine.
</h4>

You tell one lead agent what you want, and it plans the work as tasks on a local board.
Rider runs one agent per task, each in its own git worktree, with a model picked for the job.

## Feature highlights

- **Ride a swarm to a goal.** Say `ride a swarm to <goal>` in a coding-agent session. The
  goal can be one feature, many features at once, a milestone, or a list of GitHub issues.
  The `ride-swarm` skill makes the session the lead, and the lead starts the daemon with
  `rider up`.
- **Parallel agents with hard limits.** Rider caps the total number of agents, the number of
  building agents at once, and the slots of each role. Set them under `[limits]` and each
  role in `rider.toml`; `rider config` prints the values in effect.
- **A role for each job.** `implement` (Opus) and `light` (Sonnet) write code. `review` runs
  `codex exec` and `native-review` runs Codex's built-in `codex review`. `security` runs on
  Mythos, and `qa` tests the result. Roles are configurable: change any role's harness,
  model, prompt and slots, or add roles of your own, under `[roles.<name>]` in `rider.toml`.
- **Reviews at the end of each PR.** The lead runs a `review` and a `native-review` of the
  whole diff in parallel, then one fix task for their blocking findings. A Mythos `security`
  review of the whole PR comes last.
- **One rider.toml for every repository.** `<rider home>/rider.toml` holds the roles and
  limits. A `[repo."<absolute path>".roles.<role>]` table sets the skills a role runs for
  that repository's tasks only. `rider config --repo <path>` prints what those tasks see.
- **Ship finished work as PRs.** Ask the lead to ship the stack. The `ship-swarm` skill opens
  pull requests, as drafts if you choose, gets CI green and answers review threads. The lead
  merges a PR only when its CI is green and its review is done; Rider pushes only when the
  lead runs `rider pr`, and never merges. The first time, it asks how you like its loops to
  run and saves the answers in `<rider home>/ship.toml`.
- **Skills and prompts built into the binary.** The skills and the roles' prompts ship inside
  `rider`, so they always match it. `rider skills install --into ~/.claude/skills` (Claude
  Code) or `--into ~/.codex/skills` (Codex) writes the skills where your agent reads them. Rider
  updates those installs when a newer `rider up`, `rider daemon` or `rider skills install` runs.
- **Coming: an owner review queue.** Milestone 5 adds `rider queue`, a queue for triaging
  reviews of other people's PRs and following up on feedback on your own PRs. It is not on
  `main` yet.

## Getting started

Paste this into your coding agent:

```
Install Rider: clone https://github.com/emostov/rider if you don't have it yet, then run
`cargo install --locked --path <that clone>`, which installs the `rider` binary. Then run
`rider skills install --into <your agent's skills directory>`, such as `~/.claude/skills`
for Claude Code or `~/.codex/skills` for Codex.
```

`rider skills list` and `rider skills show <name>` print the built-in skills. Then say
`ride a swarm to <goal>` in a new session.

## Quickstart

The `ride-swarm` skill needs Rust, Node 22, `jq`, and both `claude` and `codex` on `PATH`,
each signed in. It stops and names any that is missing.

Open a coding-agent session in a terminal you keep open and say `ride a swarm to <goal>`.
The lead starts the daemon with `rider up`, plans the goal into tasks on the board, and
waits on the board with `rider wait`.

- Say "check my swarm" for status.
- Say "stop the swarm" to stop it. The lead runs `rider down`.

To look yourself:

```sh
rider status        # the board, the running count, and every open task by project
rider agents        # every agent the daemon runs
rider logs <id>     # the newest attempt's log of a task; -f follows it
rider pr <id>       # push a completed task's branch and open its draft PR
```

The board's web inspector is at <http://127.0.0.1:4100>.

## How it's built

- **A deterministic simulator.** The daemon's decisions are one pure function,
  `step(state, event) -> actions`, with no I/O. A seeded simulator (`tests/sim/`) drives it
  through thousands of runs with a fake board, agents and clock, injecting crashes, lost
  claims, board outages and clock jumps, and checks the design's invariants after every
  step. A failing seed replays exactly: `SIM_SEED=<n> cargo test --test sim`.
- **Elements of TigerStyle.** At least two assertions per function, every loop and
  collection bounded by a named constant, no recursion, no `unsafe`, no async runtime:
  threads, bounded channels and back pressure. See [AGENTS.md](./AGENTS.md).
- **Black-box scenario tests.** `tests/scenarios/` runs the real `rider` binary against a
  real board in a scratch directory, with fake agents, so behaviour is pinned from the
  outside and the code can be rewritten with confidence.
- **Built by its own swarm.** Rider's changes are planned on the board and carried out by
  Rider's own agents, each reviewed by a different model than the one that wrote it.
