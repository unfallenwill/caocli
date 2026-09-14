# caocli — AGENTS.md

Short guide for AI agents. Code shape and design rationale live in source comments; user-facing docs live in `README.md`.

## Workspace layout
The repo is a Cargo workspace. `caocli` is the binary; the SDKs and the protocol layer live as their own crates so each one can iterate, be tested, and be covered on its own:

```
crates/
  anthropic/         # Anthropic Messages API client (caocli-anthropic)
  caocli-core/        # Shared types: ToolDef, FunctionDef. Pure data shapes.
  caocli-mcp/         # Model Context Protocol client (the Hub, the JSON-RPC
                      # transports, the stub fixture). Public surface: Hub,
                      # McpGuard, PREFIX, is_tool, tool_name, ServerStatus,
                      # ServerState.
  openai/             # OpenAI Chat Completions client (caocli-openai)
src/                  # The agent: provider glue, request/stream/turn loop,
                      # tools dispatcher, front ends, repl, mcp→agent wiring.
```

## Dev environment tips
- Add deps by editing the `Cargo.toml` of the crate that uses them: the top-level one for the agent, `crates/<crate>/Cargo.toml` for the workspace crates. `cargo build` fetches them.
- CI plans live in `.github/workflows/`.
- For a quick type check, prefer `cargo check` over `cargo build` (it skips codegen).

## Testing instructions
- CI is `.github/workflows/ci.yml`; what it runs is the gate list in PR instructions.
- Every gate takes `--workspace`, or the SDK crates' tests and coverage never run.
- Unit + integration tests: `cargo test --workspace`. A single test: `cargo test <pattern>`. A single crate: `cargo test -p <crate>`.
- The coverage gate needs `cargo-llvm-cov` and `llvm-tools-preview` installed; once they are, `cargo llvm-cov --workspace --fail-under-lines 90` enforces ≥ 90% lines across the whole workspace. Per-crate coverage is available with `cargo llvm-cov -p <crate>`; both `caocli` and `caocli-mcp` independently stay above 90%.
- **Terminal behavior is only real under a pty** — `cargo test` runs on a pipe, so TTY-only branches (status line, key bindings, escape sequences) never execute there. Use one of:
  - `scripts/pty_smoke.py` — plain front end (status line, Ctrl-C, `/login` hidden answer)
  - `scripts/tui_smoke.py` — TUI front end (`SMOKE_LIVE=1` runs a live turn on screen)
  - `script -qec "..." /dev/null` for a one-off
  - **Rebuild before running** (`cargo build`) — a smoke run against a stale binary looks exactly like a bug in your change.
- The MCP tests start a real server (`crates/caocli-mcp/src/stub.rs`, a bash
  script the test writes out and runs): they need `bash` on PATH, like the
  shell tool's tests.
- Assert on the harness grid, not on stdout bytes — diff rendering only writes what changed, so an unchanged cell never appears in the byte stream at all.
- Before touching `heal` or request construction, run `cargo test theorem_` (the `machine::is_request_valid` invariant).

## PR instructions
- Title: `[<scope>] <Title>` (e.g. `[cli]`, `[tools]`, `[machine]`, `[mcp]`).
- Before committing, run the four CI gates in order:
  1. `cargo fmt --check --all`
  2. `cargo clippy --workspace --all-targets -- -D warnings`
  3. `cargo test --workspace`
  4. `cargo llvm-cov --workspace --fail-under-lines 90`
- Conventional Commits in English (`feat(scope): ...`, `fix(scope): ...`); commit in small steps, each commit green on its own.
- Everything committed is English: code comments, doc comments, identifiers, test names, CLI text. A CJK ideograph belongs only in test data whose purpose is display-width handling.
- If a change makes a rule in this file false, update that rule in the same commit. A rule nobody can trust is worse than a missing one.
