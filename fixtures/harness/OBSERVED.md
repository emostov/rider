# Observed headless harness behavior

Observed on 2026-11-19 with Claude Code 2.1.328, codex-cli 0.170.0.
Prompt used for every run: `Create a file named ok.txt containing OK, commit it, then reply with the single word DONE.`
Each run used its own fresh git repo (one empty initial commit) as working directory.
All runs: exit 0, `ok.txt` committed, final message `DONE`.

## Claude Code

```
claude -p --model <MODEL> --output-format stream-json --verbose --dangerously-skip-permissions \
  --strict-mcp-config --mcp-config '{"mcpServers":{}}' -- "<prompt>"
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
codex exec --model gpt-6.1-sol --json --dangerously-bypass-approvals-and-sandbox \
  --disable plugins --disable apps -C <DIR> -- "<prompt>"
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

## Codex review

```
codex review -c model="gpt-6.2-sol" -c sandbox_mode="danger-full-access" -c approval_policy="never" - < <prompt>
```

- codex-cli 0.170.0. `codex review` refuses a `[PROMPT]` together with `--base`, `--commit` or
  `--uncommitted` (exit 2, "the argument '--base <BRANCH>' cannot be used with '[PROMPT]'"), so
  the brief names the range itself (`git diff <base>...HEAD`). `-` reads the prompt from stdin.
- It has no `--model`, `--json` or bypass flag: the model and the sandbox and approval settings
  go in `-c`. The review itself runs in a subagent that takes the top-level config key
  `review_model` when one is set (in `~/.codex/config.toml` or `-c`) and `model` otherwise, so
  Rider passes both `-c model="<m>"` and `-c review_model="<m>"`. `review_model` is a known
  top-level key in 0.170.0: `--strict-config -c review_model="..."` is accepted.
- When the review delegate fails to start, Codex (per its 0.170.0 source and binary) prints `Review was interrupted. Please re-run
  /review and wait for it to complete.` as the final text and still exits 0; Rider fails such a
  run, and one with no final text. A fatal turn error (such as a provider's content-filter
  stop) is printed only on stderr, and the exit is non-zero; Rider's failure reply tails the
  stderr transcript, then the final text. Without `danger-full-access` its sandbox blocks the network, so `vb` cannot post.
- Exit code on success: 0. stdout holds only the final text, a short verdict then
  `[P1]`/`[P2]` items with absolute `file:line` paths. Fixture: `codex-review.txt` (scratch
  path replaced with `/tmp/repo`).
- stderr is the transcript: a header (`OpenAI Codex v0.170.0`, workdir, model, provider,
  approval, sandbox, reasoning effort, `session id: <uuid>`), the prompt after `user`,
  config warnings, `exec` blocks with each command and its output, and at the end a line
  `codex` followed by the final text again. No token counts are printed. Rider logs stdout as
  the agent log and stderr beside it as `<log>.stderr`. The stderr transcript is not
  committed: it held instruction text from installed skills.
- Token usage is only in the session files under `~/.codex/sessions/<yyyy>/<mm>/<dd>/`: the
  parent rollout (its first line's `payload.source` is `"exec"`, id = the printed session id)
  has no `token_count` event; a second rollout with `payload.source == {"subagent":"review"}`
  carries `event_msg` `token_count` events whose last `payload.info.total_token_usage` is the
  total (recorded run: 106,127 input, 82,700 cached, 1,325 output tokens, 34 s).
- Posting: with a prompt that tells it to, it ran `vb post "$BOARD_TASK" --schema
  rider-finding@1 -` itself on a scratch `vb serve` board and the record was accepted, so a
  worker on this harness posts findings and its result with `vb` as a `codex exec` review does.

## MCP servers

Observed on 2026-11-20 with Claude Code 2.1.x and codex-cli 0.170.0, prompt `Reply with the single word DONE.`, in a fresh empty git repo.

- Claude: `claude --help` lists `--mcp-config <configs...>` ("Load MCP servers from JSON files or strings") and `--strict-mcp-config` ("Only use MCP servers from --mcp-config, ignoring all other MCP configurations"). The inline string `'{"mcpServers":{}}'` is accepted; no file is needed. `--mcp-config` takes several values, so the next argument must be an option or `--`. Built-in tools stay. Measured with `--model claude-haiku-5-5`:

  | | init `tools` | init `mcp_servers` | first request input tokens |
  | --- | --- | --- | --- |
  | user's MCP setup | 333 | 27 | 41,036 |
  | `--strict-mcp-config --mcp-config '{"mcpServers":{}}'` | 27 | 0 | 29,666 |

- Codex: `-c mcp_servers={}` does not remove servers. `codex -c 'mcp_servers={}' mcp list` still lists every server, because the override is merged into the config table instead of replacing it. Plugins and apps add servers of their own; `--disable plugins --disable apps` (the `-c features.<name>=false` shorthand) removes those: `codex --disable plugins --disable apps mcp list` lists only the servers declared under `[mcp_servers.*]` in `~/.codex/config.toml`. Turn input tokens for `codex exec`: 19,938 before, 17,281 after. `--ignore-user-config` also drops the model provider, so it is not used.

  Re-checked on 2026-11-21 with codex-cli 0.170.0 and a scratch `CODEX_HOME` whose `config.toml` declares `[mcp_servers.alpha]` and `[mcp_servers.beta]`: `codex mcp list` and `codex -c 'mcp_servers={}' mcp list` show both `enabled`; `codex -c 'mcp_servers.alpha.enabled=false' mcp list` shows `alpha` `disabled` and `beta` `enabled`. Naming a server the configuration lacks (`-c 'mcp_servers.gamma.enabled=false'`) fails: `failed to load bootstrap configuration ... invalid transport in mcp_servers.gamma`. So Rider reads the server names from `$CODEX_HOME/config.toml` (`~/.codex/config.toml` when unset) and passes `-c mcp_servers.<name>.enabled=false` for each, to `codex exec` and to `codex review`, which takes `-c` the same way.

  Re-checked on 2026-11-21 with codex-cli 0.170.0, `codex mcp list` only, in a linked worktree (`git worktree add --detach`) of a repository whose branch commits `.codex/config.toml` declaring a `command` server and a `url` server:
  - With `[projects."<repo>"] trust_level = "trusted"` in the user's config (the path as `pwd -P` prints it), the worktree inherits the trust and both project servers are listed `enabled`. Without the trust entry they are not loaded.
  - `-c 'projects."<repo or worktree>".trust_level="untrusted"'` does not stop them loading.
  - `-c mcp_servers.<name>.enabled=false` turns a project server off when the project is trusted, but fails (`invalid transport`) when it is not, since no loaded layer declares the name. Adding a stand-in transport of the server's own kind holds either way: `-c mcp_servers.<name>.command="true"` for a `command` server, `-c mcp_servers.<name>.url="http://127.0.0.1:1/"` for a `url` server; giving a `url` server a `command` fails with `url is not supported for stdio`. So Rider reads `<worktree>/.codex/config.toml` after preparing the worktree and passes both settings for each server it declares.
  - An empty `CODEX_HOME` is treated as unset: Codex warns `codex_home: <HOME>/.codex` and reads that, so Rider does too.
  - `codex review --help` lists `--disable <FEATURE>`; `codex review` takes `--disable plugins --disable apps` as `codex exec` does.
  - A `toml` parse error's text quotes the offending line, so Rider reports only the file and the line number.

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
