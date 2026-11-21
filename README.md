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
`<rider home>/rider.toml`. Each role sets its agents' model and prompt and how many replicas run
at once. `rider config` prints the settings in effect: the built-in roles and limits with the
file's over them. Rider spawns agents through `claude` (Claude Code) or `codex` (Codex CLI),
chosen per role with `harness` in `rider.toml`; each must be installed and signed in
for the roles that use it. A role may list `skills = ["<name>", ...]`; its
worker runs each one, in order, over the task's whole PR range and fixes what it reports
before completing. A `[repo."<absolute path>".roles.<role>]` table sets only that role's
`skills`, for that repository's tasks only, and `rider config --repo <path>` prints the table
they see.

## Getting started

Paste this into your coding agent:

```
Install Rider: clone https://github.com/emostov/rider if you don't have it yet, then run
`cargo install --locked --path <that clone>`, which installs the `rider` binary. Then run
`npx skills add emostov/rider --skill ride-swarm --skill ship-swarm -g`.
```

Then say "ride a swarm to <goal>" in a new session. The first time `ship-swarm` opens a
pull request, it asks how you like its loops to run (a review packet first, comments, CI,
review threads) and saves the answers in `<rider home>/ship.toml`.

## Quickstart

Rider needs Rust, Node 22, and `claude` or `codex` on `PATH`, signed in for the roles
that use them. Install the Board CLI where Rider looks for it:

```sh
npm install --prefix "${RIDER_HOME:-$HOME/.rider}/node" @vercel/board
```

Open a coding-agent session in a terminal you keep open and say "ride a swarm to <goal>".
The `ride-swarm` skill makes that session the lead. The lead starts the daemon with
`rider up`, plans the goal into tasks on the board, and waits on the board with `rider wait`.

- Say "check my swarm" for status.
- Say "stop the swarm" to stop it. The lead runs `rider down`.

### For the curious

Watching is optional. The lead keeps you updated, but you can look yourself:

```sh
rider status        # the board, the running count, and every open task by project
rider agents        # every agent the daemon runs
rider logs <id>     # the newest attempt's log of a task; -f follows it
```

The board's web inspector is at <http://127.0.0.1:4100>.

Agents run unsandboxed, with permission prompts skipped. Point Rider only at repositories you
are prepared to have changed. Rider never pushes or merges; work stays on local branches named
`rider/<task id>-<slug>`.

## Stopping

The lead stops the swarm when you ask. To stop it by hand:

- `rider down` drains: running agents finish and their tasks settle, and nothing new is
  claimed.
- `rider down --now` stops every agent at once and releases its task, the attempt uncounted.
- `rider kill <id>` stops one task's agent and fails the task; the attempt counts.
- The board keeps running after the daemon stops.

## Where things live

Everything is under `<rider home>`: `~/.rider`, or `$RIDER_HOME` when it is set.

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

The home, its logs and the socket are readable only by their owner. The daemon refuses a home
that other users can write to, that sits under a directory another user can change, or that
carries a macOS ACL. It refuses `down` and `kill` from any process that descends from it, or
whose process group's leader does, so its agents cannot stop it or kill a task.

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
- **A task is blocked as `rider_error`: "Rider will not reuse the worktree ... was not made
  for this board".** Worktrees are named by task id, so after the board's data is replaced a
  new task can reuse the id of an old task whose worktree is still there. Each worktree Rider
  makes records which board it is for in a `.rider-board` file in the worktree's git
  directory. The board's identity is the oldest post in the board's own `rider/identity`
  channel, a random id Rider posts there when the channel is empty, so wiping, replacing, or
  switching the board gives it a new one. The daemon reads it once when it starts: restart it
  after replacing the board, and do not post in or delete `rider/identity`. A worktree with no
  record, or with another board's, is left as it is. Keep what you need from the directory
  the reason names, remove it with `git worktree remove <dir>` from the repository, then
  `vb tasks unblock <id>`.
- **Many `node ... vb` processes are left running.** `rider reap` lists Board CLI processes
  whose board no longer answers; `rider reap --yes` stops them.

## Releasing

Bump `version` in `Cargo.toml`, refresh `Cargo.lock` with
`cargo update -p swarm-rider --offline`, and merge it to `main` once CI passes. Then tag the
merge commit `v<version>` and push the tag; the release workflow checks the tag against the
manifest, runs CI again, and creates the GitHub release. Publishing to crates.io is manual. See [RELEASING.md](RELEASING.md).

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
