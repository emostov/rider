# Observed headless harness behavior

Observed on 2026-11-19 with Claude Code 2.1.328, codex-cli 0.170.0, tact 0.8.3.
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
- Fixture: `fixtures/harness/claude.jsonl` (opus run, 8.6 KB after redaction). The other three models give the same event shapes.
- Session id: every event carries `session_id`, the first `system` event (`hook_started`, or `init` without hooks) included.
- Resume (2026-11-19, Claude Code 2.1.329, `--model haiku`): `claude -p --resume <id> --output-format stream-json --verbose -- "<prompt>"` run from a different directory than the session started in continues the session: it recalled the earlier reply. It still works after the original directory is removed. The `init` event shows the new cwd and the same `session_id`. So a fix task resumes from its own worktree; no same-path worktree is needed.

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
- Fixture: `fixtures/harness/codex.jsonl` (4 KB).

## tact

```
tact --model sol --workspace <DIR> run "<prompt>"
```

- Model `sol` worked and reports `model: "gpt-6.1-sol"` in `run.completed`. The full id `--model gpt-6.1-sol` also worked (exit 0, ok.txt committed).
- `tact --help` lists no model names. `--model <MODEL>` is a free string (env `TACT_MODEL`). Other relevant flags: `--workspace`, `--thinking <low|medium|high|xhigh|max>`, `--reasoning-mode <standard|pro>`, `--auth <auto|chatgpt|api-key>`.
- tact has no permission-skip flag and none was needed.
- Exit code on success: 0. Exit code on SIGTERM: 0 (see below).
- Stream: JSON objects, one per line, envelope `{protocol_version, request_id, seq, type, payload}`. Types seen: `run.started`, `input.accepted`, `model.call.started`, `model.attempt.started`, `api.event`, `assistant.delta`, `assistant.message`, `tool.call`, `tool.result`, `model.call.completed`, `run.completed`.
- Final message: the `assistant.message` event with `payload.phase == "final_answer"`, field `payload.text` (here `"DONE"`). Other `assistant.message` events have `payload.phase == "commentary"`. `run.completed` has `payload.status == "completed"`.
- On SIGTERM tact catches the signal, emits `run.failed` with `payload.status == "cancelled"`, and exits 0. A supervisor must read the stream, not only the exit code, to learn a tact run was cancelled.
- Fixture: `fixtures/harness/tact.jsonl` has its `api.event` lines removed (300 of 350 lines in the raw stream, 1.1 MB), see Fixture redaction.

## SIGTERM test

Prompt: `Count slowly to 500, one number per message, running `sleep 2` between each.` SIGTERM sent after 12 s, stdin from `/dev/null`.

| Harness | Exit status | Note |
| --- | --- | --- |
| claude | 143 | no `result` event written |
| codex | 143 | stream stops mid-turn |
| tact | 0 | last event `run.failed`, `status: cancelled` |

## Fixture redaction

The raw streams held private material, so the committed fixtures were scrubbed. No credentials were found (bearer tokens, `sk-`, `ghp_`, JWT prefixes, authorization headers). What was removed:

- tact: every `api.event` line (they resend the full system and developer instructions, internal repo names and the skill list). All other lines are unchanged except for the redactions below. The tact fixture is now about 24 KB.
- tact: `input.accepted` `payload.input` (it embeds agent context) and `run.started` `payload.websocket_url` (internal host) are set to `REDACTED`.
- claude: in the `init` event, `tools`, `mcp_servers`, `slash_commands`, `agents`, `skills`, `plugins`, `memory_paths` and `additional_directories` are set to `REDACTED`. The `hook_response` `output` and `stdout` fields that held text are set to `REDACTED` because the session hook injected instruction text. `stderr` was already empty and is unchanged.
- All fixtures: the home directory was replaced with `/Users/user/` (also in the `-Users-...-` path segment of scratch paths), and the organisation name is replaced with `REDACTED`.
- tact: a GPG signing key id (from a `gpg: skipped` error in two `tool.result` events) is replaced with `REDACTED`.

Every line is still valid JSON, and the final-message locations above are intact.
