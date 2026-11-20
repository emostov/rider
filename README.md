<h1 align="center">Rider</h1>

<h4 align="center">
    Ride a swarm of coding agents on your own machine.
</h4>

You tell one lead agent what you want. The lead splits the goal into tasks on a
local board. Rider runs one agent per task, each in its own git worktree, with
a model picked for the job.

## How it works

**The lead** is your coding-agent session, guided by the `ride-swarm` skill. It plans the work, answers
the workers' questions, checks their results, and keeps you updated. It never
writes code itself. Questions only you can answer wait in the `decisions`
channel while the rest of the work goes on.

**The swarm** runs as many agents at once as the task graph allows, up to 32:

| Role        | Model           | Job                                         |
| ----------- | --------------- | ------------------------------------------- |
| `implement` | Opus 5.5        | Builds a task and commits it                |
| `light`     | Sonnet 5.5      | Builds a task whose brief says exactly what to change |
| `review`    | Sol 6.1 (Codex) | Reviews another task's work                 |
| `polish`    | Sonnet 5.5      | Tidies a branch without changing behaviour  |
| `security`  | Mythos 5.2      | Reviews the finished stack                  |
| `qa`        | Opus 5.5        | Optional: uses another task's work like a person would |

Every implementing task is reviewed by a different model than the one that
built it. A task is done only when its agent exited cleanly, its worktree is
clean, and its branch has new commits.

Each task's branch is cut from the task it builds on, so the result can become
one pull request or a stack.

## Getting started

Paste this into your coding agent:

```
Install Rider: run `cargo install swarm-rider`, which installs the `rider` binary.
Then run `npx skills add emostov/rider --skill ride-swarm --skill ship-swarm -g`.
```

Then say "ride a swarm to <goal>" in a new session. The first time `ship-swarm` opens a
pull request, it asks how you like its loops to run (a review packet first, comments, CI,
review threads) and saves the answers in `~/.rider/ship.toml`.

## Status

Rider is early. `rider daemon` runs the swarm from the board's event feed; `rider up`,
`rider status` and `rider chat` are next. Until then, the `ride-swarm` skill asks you to
run `rider pool <project>` in a terminal of your own.

## Docs

The [manual](./docs/manual.md) covers prerequisites, roles, labels, watching
and stopping the swarm, where files live, and troubleshooting.
The [release guide](./docs/releasing.md) covers tagging and publishing.

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
