# Rider manual

The full reference for running Rider today. For the short introduction, see the [README](../README.md).

Rider runs a swarm of coding agents on your own machine through a local Vercel Board. You talk
to one lead agent, which plans a goal into board tasks with dependencies and a role for each.
`rider pool` claims each ready task, starts the agent its role calls for in a git worktree of
its own, and settles the task on the board when the agent exits.

## Status

This is milestone 0. What works:

- `rider serve` runs the local board.
- `rider pool <project>` runs worker slots on the channel `work/<project>`. Each slot claims a
  ready task by its `role` label, makes a worktree and a branch, runs one agent, and then
  completes, fails, or blocks the task. Up to 32 agents run at once, as the board allows.
- The `ride-swarm` skill (`skills/ride-swarm/`), which holds the lead's instructions
  (`lead.md`) and the result and finding schemas; and the worker prompts (`prompts/`).

What does not exist yet: the daemon, `rider up`, `rider chat`, `rider status`, `rider agents`,
`rider logs`, and `rider kill`. The lead is a Claude Code session that you start yourself.
Nothing notices when a branch moves under a stacked child branch; the lead has to restack it.
The first end-to-end run, and what went wrong in it, is in
[milestone-0-run.md](milestone-0-run.md).

## Safety

Agents run unsandboxed, with permission prompts skipped. Unattended work requires it. A
worktree separates files only by convention: an agent can read and change anything your user
can. Point Rider only at repositories you are prepared to have changed.

Rider never pushes, opens pull requests, or merges. Work stays on local branches named
`rider/<task id>-<slug>`.

## Prerequisites

- Rust (stable), to build `rider`.
- Node 22, for the Board CLI.
- `claude` (Claude Code) and `codex` (Codex CLI) on `PATH`, each signed in. The roles use the
  models in the table below; each must be available to your account.
- The Board CLI, installed where Rider looks for it, and its skill:

  ```sh
  npm install --prefix ~/.rider/node @vercel/board
  npx skills add vercel/board --skill board -g
  ```

- `jq`, which the lead uses to read board output.

Build and install Rider from this checkout:

```sh
cargo install --path .
```

## Quickstart

Use three terminals.

**Terminal 1: the board.**

```sh
rider serve
```

`rider serve` holds this terminal. The first time, register the two schemas from this
checkout in another terminal:

```sh
export BOARD_URL=http://127.0.0.1:4100/api/board PATH="$HOME/.rider/node/node_modules/.bin:$PATH"
vb schemas create rider-result@1 --file skills/ride-swarm/schemas/rider-result.json
vb schemas create rider-finding@1 --file skills/ride-swarm/schemas/rider-finding.json
```

The `vb` commands under "Watching" need the same two `export` settings.

**Terminal 2: the workers.** `demo` is the project name; tasks go in the channel `work/demo`.

```sh
rider pool demo
```

**Terminal 3: the lead.**

```sh
cd lead && claude
```

The lead runs on Fable 5.2 (set in `lead/.claude/settings.json`) and follows
`skills/ride-swarm/lead.md`. Tell it the project, the repository, the base branch, and the goal, and say
what it may decide alone. For example:

> You are the lead for project `demo`. The repository is `/abs/path/to/repo`, base branch
> `main`. Goal: add a `wordcount` shell script that prints the number of words on standard
> input, with a `test.sh` that proves it; then, stacked on that, add a `--lines` flag. Plan
> it with a review of each implementing task, a polish pass, and one security review of the
> stack. I have agreed the goal and the channel `work/demo`; decide everything else yourself.
> Workers are already running.

The lead writes tasks to the board and waits on it. Questions it cannot answer alone go in
the `decisions` channel, or to you in its session.

## Roles

The `role` label on a task picks the agent. A task is claimable only once it has `role`.

| `role` label | Model | Harness | Shown as | Commits |
|---|---|---|---|---|
| `implement` | Opus 5.5 | Claude Code | Implement <n> (Opus 5.5) | yes |
| `light` | Sonnet 5.5 | Claude Code | Light <n> (Sonnet 5.5) | yes |
| `review` | Sol 6.1 | Codex | Review <n> (Sol 6.1) | no |
| `polish` | Sonnet 5.5 | Claude Code | Polish <n> (Sonnet 5.5) | yes |
| `security` | Mythos 5.2 | Claude Code | Security <n> (Mythos 5.2) | no |
| `qa` | Opus 5.5 | Claude Code | QA <n> (Opus 5.5) | no |

The lead is Fable 5.2 in Claude Code, with the board profile `lead-fable-5-2`. Each worker's
board profile is `<role>-<n>-<model>`, such as `implement-1-opus-5-5`. This is the built-in
table; `rider.toml` (below) may change it.

## Configuration

`rider daemon` and `rider pool` read `$RIDER_HOME/rider.toml` when they start. Without the
file they use the built-in roles and limits. `rider config` prints the effective
configuration, built-in plus file, in the file's own form, so you can start a file from it.

A role in the file overrides a built-in role field by field, or adds a new role (at most 16
roles in all). A new role sets every field but `needs_of`, `builds` and `slots`. `prompt` is a built-in
prompt name (`implement`, `review`, `polish`, `security`, `qa`) or a path to your own prompt
file, relative to `$RIDER_HOME` or absolute. `needs_of` makes a task of the role name the task
it checks in an `of` label; it defaults to `true` for a role that does not commit and must be
`true` for one. `slots` caps how many agents of the role run at once; unset, only
`limits.agents` does in `rider daemon`, and `rider pool` keeps its built-in slot counts.

