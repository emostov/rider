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

## Troubleshooting

The `vb` commands below reach the local board with `BOARD_URL`
(`http://127.0.0.1:4100/api/board` by default); the Board CLI is under
`<rider home>/node/node_modules/.bin`.

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
  for this board".** Each worktree records the board it was made for, in a `.rider-board`
  file in its git directory. The board's identity is the oldest post in its `rider/identity`
  channel, read once when the daemon starts, so restart the daemon after replacing the board
  and leave that channel alone. Keep what you need from the named directory, remove it with
  `git worktree remove <dir>` from the repository, then `vb tasks unblock <id>`.
- **Many `node ... vb` processes are left running.** `rider reap` lists Board CLI processes
  whose board no longer answers; `rider reap --yes` stops them.

## Releasing

Bump `version` in `Cargo.toml`, merge it to `main`, then tag the merge commit `v<version>`.
[RELEASING.md](RELEASING.md) has the steps.

## Status

Rider is early and still developing basic features. The built-in roles and their models are
in `src/roles.rs`; `rider config` prints them with your `rider.toml` over them. A role's
`prompt` may name a file of your own instead of a built-in prompt. When the daemon starts, it
registers Rider's board schemas (`rider-result@2`, `rider-finding@1`) on a board that lacks
them. A `prompts/` directory an older Rider wrote under `<rider home>` is no longer read.
