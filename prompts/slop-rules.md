# Slop rules

Check a diff against every rule below. Each heading names a rule; a finding cites the
heading it matches exactly, and an observation that matches no rule is not slop. Where a
rule conflicts with the repository's `AGENTS.md`, `AGENTS.md` wins.

## Comment that is not a hidden invariant

A comment that says what the code does, how or why it works, or what a language or library
guarantees, rather than an invariant the code cannot show. Delete it; public doc comments,
lint suppression reasons, and `TODO`s that link an issue are exempt.

## Doc comment on an untouched item

A doc comment added to or expanded on an item the branch did not create, or one on a private
item that only restates its signature. Delete it.

## Section banner

A comment that labels a block instead of explaining it, such as "Helpers", "Step 3:",
"Setup", "Act", or "Assert". Delete it.

## Hedging comment

"Note:", "Important:", a `TODO` with no owner or issue link, or "should", "probably", "for
now", or "hopefully" inside a comment. Resolve the uncertainty in code or delete the comment.

## Impossible-state handling

A check, default, or error branch for a state that no caller can produce and the types or
constructors already rule out. Remove it; if the state is reachable, report a bug instead.

## Name-only helper

A function with one caller, a body shorter than its signature plus call site, and a name that
says nothing the call site does not. Inline it.

## Test name that says nothing

A test named "works", "basic", "happy path", "success", "case 1", or after the function under
test with no behaviour stated. Rename it to the behaviour it asserts.

## Test below the highest level that reaches the behaviour

A unit test of internals, or a test that calls internal functions directly. Move the
assertions up to a black-box test that runs the program the way a user does; keep a unit test
only when the repository's `AGENTS.md` requires one.

## Redundant test

A test whose assertions another test already makes. Delete it; when a higher-level test
covers a unit test's assertions, delete the unit test.

## Ontological test

A test that checks a thing exists or has its declared shape: a constant equals its literal,
a constructor returns its inputs, a mock returns what it was told. Delete it.

## Implementation-detail test

A test that would fail after a refactor that keeps caller-visible behaviour the same, such
as tests of private helpers, internal representations, mock calls, or logs. Assert the
caller-visible outcome instead, or delete the test.

## Unnecessary test

A test whose failure is not a bug a caller could hit: tests of test helpers, generated code,
or dependencies, or tests that only check that a call succeeded. Delete it.

## Duplicated test setup

The same three or more lines of arrangement in three or more tests. Move them to a shared
helper or fixture, or collapse the tests into one table-driven test.

## Error message slop

An error message that repeats the error type's name, says "please try again", has an
exclamation mark, or apologises. Rewrite it to state what failed and what the caller can
change.

## Leftover scaffolding

Commented-out code, or debug output added while developing. Delete it.
