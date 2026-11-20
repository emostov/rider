<h1 align="center">Rider</h1>

<h4 align="center">
    Ride a swarm of coding agents on your own machine.
</h4>

You tell one lead agent what you want. The lead splits the goal into tasks on a
local board. Rider runs one agent per task, each in its own git worktree, with
a model picked for the job.

## How it works

**The lead** is a Claude Code session on Fable 5.2. It plans the work, answers
the workers' questions, checks their results, and keeps you updated. It never
writes code itself. Questions only you can answer wait in the `decisions`
channel while the rest of the work goes on.

**The swarm** runs up to four agents at once:

| Role        | Model           | Job                                         |
| ----------- | --------------- | ------------------------------------------- |
| `implement` | Opus 5.5        | Builds a task and commits it                |
| `light`     | Sonnet 5.5      | Builds a task whose brief says exactly what to change |
| `review`    | Sol 6.1 (Codex) | Reviews another task's work                 |
| `polish`    | Sonnet 5.5      | Tidies a branch without changing behaviour  |
| `security`  | Mythos 5.2      | Reviews the finished stack                  |

Every implementing task is reviewed by a different model than the one that
built it. A task is done only when its agent exited cleanly, its worktree is
clean, and its branch has new commits.

Each task's branch is cut from the task it builds on, so the result can become
one pull request or a stack.

## Getting started

Clone this repository, open your agent in it, and paste:

```
Set up Rider from this checkout by following docs/manual.md: check the
prerequisites, install the Board CLI and its skill, run `cargo install --path .`,
start the board with `rider serve` in the background, and register the two
schemas in schemas/. Then tell me the commands to start the worker pool and the
lead in my own terminals. Do not start any agents yourself.
```

To install by hand instead, you need Rust, Node 22, `jq`, and the `claude` and
`codex` CLIs, each signed in:

```bash
npm install --prefix ~/.rider/node @vercel/board
npx skills add vercel/board --skill board -g
cargo install --path .
```

Then register the schemas (see the [manual](./docs/manual.md)) and start three
terminals:

```bash
rider serve              # the board
rider pool demo          # the swarm, for project "demo"
cd lead && claude        # the lead
```

Tell the lead what you want:

```
You are the lead for project `demo`. The repository is /abs/path/to/repo, base
branch `main`. Goal: <what you want built, and how you will know it is done>.
Plan it with a review of each implementing task, a polish pass, and one
security review of the stack. I have agreed the goal and the channel
`work/demo`; decide everything else yourself. Workers are already running.
```

Watch the swarm in the board's inspector at <http://127.0.0.1:4100>.

## Safety

- **Agents run unsandboxed, with permission prompts skipped.** They can read
  and change anything your user can. Only point Rider at repositories you are
  prepared to have changed.
- **Nothing leaves your machine.** Rider never pushes, opens a pull request, or
  merges. Work stays on local branches named `rider/<task id>-<slug>`.

## Status

Rider is early. Next up is a daemon: `rider up` to start everything,
`rider chat` to talk to the lead, `rider status` to see the swarm, and recovery
after a crash.

## Docs

The [manual](./docs/manual.md) covers prerequisites, roles, labels, watching
and stopping the swarm, where files live, and troubleshooting.
The [release guide](./docs/releasing.md) covers tagging and publishing.
