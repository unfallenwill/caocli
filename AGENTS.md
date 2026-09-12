# caocli — AGENTS.md

Short guide for AI agents. Code shape and design rationale live in source comments; user-facing docs live in `README.md`.

## Dev environment tips
- Add deps by editing the `Cargo.toml` of the crate that uses them: the top-level one for the agent, `crates/anthropic/Cargo.toml` for the Anthropic wire. `cargo build` fetches them.
- CI plans live in `.github/workflows/`.
- For a quick type check, prefer `cargo check` over `cargo build` (it skips codegen).

## Testing instructions
- CI is `.github/workflows/ci.yml`; what it runs is the gate list in PR instructions.
- The repo is a workspace: `caocli` plus the `crates/anthropic` SDK. Every gate takes `--workspace`, or the SDK's tests and coverage never run.
- Unit + integration tests: `cargo test --workspace`. A single test: `cargo test <pattern>`.
- The coverage gate needs `cargo-llvm-cov` and `llvm-tools-preview` installed; once they are, `cargo llvm-cov --workspace --fail-under-lines 90` enforces ≥ 90% lines.
- **Terminal behavior is only real under a pty** — `cargo test` runs on a pipe, so TTY-only branches (status line, key bindings, escape sequences) never execute there. Use one of:
  - `scripts/pty_smoke.py` — plain front end (status line, Ctrl-C, `/login` hidden answer)
  - `scripts/tui_smoke.py` — TUI front end (`SMOKE_LIVE=1` runs a live turn on screen)
  - `script -qec "..." /dev/null` for a one-off
  - **Rebuild before running** (`cargo build`) — a smoke run against a stale binary looks exactly like a bug in your change.
- Assert on the harness grid, not on stdout bytes — diff rendering only writes what changed, so an unchanged cell never appears in the byte stream at all.
- Before touching `heal` or request construction, run `cargo test theorem_` (the `machine::is_request_valid` invariant).

## PR instructions
- Title: `[<scope>] <Title>` (e.g. `[cli]`, `[tools]`, `[machine]`).
- Before committing, run the four CI gates in order:
  1. `cargo fmt --check --all`
  2. `cargo clippy --workspace --all-targets -- -D warnings`
  3. `cargo test --workspace`
  4. `cargo llvm-cov --workspace --fail-under-lines 90`
- Conventional Commits in English (`feat(scope): ...`, `fix(scope): ...`); commit in small steps, each commit green on its own.
- Everything committed is English: code comments, doc comments, identifiers, test names, CLI text. A CJK ideograph belongs only in test data whose purpose is display-width handling.
- If a change makes a rule in this file false, update that rule in the same commit. A rule nobody can trust is worse than a missing one.
