# AGENTS.md

Rider is written in TigerStyle and tested thoroughly enough that it can be rewritten with confidence. When a rule here conflicts with a habit from another codebase, this file wins.

- Before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`, the scenarios the change added or changed one at a time (`cargo test --test scenarios <name>`) and `cargo test --test sim`; the full `cargo test` runs in CI before the PR is marked ready. The lead's small mechanical edit runs only fast checks.
- Commit `Cargo.lock`. Commit messages are one sentence and name no tool or assistant.
- Rider is developed with Rider. Changes to this repository are planned by the lead as tasks on the board channel `work/rider` and carried out by the `rider up` daemon, so every change exercises the tool. Work done any other way is the exception and says why.
- The board is where work is discussed. Findings, problems and deferred work found while building or using Rider go on the board (a finding record, a reply in the task's thread, or the project's deferred-work thread), not to the project's tracker. The tracker (GitHub issues or Linear, whichever the project uses) is for work that must outlive the board: only when a run ends does the lead, or a person, file one issue there for each item still open, written as a brief a stranger could act on: what happens, the fix wanted, the paths owned, the command that proves it.
- Security first: correctness and code security outrank features. Prefer verification (types, assertions, simulation) over trust.
- Keep Rider simple and uncustomised: prefer fewer configuration knobs and less orchestration machinery. A feature that only tunes around a current model's weakness is suspect.
- Goals are measurable: a change to performance or resource use states a target (10x faster, a memory bound) and measures before and after.
- There is no `docs/` directory. AGENTS.md, README.md and the shipped skills under `skills/` are the only prose. How Board and the agent CLIs behave lives in recorded fixtures, scenarios and types.

## TigerStyle

- Assert arguments, return values, and invariants: at least two assertions per function on average, with `assert!`, never `debug_assert!`. An assertion must be able to fail.
- Assertions are for programming errors only. Anything from the board, an agent, git, or the filesystem is external input: a bad value becomes a returned error, never a panic.
- Every loop and every collection has an upper bound declared as a named constant. Reaching it is a returned error, not growth.
- No recursion, no `unsafe`, no async runtime. Threads and bounded channels only.
- When work has several independent parts (board calls, git calls, agent waits), run them in parallel and join them, not one after another, whenever their order does not matter.
- Model parallel work as actors: each thread owns its state and talks to others only through bounded channels. A full channel makes the sender wait or shed work (back pressure), never grow.
- Functions are at most 70 lines; lines are at most 100 columns.
- Errors are enums, handled at every call site. No `unwrap()` or `expect()` outside tests. No `thiserror` or `anyhow`.
- Use a well-maintained crate when it saves writing and maintaining code of our own, but weigh each new one's supply-chain risk. Prefer fewer crates, and own small code rather than add a crate that pulls a large tree.
- Use checked arithmetic (`checked_add`, `try_from`) on lengths, indices, and counts from external input, not `as` casts or bare `+`.
- A lint suppression uses `#[expect(lint, reason = "...")]` naming the specific false positive, not a bare `#[allow]`.

## Structure for testing

- Keep decisions pure and I/O at the edges, because it keeps the code simple to reason about and to simulate. Do not extract a function only so a test can reach it.
- Board, git, processes, the clock, and sockets sit behind traits or thin wrappers so the deterministic simulator can substitute fakes and inject faults.
- Otherwise keep call stacks flat: inline one-use helpers and extract on the second call site. Length alone doesn't justify a split.
- Board is reached only through the `Board` trait. The role table in `src/roles.rs` and the label scheme are each defined once.

## Types and data flow

- Encode every valid state in the type system and make invalid states unrepresentable with enums, domain wrappers, and private constructors. Keep each value in one authoritative place.
- An implicit state machine (flags, `Option`s and strings that together mean a state) becomes an enum, so whole categories of bug fail to compile.
- Parse, don't validate: run the fallible check once at the boundary and return a narrow type that carries the proof.
- When converting an external shape (Board JSON, an agent's output stream) into our own type, name every field you rely on and reject what you don't recognise where that is safe, so a changed upstream shape fails loudly.
- Keep missing data distinct from malformed data. Default only for intentional absence; report malformed values with the field and operation.
- Treat serialized shapes as compatibility boundaries; keep runtime types separate from wire or persisted schemas when they need to differ.
- Prefer infallible constructors: do fallible acquisition first and pass finished dependencies in.
- Move owned values on their last use instead of cloning. Don't allocate inside a loop when a borrow works.
- Match enums exhaustively when variants need distinct behaviour; use a wildcard only when every other variant, current and future, is handled alike.
- Use raw string literals (`r#"..."#`) for multi-line or quote-heavy strings and fixtures.
- Prefer short imported names over fully qualified paths.

## Processes and git

- Every child process gets a closed standard input, unless text is being fed to it, and has its output read through a named byte bound.
- Text handed to another CLI goes after a final `--` or through standard input, never as a bare argument that could be read as an option.
- Rider runs against real repositories. Never reset, clean, force, push, merge, or delete a branch. A worktree is removed only when it is safe to: no modified or untracked files, on its expected branch, no operation in progress. Git-ignored build output does not stop a removal.
- Clear inherited `GIT_*` repository variables on every git child.

## Comments

- No `///` or `//!` docs and no module docs.
- A `//` comment exists only for an external CLI's quirk or a platform constraint the code cannot show. Delete every other comment.
- Invariants go into types, assertions and names, not comments.

## Tests

- No unit tests. Behaviour is pinned from outside, so Rider can be rewritten: a test that would break when the code is restructured without changing what `rider` does is testing the wrong thing.
- Behaviour a person can observe (what `rider` does to the board and to a repository) is pinned by a black-box scenario test in `tests/scenarios/`, written as Given / When / Then against the real binary. Add or change the scenario first, see it fail, then change the behaviour.
- The deterministic simulator in `tests/sim/` drives the core through its events with seeded faults and checks invariants, not internals.
- A bug found anywhere becomes a simulator seed or a scenario that fails before the fix, so it never recurs.
- Assert on the value: `assert_eq!` with the full expected value, not `assert!(x.is_some())` or `assert!(x.is_ok())`, when the value is available.
- Avoid `unreachable!()` in tests; use `assert_eq!`, `assert!(matches!(...))`, or a descriptive `panic!`.
- Tests never run a real agent CLI (`claude`, `codex`, `tact`); use `fake_agent` and recorded fixtures. Tests that need a repository build one under the system temp directory and remove it.
- Fixtures are real recorded output, redacted before commit: no home-directory paths, organisation or private repo names, key ids, or instruction text.

## Secrets

- Never log a token or include one in an error message, and never `#[derive(Debug)]` on a type that holds one.
