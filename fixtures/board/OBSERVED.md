# Board behaviour observed locally

Observed on 2026-11-19 against `@vercel/board` 0.52.0, served locally with `vb serve`. Every statement below is something that was run. Fixtures in `fixtures/board/` are the raw stdout of each `--json` command. Exit codes were recorded from the shell.

Setup for all checks: `BOARD_URL=http://127.0.0.1:4100/api/board`, binary `~/.rider/node/node_modules/.bin/vb`, channel `work/spike`, run from `/tmp` (not a git checkout).

## Install

- `npm install --prefix ~/.rider/node @vercel/board`: exit 0, 27 packages.
- `vb --version` prints `0.52.0`.
- `~/.rider/node/node_modules/.bin/` holds `board`, `vb` and `node-which`. The package's own usage text says `board` is the CLI and `vb` is an alias; both are installed.
- `npx skills add vercel/board --skill board -g`: with stdin from `/dev/null` it ran without prompting and printed "Installed 1 skill" at `~/.agents/skills/board`, symlinked for Claude Code, Hermes Agent and Junie.

## Serve

- Command: `vb serve --dir ~/.rider/board --no-open`, started with `nohup` in the background, pid in `~/.rider/board.pid`, log in `~/.rider/board.log`.
- Printed: `serving a local board at http://127.0.0.1:4100/api/board as local:user:zeke, its log in /Users/zeke/.rider/board  (Ctrl-C to stop)`, then a `reach it with ...` line and `inspector at http://127.0.0.1:4100`. This matches the brief apart from the extra `as local:user:zeke, its log in ...` text.
- Port 4100 was free.
- `--dir` works outside a git checkout: the server was started from `/tmp`, which is not a repository, and it created `~/.rider/board/local.db`, `local.db-shm` and `local.db-wal`.
- `vb whoami --json`: exit 0. Output has `actor`, `principal`, `kind: "person"`, `handle: "zeke"`, `can: {"*": [read, post, react, label, sync, manage]}`, `source: "vb serve"`. (No fixture needed.) Required: passed.

## Plan create (`plan-created.json`, required)

Command: `vb tasks create work/spike --file plan.json --idempotency-key spike-1 --json`. Exit 0.

- Plan: three tasks with keys `a`, `b` (`needs: [{"key":"a"}]`), `c` (`needs: [{"key":"b"}]`). Each has labels `role=implement`, `repo=/tmp/spike-repo`, `base=main`.
- Output is `{"tasks":[ ...full message objects... ]}`. Task IDs were `2`, `4`, `6` (IDs are strings and are not consecutive; replies and events share the same counter). `needs` on `b` is `["2"]`, on `c` is `["4"]`.
- The board adds the label `board.task` (`ready`, `waiting`, `waiting`) to each root.
- A plan file cannot name a sibling's ID in a label: a label value is just a string, and the only cross-reference mechanism is `needs` with `{"key": ...}`. `base` was therefore written literally as `main` in the plan. Later, `vb label 4 base=rider/task-2 --json` worked (exit 0, `{"message":{"id":"4"},"outcome":"labeled"}`), so `base` can be set after creation.
- Second run with the same key and file: exit 0, and it printed the same three tasks (same IDs `2`, `4`, `6`), not an empty list. Recorded on a fresh plan in `work/spike2` (key `spike2-1`, IDs 25, 27, 29): the rerun output is `plan-created-rerun.json`, exit 0, empty stderr, and still three tasks afterwards. A later `tasks --state all` showed still only three tasks. Differs from the brief's "second makes nothing" wording only in that the output repeats the existing tasks rather than being empty.

## Task list (`tasks-list.json`, required)

Command: `vb tasks work/spike --state all --json`. Exit 0. Output: `{"tasks":[{"message":{...},"replies":0,"last":null}, ...],"cursor":"7"}`. `a` (id 2) is `ready`, `b` (4) and `c` (6) are `waiting`. The task state is in `message.task.state` and mirrored in `message.labels["board.task"]`. Note the list shape wraps each task in `message`, unlike the task writes (`claim`, `complete`, ...), which return the message under `task`, and `create --file`, which returns a bare `tasks` array. `after-complete-list.json` is the same command run after `a` completed.

## Claim (`claim.json`, required)

Command: `vb tasks claim --ready work/spike --label role=implement --lease 1h --as opus-1 --json`. Exit 0. Output: `{"task":{...message with task.state "claimed"...},"claim":{"epoch":1,"until":"2026-11-19T20:38:37.720Z"}}`. It returned `a` (id 2). The lease was 1 hour exactly (claimed at 19:38:37, until 20:38:37). `task.claim.actor` is `local:user:zeke+opus-1`. The token is not printed.

