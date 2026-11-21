---
name: ride-swarm
description: Use when the user asks to ride a swarm, start a swarm, have Rider or "the swarm" implement, build, or fix something (a goal, a milestone, GitHub issues), asks for swarm status ("check my swarm", "how is the swarm doing", "what's running"), or asks to stop the swarm.
---

# Ride a swarm

Rider runs a swarm of coding agents on this machine: a local board holds tasks, the `rider up`
daemon runs one agent per ready task in its own git worktree, and a lead plans the goal
into tasks. When the user rides a swarm from this session, **this session is the lead**.

`<skill>` is the directory that contains this SKILL.md. It holds `lead.md`, the lead's
instructions, and `schemas/`. The board is reached with `vb`, which needs:

```sh
export BOARD_URL=http://127.0.0.1:4100/api/board PATH="$HOME/.rider/node/node_modules/.bin:$PATH"
```

## Swarm status

Run these, then answer in the order below. The board changes while you talk, so never
answer from memory.

| Question | Command |
|---|---|
| Is the board up? | `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:4100` (`000` is down) |
| Is the daemon up, and what is open? | `rider status` (board, running count, open tasks by project) |
| Which agents run? | `rider agents` |
| Which projects exist? | `vb channels list work/` |
| A project's tasks, with each result's summary | `vb tasks work/<project> --state all` |
| Why tasks are blocked | `vb tasks work/<project> --state blocked` |
| Questions waiting on the user | `vb tasks decisions --state blocked` ("Couldn't find channel" means none) |
| One task in full | `vb read <id>` |

1. What needs the user: open decisions, and tasks blocked as `rider_error`, `branch_busy`,
   or `attempts_exhausted`.
2. What is running: claimed tasks, and whether the daemon runs, from `rider status`.
   Ready tasks with no daemon are stalled: say so.
3. What finished or failed recently. For a finished `review` or `security` task, say
   whether its summary reports anything blocking, and whether a fix task exists for it.

End the answer with the `Your move:` line that `lead.md` defines under "Telling the owner
where it stands".

Old blocked tasks the user did not mention are theirs to keep or cancel: list them, and ask.

## Ride a swarm

1. **Install what is missing.** Check each with `command -v`; install only what is absent,
   and tell the user what you installed.
   - `rider`: `cargo install --locked --git https://github.com/emostov/rider swarm-rider`
   - `vb` (the Board CLI): `npm install --prefix "${RIDER_HOME:-$HOME/.rider}/node" @vercel/board`
   - `claude`, `codex`, `jq`, Node 22, and Rust cannot be installed for the user: name the
     missing ones and stop. `claude` and `codex` must be signed in.
2. **Agree the shape.** Ask in one question only for what the request and the repository do
   not already say: the project name (channel `work/<project>`), the repository, the base
   branch, the goal and how the user will know it is done, and how finished work reaches the
   base branch. Rider never merges: either the lead merges reviewed work locally
   (`git -C <repo> merge --no-ff <branch>` on a clean checkout, `git merge --abort` and a
   restack task on conflict, never push), or the user merges when asked in `decisions`.
3. **Board.** If it is down, start `rider serve` as a background command and check again.
   `vb schemas list` must list `rider-result@2`, `rider-finding@1` and `rider-github@1`;
   register a missing one with `vb schemas create rider-result@2 --file
   <skill>/schemas/rider-result@2.json` (`rider-finding@1` from `rider-finding.json`,
   `rider-github@1` from `rider-github@1.json`).
4. **Workers.** One daemon serves every project. Check `rider status`; if no daemon runs,
   run `rider up`, which starts it in the background, and check `rider status` again.
5. **Lead.** Read `<skill>/lead.md` in full and follow it for the rest of the session, as
   `lead-fable-5-2`. Answer the user before anything else.

To send a finished branch for the owner's review, open its PR, and get CI green, follow the
`ship-swarm` skill. Its loops follow the owner's settings in `<rider home>/ship.toml`.

## Stop the swarm

Run `rider down`. Running agents finish and their tasks settle; nothing new is claimed.
`rider down --now` stops every agent at once and releases its task, the attempt uncounted.
The board keeps running.

## Common mistakes

- Doing a worker's job as the lead: editing the repository, building, or running tests.
  Make it a task, except the small mechanical edit `lead.md` allows.
- Starting a second daemon. Check `rider status` first.
