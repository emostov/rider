<h1 align="center">Rider</h1>

<h4 align="center">
    Ride a swarm of coding agents on your own machine.
</h4>

You tell one lead agent what you want. The lead splits the goal into tasks on a
local board. Rider runs one agent per task, each in its own git worktree, with
a model picked for the job.

Fully configurable per repo so you can have sane workflows across both your most
ambitious personal projects and more cautious production work.

## How it works

**The lead** is your coding-agent session, guided by the `ride-swarm` skill. It plans the work, answers
the workers' questions, checks their results, and keeps you updated. It never
writes code itself. Questions only you can answer wait in the `decisions`
channel while the rest of the work goes on.

**The swarm** is the `rider up` daemon. It runs agents in roles you configure in
`~/.rider/rider.toml`. Each role sets its agents' model and prompt and how many replicas run
at once. `rider config` prints the settings in effect: the built-in roles and limits with the
file's over them. Rider spawns agents through `claude` (Claude Code), `codex` (Codex CLI) or
`tact`, chosen per role with `harness` in `rider.toml`; each must be installed and signed in
for the roles that use it.

## Getting started

Paste this into your coding agent:

```
Install Rider: clone https://github.com/emostov/rider if you don't have it yet, then run
`cargo install --locked --path <that clone>`, which installs the `rider` binary. Then run
`npx skills add emostov/rider --skill ride-swarm --skill ship-swarm -g`.
```

Then say "ride a swarm to <goal>" in a new session. The first time `ship-swarm` opens a
pull request, it asks how you like its loops to run (a review packet first, comments, CI,
review threads) and saves the answers in `~/.rider/ship.toml`.

## Quickstart

Rider needs Rust, Node 22, and `claude`, `codex` or `tact` on `PATH`, signed in for the roles
that use them. Install the Board CLI where Rider looks for it:

```sh
npm install --prefix ~/.rider/node @vercel/board
```

1. Start the daemon. It starts the local board if none is running, then claims ready tasks on
   every `work/<project>` channel:

   ```sh
   rider up
   ```

2. Start the lead: open a coding-agent session in a terminal you keep open and say "ride a
   swarm to <goal>". The `ride-swarm` skill makes that session the lead. It plans the goal
   into tasks on the board and watches the board with `vb wait`. Say "check my swarm" for
   status and "stop the swarm" to stop it.

3. Watch the work:

   ```sh
   rider status        # the board, the running count, and every open task by project
   rider agents        # every agent the daemon runs
   rider logs <id>     # the newest attempt's log of a task; -f follows it
   ```

   The board's web inspector is at <http://127.0.0.1:4100>.

4. Stop: `rider down`.

Agents run unsandboxed, with permission prompts skipped. Point Rider only at repositories you
are prepared to have changed. Rider never pushes or merges; work stays on local branches named
`rider/<task id>-<slug>`.

## Stopping

- `rider down` drains: running agents finish and their tasks settle, and nothing new is
  claimed.
- `rider down --now` stops every agent at once and releases its task, the attempt uncounted.
- `rider kill <id>` stops one task's agent and fails the task; the attempt counts.
- The board keeps running after the daemon stops.

## Where things live

Everything is under `~/.rider`, or `$RIDER_HOME` when it is set.

| Path | What |
|---|---|
| `rider.toml` | roles and limits; optional, read when the daemon starts |
| `ship.toml` | how `ship-swarm` runs its loops, written the first time it asks |
| `agents.json` | each running agent's process group, so a restarted daemon can find it |
| `cursor` | the last board event the daemon handled, so a restart resumes from it |
| `board.json` | the board process the daemon started, kept across a restart |
| `board/` | the local board's database |
| `logs/<task id>/` | each attempt's agent output |
| `worktrees/<repository>/<task id>` | a task's worktree, removed when it completes |
| `daemon.log`, `board.log` | what the daemon and the board it started printed |
| `rider.sock` | the daemon's control socket |

## Troubleshooting

- **`rider status` says the daemon is not running.** Run `rider up`. If it does not start,
  `daemon.log` says why: an error in `rider.toml` names the field and the file.
- **`rider daemon` exits saying `daemon.lock` is held.** Another daemon runs on the same
  home, and the error names its pid. One daemon serves every project, so use that one.
- **`vb tasks work/<project>` says "Couldn't find channel".** The channel exists only after
  its first task.
- **A failed task's log reply is cut short.** The reply keeps within the board's message
  limit and names the whole log; `rider logs <id>` prints it.
- **A task is blocked as `invalid_brief`.** The reason names the label to fix. Fix it with
  `vb label`, then `vb tasks unblock <id>`.
- **Many `node ... vb` processes are left running.** `rider reap` lists Board CLI processes
  whose board no longer answers; `rider reap --yes` stops them.

## Releasing

Bump `version` in `Cargo.toml`, refresh `Cargo.lock` with
`cargo update -p swarm-rider --offline`, and merge it to `main` once CI passes. Then tag the
merge commit `v<version>` and push the tag; the release workflow checks the tag against the
manifest, runs CI again, and creates the GitHub release. Publishing to crates.io is manual.

## Status

Rider is early and still developing basic features.

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