## Claim timeout (`claim-timeout.json`, required)

Re-recorded with `vb tasks claim --ready work/spike --label role=nonexistent --lease 1h --as opus-1 --wait 5s --json`. Exit 1. stdout is empty (0 bytes, so `claim-timeout.json` is empty on purpose). The JSON goes to stderr, saved as `claim-timeout.stderr.txt`: `{"status":"error","reason":"timeout","message":"Nothing was ready in work/spike within 5s."}`. Reason `timeout` and exit 1 are as documented. An earlier version of this note said the JSON was on stdout; that was wrong.

## Read (`read-task.json`, required)

Command: `vb read 2 --json`. Exit 0. Output: `{"message":{... "text":"task a","labels":{...},"task":{"state":"claimed",...}},"replies":[],"cursor":"9","schedules":[]}`. Text, labels and `message.task.state` are all present.

## Label (`read-labelled.json`, required)

Commands: `vb label 2 branch=rider/x --as opus-1 --json` (exit 0, `{"message":{"id":"2"},"outcome":"labeled"}`), then `vb read 2 --json`. Exit 0. Labels now include `"branch":"rider/x"`. Labelling as the claim holder (`opus-1`) worked. Separately, `vb label 4 probe=1 --as other-1 --json` (a profile holding no claim, on task `b`) also exit 0 with `outcome: "labeled"`, so labelling does not need the claim. The cursor after this read was `10`.

## Complete (`after-complete.json`, required)

Command: `vb tasks complete 2 "done" --as opus-1 --json`. Exit 0. Output: `{"task":{... "board.task":"completed", task.state "completed", "result":"done"},"outcome":"completed"}`. Listing afterwards showed `2 completed`, `4 ready`, `6 waiting` (`after-complete-list.json`). Completing as the profile that holds the claim worked. As documented.

## Fail and reopen (`after-reopen.json`, required)

- `vb tasks claim 4 --as opus-1 --json`: exit 0.
- `vb tasks fail 4 "why" --as opus-1 --json`: exit 0, `outcome: "failed"`, `task.reason: "why"`, `task.claim: null` (saved as `task-failed.json`). Listing: `2 completed`, `4 failed`, `6 waiting`, so `c` stayed waiting. Repeated on `work/spike2` and saved as `after-fail-list.json`: `25 completed`, `27 failed`, `29 waiting`.
- `vb tasks reopen 4 --json` (as yourself, the owner, no `--as`): exit 0, `outcome: "reopened"`, state `ready`, `epoch` still 1, reason cleared. Saved as `after-reopen.json`. Listing: `4 ready`.

## Block and unblock (`blocked.json`, required)

- `vb tasks claim 4 --as opus-1 --json`: exit 0 (saved as `claim-b.json`).
- `vb tasks block 4 "q?" --kind needs_input --as opus-1 --json`: exit 0. Output has `outcome: "blocked"`, `task.state: "blocked"`, `task.blocked: {"kind":"needs_input","reason":"q?"}`, `task.claim: null` (claim ended), epoch 2. Saved as `blocked.json`.
- `vb tasks unblock 4 --json`: exit 0, `outcome: "unblocked"`, state `ready`. Saved as `unblocked.json`.

## Schema post (`schema-post.json`, optional)

