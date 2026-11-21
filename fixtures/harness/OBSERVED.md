# Observed headless harness behavior

Observed on 2026-11-19 with Claude Code 2.1.328, codex-cli 0.170.0.
Prompt used for every run: `Create a file named ok.txt containing OK, commit it, then reply with the single word DONE.`
Each run used its own fresh git repo (one empty initial commit) as working directory.
All runs: exit 0, `ok.txt` committed, final message `DONE`.

## Claude Code

```
claude -p --model <MODEL> --output-format stream-json --verbose --dangerously-skip-permissions "<prompt>"
```

- Run with cwd set to the worktree. There is no working-directory flag in use.
- Models that worked (each exit 0, ok.txt committed, result `DONE`):
  - `claude-opus-5-5`
  - `claude-sonnet-5-5`
  - `claude-fable-5-2`
  - `claude-mythos-5-1` (id taken from `model:` in `~/.claude/agents/adversarial-reviewer.md`)
  - `claude-mythos-5-2` was also confirmed to answer headless (exit 0, `stop_reason: end_turn`). It is the id the security role will use, by the owner's choice. `claude-mythos-5-1` is the id in the existing agent definition.
- Exit code on success: 0. Exit code on SIGTERM: 143 (128+15), process dies at once.
- Stream: one JSON object per line. Event types seen: `system` (subtypes `hook_started`, `hook_response`, `init`), `assistant`, `user`, `rate_limit_event`, `result`.
- Final message: the last line, `type == "result"`, `subtype == "success"`, field `result` (string, here `"DONE"`). `is_error` is `false` on success. The same text is also in the last `assistant` event at `message.content[].text`.
- stderr prints `Warning: no stdin data received in 3s...` when stdin is not redirected. Spawn with stdin set to null (`< /dev/null`) to avoid the 3 s delay.
- Fixture: `claude.jsonl` (opus run, 8.6 KB after redaction). The other three models give the same event shapes.
- Session id: every event carries `session_id`, the first `system` event (`hook_started`, or `init` without hooks) included.
- Resume (2026-11-19, Claude Code 2.1.329, `--model haiku`): `claude -p --resume <id> --output-format stream-json --verbose -- "<prompt>"` run from a different directory than the session started in continues the session: it recalled the earlier reply. It still works after the original directory is removed. The `init` event shows the new cwd and the same `session_id`. So a fix task resumes from its own worktree; no same-path worktree is needed.
- Fork (2026-11-19, Claude Code 2.1.329, `--model haiku`): `claude --help` lists `--fork-session`: "When resuming, create a new session ID instead of reusing the original (use with --resume or --continue)". `claude -p --resume <id> --fork-session ...` recalled the earlier reply, and its `init` and `result` events carried a new `session_id`. A later plain `--resume <id>` still reported `<id>`. Without the flag, Claude appends the resumed run to `<id>` in place, so a failed fix would become part of what later resumes see. Rider always forks, and records the fork's id for the fix task's own attempt.
- Resume of a missing session (same version): `claude -p --resume 00000000-0000-4000-8000-000000000000 --output-format stream-json --verbose -- hi` exits 1 at once. stderr says `No conversation found with session ID: <id>`. stdout has a single event: `{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"session_id":"<new id>","errors":["No conversation found ..."],...}`. There is no `system` init event and no `assistant` event. Rider reads an error `result` with `num_turns: 0` and no turn events as a resume that failed before any work, and starts fresh in the same attempt.

## Codex

```
codex exec --model gpt-6.1-sol --json --dangerously-bypass-approvals-and-sandbox -C <DIR> "<prompt>"
```

- Flags match the brief. `--skip-git-repo-check` also exists but was not needed.
- Model `gpt-6.1-sol` worked (provider `valet` from `~/.codex/config.toml`).
- Exit code on success: 0. Exit code on SIGTERM: 143.
- stderr prints `Reading additional input from stdin...`. Spawn with stdin set to null.
- Items with `item.type == "error"` carry `item.message`, not `item.text`.
- Stream events (`type`): `thread.started`, `turn.started`, `item.started`, `item.completed`, `turn.completed`.
- The stream begins with two `item.completed` events of `item.type == "error"` (a deprecation warning from this machine's config). These are not failures. A parser must not treat `item.type == "error"` as a run failure.
- Final message: the last `item.completed` event whose `item.type == "agent_message"`, field `item.text` (here `"DONE"`). Earlier `agent_message` items are progress commentary. `turn.completed` follows and carries only `usage`.
- Fixture: `codex.jsonl` (4 KB).
- Codex loads skills from `~/.codex/skills`, as Claude Code does from `~/.claude/skills`. Not re-observed yet.
- During a provider outage Codex prints top-level events such as
  `{"type":"error","message":"Reconnecting... 2/5 (unexpected status 503 Service Unavailable ...)"}`
  mid-turn, numbered up to `5/5`, and carries on. A turn that recovers still ends with
  `turn.completed`, so a run is judged by its last turn event: `turn.completed` succeeds;
  `turn.failed`, or a top-level `error` after which no `turn.completed` comes (Codex gave up), fails.
  Fixture: `codex-reconnect.jsonl`, cut from a real review run, with the provider
  URL and `cf-ray` id redacted.

- Resume (2026-11-20, codex-cli 0.170.0, `gpt-6.1-sol`): a fresh `codex exec ... -C <A>` run
  was told to remember a word. Then, with `<A>` removed and the working directory set to
  another repository `<B>`:
  `codex exec resume --model <MODEL> --json --dangerously-bypass-approvals-and-sandbox -- <thread_id> "<prompt>"`
  exits 0 and recalls the word. The subcommand has no `-C`; its shell commands ran in `<B>`
  (`pwd` printed `<B>`), so Rider sets the child's working directory. The stream has the
  same shape as a fresh run and its `thread.started` carries the same `thread_id`: Codex
  continues the session in place, with no fork. `codex exec fork` exists but Rider does not
  use it. Fixture: `codex-resume.jsonl`.
- Resume of a missing session (same version):
  `codex exec resume ... -- 00000000-0000-4000-8000-000000000000 "<prompt>"` exits 1 at once.
  stdout is empty, with no `thread.started`; stderr is one line,
  `Error: thread/resume: thread/resume failed: no rollout found for thread id <id> (code -32600)`.
  Rider reads a failed run whose log has no `thread.started` as a refused resume and starts
  fresh in the same attempt. Fixture: `codex-resume-refused.log` (the stderr; stdout was empty).

## tact

Rider no longer supports tact; its observations and fixture were removed.

## SIGTERM test

Prompt: `Count slowly to 500, one number per message, running `sleep 2` between each.` SIGTERM sent after 12 s, stdin from `/dev/null`.

| Harness | Exit status | Note |
| --- | --- | --- |
| claude | 143 | no `result` event written |
| codex | 143 | stream stops mid-turn |

## Fixture redaction

The raw streams held private material, so the committed fixtures were scrubbed. No credentials were found (bearer tokens, `sk-`, `ghp_`, JWT prefixes, authorization headers). What was removed:

- claude: in the `init` event, `tools`, `mcp_servers`, `slash_commands`, `agents`, `skills`, `plugins`, `memory_paths` and `additional_directories` are set to `REDACTED`. The `hook_response` `output` and `stdout` fields that held text are set to `REDACTED` because the session hook injected instruction text. `stderr` was already empty and is unchanged.
- All fixtures: the home directory was replaced with `/Users/user/` (also in the `-Users-...-` path segment of scratch paths), and the organisation name is replaced with `REDACTED`.

Every line is still valid JSON, and the final-message locations above are intact.
