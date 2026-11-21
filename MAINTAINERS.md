# Maintaining Rider

This file is for people who work on Rider itself. [AGENTS.md](AGENTS.md) holds the rules
for changing the code.

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

The home, its logs and the socket are readable only by their owner. Every rider command that
creates the home refuses one that other users can write to, that sits under a path another
user can change, or that carries a macOS ACL.

Agents run as the same user as the daemon, so nothing can reliably stop them from stopping it.
As a guard against accidents, the daemon refuses `down` and `kill` from a process that descends
from it, or whose process group's leader does, which covers an agent running `rider down` or
`rider kill` itself. It does not stop a process that has left both (a child in its own process
group whose parent has exited), nor an agent that signals the daemon directly. Keeping agents
from stopping the daemon needs a separate user or a sandbox.

## Agent sandbox

On macOS agents run under a best-effort `sandbox-exec` sandbox that blocks the obvious ways to
reach Rider's socket, control files and programs. It is defence in depth, not a security
boundary: a determined agent running as the owner's user can still escape it. Linux agents are
not confined.

## Troubleshooting

`rider vb` runs the Board CLI under `<rider home>/node/node_modules/.bin` on the local board
(`BOARD_URL`, `http://127.0.0.1:4100/api/board` by default).

- **`rider status` says the daemon is not running.** Run `rider up`. If it does not start,
  `daemon.log` says why: an error in `rider.toml` names the field and the file.
- **`rider daemon` exits saying `daemon.lock` is held.** Another daemon runs on the same
  home, and the error names its pid. One daemon serves every project, so use that one.
- **On macOS `daemon.log` says `agents run unconfined: the daemon runs in a sandbox`.** The
  daemon was started inside a sandbox (as in a Rider agent running Rider's own scenarios), and
  macOS does not let a sandboxed process confine an agent again, so its agents run without
  Rider's confinement. Start `rider up` from a terminal that is not sandboxed to confine them.
- **`rider vb tasks work/<project>` says "Couldn't find channel".** The channel exists only after
  its first task.
- **A failed task's log reply is cut short.** The reply keeps within the board's message
  limit and names the whole log; `rider logs <id>` prints it.
- **A task is blocked as `invalid_brief`.** The reason names the label to fix. Fix it with
  `rider vb label`, then `rider vb tasks unblock <id>`.
- **A task is blocked as `rider_error`: "Rider will not reuse the worktree ... was not made
  for this board".** Each worktree records the board it was made for, in a `.rider-board`
  file in its git directory. The board's identity is the oldest post in its `rider/identity`
  channel, read once when the daemon starts, so restart the daemon after replacing the board
  and leave that channel alone. Keep what you need from the named directory, remove it with
  `git worktree remove <dir>` from the repository, then `rider vb tasks unblock <id>`.
- **Many `node ... vb` processes are left running.** `rider reap` lists Board CLI processes
  whose board no longer answers; `rider reap --yes` stops them.

## Releasing

Bump `version` in `Cargo.toml`, merge it to `main`, then tag the merge commit `v<version>`.
[RELEASING.md](RELEASING.md) has the steps.

## Status

Rider is early and still developing basic features. The built-in roles and their models are
in `src/roles.rs`; `rider config` prints them with your `rider.toml` over them. A role's
`prompt` may name a file of your own instead of a built-in prompt. When the daemon starts, it
registers Rider's board schemas (`rider-result@3`, `rider-finding@1`) on a board that lacks
them. A `prompts/` directory an older Rider wrote under `<rider home>` is no longer read.