- `vb schemas create rider-result@1 --file <throwaway.json> --json`: exit 0, `outcome: "created"`. Running it again with the same definition: exit 0, no error.
- `vb post 2 --schema rider-result@1 '{"summary":"x","ok":true}' --json`: exit 0. A post to an ID makes a reply. The text is stored as a fenced block ```` ```json#rider-result@1 ````, and the message has `blocks: [{"schema":"rider-result","version":1,"data":{...}}]`. Saved as `schema-post.json`.
- Invalid data (`{"summary":1}`): exit 2, JSON (stream not captured separately) `{"status":"error","reason":"invalid_block",...,"data":{"error":{"code":"invalid_block","message":"the rider-result@1 block doesn't match its schema: ok is missing","status":400}}}`. Not saved as a fixture.
- Note: the argument order `post <id> --schema name@1 '<json>'` from the brief worked as written.

## Wait (`wait.json`, required)

Command: `vb wait work/spike --label 'board.task=completed|blocked|failed' --after 10 --timeout 10s --json`, where `10` was the cursor printed by the earlier `read`, run after `a` was completed. Exit 0. Output is `{"event":{"cursor":"11","type":"task.completed","createdAt":...,"payload":{"id":"2","thread":...,"epoch":1,"result":"done","by":{...}},"message":{...},"actor":{"id":"local:user:zeke+opus-1","kind":"agent","handle":"zeke/opus-1",...}}}`. The `a|b|c` alternative syntax in a label value worked. The event is the completion, which happened after the cursor.

## Reply with attachment (`reply-attach.json`, optional)

Command: `vb reply 2 "x" --attach /tmp/attach.txt --json`. Exit 0. The message has `attachments: [{"id":"...","name":"attach.txt","type":"text/plain","size":6}]`, outcome `posted`.

## Differences that change the design

- A plan file cannot reference a sibling's ID in a label. Set `base` afterwards with `vb label <id> base=<value>` (works), or have the caller supply it another way.
- Output shapes differ by command: list wraps each task as `{message, replies, last}`; `read` returns `{message, replies, cursor, schedules}`; `complete`, `fail`, `block`, `reopen`, `unblock` return `{task, outcome}`; `claim` returns `{task, claim}`; `create --file` returns `{tasks: [...]}`; `label` returns `{message: {id}, outcome}`; `post` and `reply` return `{message, outcome}`; `wait` returns `{event}`. Parsers need one type per shape.
- Idempotent plan re-run returns the existing tasks (exit 0) rather than nothing, so a caller cannot tell "created now" from "already existed" by output alone; compare against what it knew.
- Claim timeout exits 1 with its JSON on stderr and an empty stdout. Invalid schema data exits 2; its JSON was seen in the terminal but stdout and stderr were not captured separately, so which stream it used is unrecorded. Callers should read both streams on non-zero exit.
- Fixtures contain machine-specific values (`/tmp/spike-repo`, the `zeke` handle, `local:user:zeke` actors, timestamps, numeric IDs). Parsers must not treat them as constants.

## Worker and lead commands (task 8, 2026-11-19)

Run against the same board, channel `work/spike8` (tasks 83 and 85), with `decisions/spike8` standing in for `decisions`. Every command below exited 0 unless noted.

- Plan as the lead: `vb tasks create work/spike8 --file plan.json --idempotency-key spike8-1 --as lead-fable-5-2 --json`. A claim by another profile of the same login (`--as implement-1-opus-5-5`) took the lead's task.
- `vb label 85 of=83 --as lead-fable-5-2`, `vb label 83 attempt=0 --as lead-fable-5-2`, `vb tasks needs 85 83 --as lead-fable-5-2`, `vb tasks cancel 85 "<reason>" --as lead-fable-5-2`.
- After a claim made with `--as implement-1-opus-5-5`, a shell with `BOARD_PROFILE=implement-1-opus-5-5` and no `--as` used that claim: `vb read "$BOARD_TASK"`, `printf '%s' "$text" | vb reply "$BOARD_TASK" -`, `vb reply "$BOARD_TASK" -- "-text"`, `vb post "$BOARD_TASK" --schema rider-finding@1 -- '<json>'`, the same post with `-` and a heredoc, and `vb tasks block "$BOARD_TASK" --kind needs_input -- "-question"` (state `blocked`, kind `needs_input`, reason kept with its leading dash).
- `vb reply 83 "-bare dash text"`: exit 2, `Unknown option`. Options after `--` are taken as text: `vb post 83 --schema rider-result@1 -- '<json>' --as x` fails with exit 2, `post takes one text`. Every option goes before `--`.
- A finding with `severity: "minor"`: exit 2, `severity must be "blocking" or "non-blocking"`.
- Lead flow: `vb tasks unblock 83`, `vb tasks reopen 83`, `vb tasks work/spike8 --state blocked --json`, each `--as lead-fable-5-2`. Decision flow: `vb tasks create decisions/spike8 - --as lead-fable-5-2 --json` from stdin, `vb tasks block <id> --kind awaiting_approval --as lead-fable-5-2 -- "waiting for Zeke"` (an unclaimed task, blocked as its owner), then `unblock`, `claim`, and `complete <id> -- "<answer>"` as the lead.
- `vb wait ... --label 'board.task=completed|blocked|failed' --after <cursor>` also matches a reply posted in a thread whose root has one of those states: the lead's own answer in a blocked thread woke the wait (event type `message.posted`). With `--timeout 3s` and nothing new: exit 1, `reason: "timeout"`.
- `vb schemas create rider-finding@1 --file schemas/rider-finding.json`: exit 0, `outcome: "created"`.
- `vb schemas create rider-result@1 --file schemas/rider-result.json`: exit 1, `reason: "schema_immutable"`, "rider-result@1 is registered with another definition, and a version never changes: register a new version". The spike registered `rider-result@1` with a throwaway definition (`summary`, `ok`). Posting a real record against it fails with exit 2, `ok is missing`, so the removed run-task path fell back to a plain reply; `rider-result@2` replaced the record.

## Claimable only with `role` (task 8 fix, 2026-11-19)

Channel `work/spike8f`. Plan (key `spike8f-1`, `--as lead-fable-5-2`, exit 0): task 126 with labels `repo` only, and task 128 with `role=implement`, `onto: "83"` and `needs: ["83"]`, naming a task that already existed. Both were created; 128 came back with `needs: ["83"]` and the `onto` label as written, so a plan file accepts an existing id as a string in a label and in `needs`.

- `vb tasks claim --ready work/spike8f --label role=implement --wait 2s --json --as implement-1-opus-5-5`: exit 0, took 128. After releasing and canceling 128, the same claim: exit 1, `reason: "timeout"`. The ready task 126, which had no `role`, was not taken.
- `vb label 126 role=implement --as lead-fable-5-2`: exit 0. The same claim: exit 0, took 126.
- `vb read <id> --json | jq -r '.message.labels.start // empty'` printed the label's value, or an empty line when the label is absent. Exit 0 both times. `/usr/bin/jq` is jq 1.7.1.

## Plan ids, role-less tasks, and relabel-then-reopen (task 8 fix 2, 2026-11-19)

Channel `work/spike8g`.

- `vb tasks create work/spike8g --file plang.json --idempotency-key spike8g-1 --as lead-fable-5-2 --json`: exit 0. Each returned task has `"key": null`, and the tasks come back in plan order (154, 156, 158, 160 for keys a, b, c, d). Running it again with the same file and key: exit 0, and the same ids come back in the same order.
- `vb tasks work/spike8g --limit 500 --json | jq -r '.tasks[] | select(.message.labels.role == null) | .message.id'` (the default state is open) printed the role-less tasks that were waiting (156) and ready (160). It did not print the canceled 158, or 154, which has `role`. With no open tasks it printed nothing.
- Light retried as implement: claim `--ready --label role=light --as light-1-sonnet-5-5` (exit 0), `fail` (exit 0), `vb label 166 role=implement --as lead-fable-5-2` (exit 0), `vb tasks reopen 166 --as lead-fable-5-2` (exit 0). Then `claim --ready --label role=implement --wait 2s --as implement-1-opus-5-5` took 166 with `role=implement` (exit 0).

## Claim by id, renew, release, listing, lost claims

Recorded on a scratch board, `vb serve --dir <temp> --port 4177 --no-open --no-inspect`, never the board on 4100. Channel `work/scratch`, a two-task plan (`a`, then `b` with `needs: [{"key":"a"}]`; ids 2 and 4) and a single task `c` (id 12), all created as the owner.

- `vb tasks claim 2 --lease 1h --as opus-1 --json`: exit 0, `{task, claim: {epoch, until}}` (`claim-by-id.json`). `claim.epoch` and `claim.until` equal `task.task.epoch` and `task.task.claim.until`; `task.task.claim.actor` is `local:user:zeke+opus-1`.
- `vb tasks renew 12 --lease 1h --as opus-1 --json`: exit 0, the same `{task, claim}` shape with `until` an hour from the renewal (`renew.json`). Without `--lease`, renew set `until` 15 minutes from now, not the claim's own lease, so a renewal always passes `--lease`.
- `vb tasks work/ --state open --json`: exit 0, `{tasks: [{message, replies, last}], cursor}` (`tasks-open.json`). Every `message.task` carries `state`, `epoch`, `needs` (ids as strings) and `claim` (`{actor, until}` when claimed, otherwise null). Across every fixture, `claim` is present exactly when the state is `claimed`.
- A subtree: `work/scratch/` lists `work/scratch` and `work/scratch/deep`; `work/scratch` lists only that channel; `work` (no such channel) prints an empty list, exit 0; `work/nonexistent/` exits 1 with reason `channel_not_found`.
- Paging: `--limit` above 500 exits with reason `invalid_arguments` ("expected number to be <=500"). With `-n 1` the `cursor` is the last task's id and `--after <cursor>` gives the next page; on a page that is not full the `cursor` is the board's event cursor.
- An event cursor before listing: recorded on a scratch board (port 4193, threads in `work/scratch` and a task in `work/scratch/deep`), `vb read work/ --limit 1 --json` exited 0 with `{threads: [one], cursor, older}` (`read-cursor.json`), and its `cursor` (`6`) was the board's latest, though older threads existed. The cursor is board-wide: `vb read work --json` on the same board, with no such channel, printed no threads and the same cursor as the listing.
- `vb tasks release 2 --as opus-1 --json`: exit 0, `{task, outcome: "released"}`, state `ready`, `claim` null, epoch unchanged (`release.json`).
- `vb tasks complete 2 --as opus-1 --json -- done` after the release: exit 1, stdout empty, stderr `reason: "claim_not_held"` (`claim-not-held.stderr.txt`). `vb tasks renew` of a task the profile does not hold gives the same reason.
- Claimed again, then `vb tasks cancel 2 --json -- scratch` as the owner, then the same complete: exit 1, stderr `reason: "claim_lost"`, with `data.error.code` `claim_lost` and status 409 (`claim-lost.stderr.txt`). A renewal of the canceled task afterwards gave `claim_not_held`.

## The event feed: `vb tail --json`

Recorded on a scratch board, `vb serve --dir <temp> --port 4178 --no-open --no-inspect`, never the board on 4100. Channel `work/scratch`, a two-task plan (`a`, id 2; `b`, id 4, needing `a`), worked as the owner and as `--as w-1`. A `vb tail --after 0 --json` with no channel ran throughout.

- One JSON object per line, for the whole board: `{cursor, type, createdAt, payload, message, actor}`. `cursor` is a decimal string, and replies, tasks and events share one counter. `message` is the full message after the event, with `channel`, `labels` and, on a task, `task.state`.
- Fixtures, one raw line each: `tail-task-created.json` (3), `tail-labels-changed.json` (6, `vb label`), `tail-task-claimed.json` (8), `tail-task-released.json` (9), `tail-task-expired.json` (11, a claim with `--lease 30s` left to run out), `tail-task-blocked.json` (13), `tail-message-posted-reply.json` (14, a reply by the owner in the blocked thread), `tail-task-unblocked.json` (15), `tail-task-failed.json` (17), `tail-task-reopened.json` (18), `tail-task-completed.json` (20), `tail-task-ready.json` (21), `tail-task-canceled.json` (22).
- `--lease` takes 30s to 1h; `--lease 2s` exits 2, `invalid_arguments`.
- A plan task appears first as `message.posted` (cursor 2) whose `message` has no `task`, then as `task.created` (3) with `board.task` set.
- A reply's `message` is the reply: `re` holds the root's id, its own `labels` (here `{}`) and no `task`, so the event does not carry the root's state.
- On every task event `payload.id` equals `message.id`. `task.claimed` has `payload.until`; `task.blocked` has `payload.kind` and `reason`; `task.failed` and `task.canceled` have `reason`; `task.completed` has `result`.
- `actor` is `{id, kind, handle, ...}`: `kind` `person` with id `local:user:<login>` for the owner, `agent` with id `local:user:<login>+<profile>` for a profile. `task.expired` and `task.ready` have no `actor` key at all: the board did them.
- `task.ready` (21) is printed when a task's needs are met (task 4, after 2 completed). Its `message` is the task after the change, with `labels` and `task.state` `ready`, and the feed parses it like the other task events.
- `labels.changed` is not task-only. Recorded on another scratch board (port 4191, channel `review/feed`): an ordinary root posted (2), `vb label 2 status=ready` (3, `tail-labels-changed-message.json`), a reply to it (4), `vb label 4 note=seen` (5, `tail-labels-changed-reply.json`). Both events carry the labelled message as `message`, with no `task`; on the reply, `re` holds the root's id. `payload` is `{id, thread, set, unset, by}`.
- `vb tail` stayed running, printing nothing, while its board was stopped. When the board was started again on the same directory and port, both running tails printed `reason: "login_required"` ("invalid token: signature verification failed", status 401) on stderr and exited; the exit code was not recorded. A new `vb tail --after 22 --json` then printed the next event (23, a reply) and kept following.

## Replies in a read (`read-replies.json`)

Commands: on a claimed task `2`, `vb reply 2 --as implement-1-opus-5-5 -- "agent note"`, `vb reply 2 --as lead-fable-5-2 -- "lead decision"`, `vb reply 2 -- "owner decision"`, then `vb read 2 --json`. Each entry of `replies` carries `id` (a decimal string on the same counter as the top-level `cursor`, which was `5` before the replies and `9` after), `text`, and `by: {actor, principal, profile}`, where `profile` is the writing profile and null for the owner. Every profile of one login shares its `principal`, so a profile name proves nothing. The task's `message.task.owner` is the owner's principal and `message.by.actor` is the actor that created the task. Rider delivers to a running agent the replies whose `id` is past the cursor read before the agent started, whose `by.principal` is the owner, and whose `by.actor` is the owner or the task's creator.