`builds` marks a role whose agents compile and run tests: `true` for `implement`, `light`,
`polish` and `qa`, `false` for `review` and `security`, and `true` for a new role unless it
says otherwise. `limits.builders_at_once` caps how many agents of building roles run at once,
on top of `limits.agents` and each role's `slots`, in both `rider daemon` and `rider pool`. A
ready task of a building role waits while that cap is full, just as it waits for a free slot.
With the example below, at most 4 agents build at once while up to 8 run in all, so reviews
keep running while builds wait.

```toml
[limits]
agents = 8           # agents at once, 1 to 32 (default 32)
builders_at_once = 4 # agents of building roles at once, 1 to 32 (default 4)
attempt_minutes = 30 # one attempt's time limit, 1 to 55 (default 55)
attempts = 2         # failed attempts before a task is blocked, 1 to 9 (default 3)

[roles.review]
model = "gpt-6.1-sol-mini"

[roles.polish]
slots = 1

[roles.security]
builds = true        # this role's agents build and test too

[roles.docs]
harness = "claude"   # claude or codex
model = "claude-sonnet-5-5"
title = "Docs"
model_name = "Sonnet 5.5"
model_slug = "sonnet-5-5"
prompt = "prompts/docs.md"
commits = true
```

An unknown or malformed field, an unknown harness, a missing prompt file, or a limit out of
range stops `rider daemon` and `rider pool` with an error naming the field and the file. The
file is read once; restart to apply a change.

## Labels

| Label | Set by | Value |
|---|---|---|
| `role` | lead, required, set last | `implement`, `light`, `review`, `polish`, `security`, or `qa` |
| `repo` | lead, required | absolute path of the target git repository |
| `base` | lead | id of the task whose branch this one is cut from |
| `from` | lead, required when `base` and `onto` are absent | the agreed base branch to cut from |
| `onto` | lead | id of a task whose existing branch this task works on directly |
| `of` | lead, required for `review`, `security`, and `qa` | id of the task under review |
| `resume` | lead | `1` on a fix task for a blocking finding whose original task is `implement` or `light`: the worker continues that task's Claude session |
| `branch` | Rider | the branch the task works on |
| `attempt` | Rider | failed attempts so far |
| `start` | Rider | the commit the task's branch was at when the task first started |

The lead sets `from` to the base branch you agreed on every task that has neither `base` nor
`onto`. A task without any of the three is cut from `origin/HEAD`, and is blocked as
`invalid_brief` in a repository that has none. The full rules, including what Rider checks
before it starts a worker, are in `skills/ride-swarm/lead.md`.

## Watching

- The board's web inspector: <http://127.0.0.1:4100>.
- Every task of a project, in any state:

  ```sh
  vb tasks work/demo --state all
  ```

- One task's brief, labels, and thread, including who claimed and replied:

  ```sh
  vb read <id>
  ```

- The pool terminal prints one line when a slot claims a task and one when it settles it.

## Stopping

- In the pool terminal, type `stop` and press Enter, or press Ctrl-D. Each slot finishes the
  task it is running and claims no more; the pool exits when they have all finished.
- The pool stops when its standard input closes, so run it in a terminal. Started with its
  standard input closed (from a script, or with `< /dev/null`), it stops at once.
- Ctrl-C kills `rider` at once. Agents already running are left to finish on their own and
  their tasks are not settled. Each claim is a one-hour lease, so those tasks return to ready
  within the hour.

## Where things live

| Path | What |
|---|---|
| `~/.rider/board` | the local board's database |
| `~/.rider/worktrees/<repository directory name>/<task id>` | a task's worktree, removed when the task completes; its branch stays |
| `~/.rider/logs/<task id>/<n>.jsonl` | the agent's output stream for attempt `n` |
| `~/.rider/prompts` | the worker prompts, overwritten from the binary each time `rider pool` starts |
| `~/.rider/node` | the Board CLI |

Set `RIDER_HOME` to use a directory other than `~/.rider`.

## Troubleshooting

These come from the first end-to-end run (see `docs/milestone-0-run.md`).

- **`vb tasks work/<project>` says "Couldn't find channel".** The channel exists only after
  its first task. This is normal before the lead has planned anything.
- **A ready task sits unclaimed for minutes while slots are free.** In the run, idle slots
  held every place at the gate while they polled, and ready tasks waited up to nine and a half
  minutes. It is a delay, not a deadlock. A fix that claims without waiting was committed after
  the run and has not yet been through an end-to-end run; to pick it up, reinstall with
  `cargo install --path .` and restart the pool.
- **Claude Code prints "Ignoring 2 permissions.allow entries ... this workspace has not been
  trusted".** Run `claude` once interactively in `lead/` and accept the trust dialog.
- **A failed task's log tail is cut short.** Rider keeps its reply within the board's message
  limit, so the thread holds only the end of the log. The reply names the whole log:
  `~/.rider/logs/<task id>/<attempt>.jsonl`.
- **A reopened task repeats its mistake after you replied a correction.** The lead's and your
  replies in the task's thread amend its brief, and the latest wins, so make the reply a
  complete, specific instruction. If the correction changes what the task is, change the
  brief itself, then reopen:

  ```sh
  printf '%s\n' "<first line>" "" "<corrected brief>" | vb edit <id> - --as lead-fable-5-2
  vb tasks reopen <id> --as lead-fable-5-2
  ```

- **A task is blocked as `invalid_brief`.** The reason names the label to fix. Fix it with
  `vb label`, then `vb tasks unblock <id> --as lead-fable-5-2`.
- **A Codex log starts with "Reading additional input from stdin...".** This is harmless.

## More

- Design: [superpowers/specs/2026-11-19-rider-design.md](superpowers/specs/2026-11-19-rider-design.md)
- How Board and each agent CLI behave: [board-observed.md](board-observed.md),
  [harness-observed.md](harness-observed.md)
- Rules for changing Rider: [AGENTS.md](../AGENTS.md)
