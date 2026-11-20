# Ship settings

The owner's choices for the loops in `SKILL.md`: the owner's review in a packet, what to do
with comments, how to watch CI, and what to do with PR review threads. They live in one TOML
file, `<rider home>/ship.toml` (`~/.rider/ship.toml` unless `RIDER_HOME` is set), kept per
repository, so they carry over between sessions.

## Reading the file

The file holds one `[repo."<owner>/<name>"]` table per repository, with sub-tables such as
`[repo."<owner>/<name>".ci]`. Top-level keys are allowed as the owner's own defaults; the lead
never writes them.

Read the file at the start of every ship and find this repository's table. A key that table
leaves out takes the top-level value, then the default in the Keys table:

```toml
[ci]
poll_seconds = 30

[repo."acme/api".ci]
poll_seconds = 60

[repo."acme/api".pr]
draft = true
reviewers = ["octocat"]

[repo."acme/api".threads]
actionable = "task"
```

Here `acme/api` polls CI every 60 seconds; any other repository with a table polls every 30.

A key or a value this file does not list is an error: tell the owner which key and which
file, and stop. Do not guess what it meant.

## The first ship from a repository: ask

When the file has no table for this repository, including when the file does not exist, ask
the owner before step 1. Ask in one round, with the
`AskUserQuestion` tool when the harness has it, otherwise as a short numbered list. Offer the
default first in each question and say it is the default. Ask only these:

1. **Owner review before the PR.** Push each settled branch as a review packet and wait for
   the owner, or open the PR straight away? If a packet: which tool (`reviews` is the only one
   supported today), and what counts as approval.
2. **Comments in the packet.** For an actionable comment: make a board task at once, or ask
   first? Reply in the packet after the fix, or leave replies to the owner?
3. **CI.** How often to poll, the longest to wait, and how many reruns a flake gets.
4. **PR review threads.** For an actionable thread: make a board task at once, or ask first?
   Reply and resolve after the fix is pushed and CI is green, or ask first? Who to request as
   reviewers, whether to request Copilot, and whether to mark the PR ready on your own when
   it is done.
5. **Pings.** Where to ping (this session, or a push notification when the harness has
   one), and whether to hold routine updates for a digest.

Write the answers into this repository's table only, never into the top level or another
repository's table. Show the owner the table, and go on. Never ask again for a repository that
has a table. The owner changes a choice by editing the file or by asking you to.

## Keys

Keys are named relative to the repository's table (`ci.poll_seconds` is
`[repo."<owner>/<name>".ci]`, `poll_seconds`).

| Key | Values | Default | Used in |
|---|---|---|---|
| `packet.enabled` | `true`, `false` | `false` | step 0 |
| `packet.tool` | `"reviews"` | `"reviews"` | step 0 |
| `packet.approval` | `"decisions"`, `"owner_says"` | `"owner_says"` | step 0 |
| `packet.poll_seconds` | 30 to 600 | `60` | step 0 |
| `packet.cap_minutes` | 10 to 480 | `120` | step 0 |
| `comments.actionable` | `"task"`, `"ask"` | `"task"` | step 0 |
| `comments.reply` | `"after_fix"`, `"never"` | `"after_fix"` | step 0 |
| `ci.poll_seconds` | 10 to 300 | `30` | step 2 |
| `ci.cap_minutes` | 0, or 5 to 240 | `0` | step 2 |
| `ci.flake_reruns` | 0 to 2 | `1` | step 3 |
| `pr.draft` | `true`, `false` | `true` | step 1 |
| `pr.reviewers` | GitHub logins | `[]` | step 1 |
| `pr.ready` | `"auto"`, `"ask"` | `"ask"` | step 5 |
| `threads.actionable` | `"task"`, `"ask"` | `"task"` | step 4 |
| `threads.reply` | `"after_green"`, `"ask"` | `"after_green"` | step 4 |
| `pr.copilot` | `true`, `false` | `false` | step 1 |
| `notify.channel` | `"session"`, `"push"` | `"session"` | Pinging the owner |
| `notify.digest_minutes` | 0, or 15 to 240 | `0` | Pinging the owner |
| `sensitive.reviewers` | 2 to 4 | `2` | step 0 |

- `packet.approval = "decisions"` reads section decisions from the packet tool, and needs a
  tool that returns them. `"owner_says"` waits for the owner to say so in this session.
- `ci.cap_minutes = 0` keeps the rule in step 2: the longest the workflows normally run,
  doubled.
- `notify.digest_minutes = 0` reports routine updates at the end of the turn that made them.
  A number holds them and sends one digest at that interval. Urgent pings never wait.
- `sensitive.reviewers` is how many independent reviewers judge a sensitive branch before
  anything about it is published. The gate itself cannot be turned off.
- A `"task"` value still follows `lead.md`: the task is a fix task, and the lead makes no
  code change beyond the small mechanical edit `lead.md` allows.

The rules in `SKILL.md` are not settings. No setting lets the lead merge, push to the base
branch, force-push a branch the swarm did not create, or leave a review on a PR.

## Example

```toml
[repo."acme/api".packet]
enabled = true
tool = "reviews"
approval = "owner_says"
poll_seconds = 60
cap_minutes = 120

[repo."acme/api".comments]
actionable = "task"
reply = "after_fix"

[repo."acme/api".ci]
poll_seconds = 30
cap_minutes = 0
flake_reruns = 1

[repo."acme/api".pr]
draft = true
reviewers = ["octocat"]
copilot = true
ready = "ask"

[repo."acme/api".threads]
actionable = "task"
reply = "after_green"

[repo."acme/api".notify]
channel = "push"
digest_minutes = 60

[repo."acme/api".sensitive]
reviewers = 2
```
