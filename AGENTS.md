# caocli — agent behavior guide

This repository is developed and maintained by AI agents. This file is written for
agents and records only conventions that are **stable, independent of the current
implementation, and not inferable from general Rust/git experience**.

It deliberately does **not** describe code structure, file responsibilities, function
names, constants, field formats, or command-line options — those change fast, so read
the code when you need them. The purpose of this file is to guide agent behavior in
this repository, not to replace reading the code.

## Definition of done

Any change must pass the following in order before you consider it done:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo llvm-cov --fail-under-lines 90   # line coverage gate
```

- Pushing triggers CI; any red above blocks merging. CI additionally runs a
  dependency security audit.
- Commit messages are English, Conventional Commits (`feat(scope): ...`,
  `fix(ui): ...`, etc.).
- Commit in small steps; every commit must independently pass the gates above.

## Language

Everything committed to this repository is **English**: code comments, doc comments,
test names, assertion messages, panic messages, user-facing CLI text, and the text
that is sent to the model (tool names, tool descriptions, parameter descriptions,
tool results, synthesized markers). Documentation and commit messages too.

Chinese (or any other non-English text) in the repo is a defect, not a style choice.
The only place non-ASCII text is legitimate is test data whose *purpose* is
multi-byte handling — and even then prefer accented Latin or emoji over CJK.

## How to verify

Unit tests are not sufficient evidence that a change is correct: this is a program
that talks to a network API and drives an interactive terminal.

- After a change, run `cargo test`, then do one smoke run through **one-shot mode
  against the real API** (`-p`) — that is the agent's primary self-test channel.
- Pure logic (parsing, stream aggregation, truncation) should be unit tested;
  boundaries with side effects (network, terminal, files) rely on the smoke run.
- **Terminal behavior must be verified under a pty**: a unit test's stdout is a pipe,
  so TTY-only branches (status bar, key bindings, escape sequences) never execute.
  Use `script -qec "..." /dev/null` to get a pseudo-terminal for smoke runs.

## Design principles (do not violate)

- **Deliberate minimalism**: one main loop. Providers are expressed as a static preset
  table — no dynamic registration, plugin systems, or other indirect layers; write it
  directly when you can. `trait Ui` is the only exception — it is not an abstraction
  layer but the machine's notification vocabulary (exactly one in-process
  implementation, `Renderer`).
- **The machine decides, never executes**: turn flow is determined by
  `machine::next_action` (a pure fold over the persisted history); the interpreter
  executes and writes back. To add behavior to the loop, extend the vocabulary first
  (the target event table lives in the `machine.rs` module comment); do not stuff
  implicit state into the loop.
- **Callbacks receive notifications only, never return data**: a `Ui` implementation
  must not block and must not return decisions to the machine; operations that need
  results (such as a future approval gate) must come back to the decision function as
  events.
- **State = a fold of the log**: `Session` is the only persistent state; there is no
  second source of truth in memory, and at any moment it can be rebuilt by
  `Session::load`.
- **The transcript is cells, and replay is not a second renderer**: every line the UI
  writes is a cell painted by the same painter, whether it arrives as a live
  notification or is folded out of the session log when resuming. A resumed session
  must be laid out exactly like the one that was watched live — adding a notification
  means adding a cell, never a second formatting path.
- **Ctrl-C during a turn is an out-of-band Command (graceful cancel), not a process
  kill**: the current effect is dropped (stream disconnected, child process
  `kill_on_drop`), unanswered calls are persisted with a deterministic cancellation
  marker, and the history still satisfies `is_request_valid`; cancellation is handled
  in the interpreter layer and never enters `next_action`. The SIGINT listener is
  created once per turn and subscribed at construction time — constructing a listener
  inside each `select` leaves a window between two `select`s in which a signal that
  arrives exactly then is swallowed forever (tokio watch semantics, measured in the
  pty smoke test).
- **Turn step cap**: at most `machine::MAX_TOOL_STEPS` (500) tool-call steps per turn;
  exceeding it ends the turn with a deterministic marker — a product-level
  termination guarantee.
- **Tool results are always text and are never raised as hard errors**: failures are
  also passed back to the model so it can correct itself.
- **Approval gate**: execution is trusted by default; with `--ask`, Bash/Edit/Write
  require a user y/N before running (Read is always allowed). Denial, cancellation,
  and the step cap all persist deterministic text markers that are passed back to the
  model so it can adjust on its own; the command echo is unchanged.
- **Session logs are append-only and never rewritten**: a crash loses at most a
  trailing partial line, and corrupt lines are skipped when reading; interrupted tool
  calls are healed **deterministically** at load time (in-memory view only — the
  synthesized text must be a constant, otherwise the prefix cache becomes unstable).
- **Exactly one renderer per process**: streaming output and usage accounting must go
  through the same instance, otherwise counts are lost.
- Truncating or clipping text must land on a UTF-8 character boundary, and any text
  measured against a terminal width must be measured in **display columns**, not
  chars: a CJK ideograph or an emoji is one char but two columns, so char-based
  arithmetic silently overruns the field it was sizing.

## Prefix cache (must read before changing request or message construction)

The backend matches its cache on the exact request prefix; the hit rate directly
determines cost and latency:

- The system prompt is a fixed constant — it is **forbidden** to inject time, cwd,
  random ids, or any other dynamic content.
- History messages are **replayed byte-for-byte**: no trimming, no reordering, no
  clipping, no compression, no normalization.
- Changing the tool set, the tool order, or any tool schema text (names,
  descriptions, parameter descriptions) changes the prefix and causes a full cache
  miss — only do it deliberately.
- Any change that "optimizes the history" equals a full cache miss.
- Verify with the per-turn hit/miss token counts; do not go by feel.

## Backend API hard constraints (must read before changing request construction)

The official docs are authoritative (`thinking_mode`, the `kv_cache` guide). The two
easiest ones to trip over:

- **When a request carries `tools`, the `reasoning_content` of historical assistant
  messages must be passed back verbatim (DeepSeek returns 400 when it is missing).**
- **History validity has an executable specification**: `machine::is_request_valid`
  (every declared call has exactly one result within the immediately following
  window; no stray tool results). Both `heal` and request construction target it as
  an invariant; its tests pin down "any crash prefix, once healed, is valid" by
  exhaustively enumerating a bounded shape family (19608 shapes). Run those two
  theorem tests before changing `heal` or request construction.
- In a streaming response, thinking content precedes the body text; token usage rides
  on the last content block and there is no separate usage event.

## Working style

- Read the code before changing it; do not rely on implementation descriptions in this
  file (it deliberately omits them).
- When behavior is uncertain, first confirm it with a `-p` or pty smoke run, then draw
  conclusions.
- Do not abstract for "we might need it later"; the value of this project is being
  small and direct.
