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

**The swarm** runs up to four agents at once:

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

Then say "ride a swarm to <goal>" in a new session.

## Safety

- **Agents run unsandboxed, with permission prompts skipped.** They can read
  and change anything your user can. Only point Rider at repositories you are
  prepared to have changed.
- **The Rider binary never pushes, opens a pull request, or merges.** Work stays on
  local branches named `rider/<task id>-<slug>`. The `ship-swarm` skill pushes branches
  and opens pull requests only when you ask it to.

## Status

Rider is early. It runs a swarm from the board and the skills; a daemon (`rider up`,
`rider chat`, `rider status`) is next.

## Docs

The [manual](./docs/manual.md) covers prerequisites, roles, labels, watching
and stopping the swarm, where files live, and troubleshooting.
The [release guide](./docs/releasing.md) covers tagging and publishing.
