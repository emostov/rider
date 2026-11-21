<h1 align="center">Rider</h1>

<h4 align="center">
    Ride a swarm of coding agents on your own machine.
</h4>

You tell one lead agent what you want, and it plans the work as tasks on a local board.
Rider runs one agent per task, each in its own git worktree, with a model picked for the job.

## Feature highlights

- **Ship many features at once.** Say `ride a swarm to <goal>`: a feature, a milestone or a
  list of GitHub issues. One lead plans it; agents build it in parallel.
- **The right model for each job.** Opus builds, Sonnet takes small edits, Codex reviews and
  Mythos checks security. Change any role or add your own.
- **Every PR reviewed by other models.** Two Codex reviews, one fix, then a Mythos security
  pass, before anything merges.
- **From goal to merged PR.** Rider opens draft PRs, gets CI green and answers review threads.
- **Your review queue, triaged.** `rider queue` lines up other people's PRs to review and the
  feedback waiting on yours, with agents preparing each one.
- **One config for every repository.** Per-repository skills and settings in one
  `rider.toml`, for side projects and careful production work alike.
- **Safe to leave running.** Hard limits on agents, a worktree per task, and recovery from
  crashes and board outages.

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
each signed in. It stops and names any that is missing. Rider installs the Board CLI it
needs under its home the first time `rider up`, `rider serve` or `rider vb` runs, and
`rider vb <args>` runs it on the local board.

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
