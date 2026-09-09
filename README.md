# caocli

[![CI](https://github.com/unfallenwill/caocli/actions/workflows/ci.yml/badge.svg)](https://github.com/unfallenwill/caocli/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A minimal terminal coding agent in Rust, backed by an OpenAI-compatible
`/chat/completions` API: DeepSeek by default, Zhipu BigModel (GLM) via
`--provider glm`. It streams the model's thinking (`reasoning_content`) in
dim gray, then runs a tool loop over four tools: `Bash`, `Read`, `Edit`,
and `Write`. Sessions are append-only JSONL logs under `~/.caocli/sessions/`,
resumable across runs and replayed byte-for-byte so the backend's prefix
cache keeps hitting.

## Requirements

- Rust 1.85+ (edition 2024)
- An API key for the selected provider: `DEEPSEEK_API_KEY` (default) or
  `ZAI_API_KEY` / `GLM_API_KEY` for `--provider glm` (or `CAOCLI_API_KEY`
  to override either)

## Quick start

```bash
export DEEPSEEK_API_KEY=sk-...          # or: export ZAI_API_KEY=...

cargo run --                            # interactive REPL (/help for commands)
cargo run -- -c -p "check disk usage"   # one-shot, continuing the latest session
cargo run -- --effort max --model deepseek-v4-pro -p "..."
cargo run -- --provider glm -p "1+1"    # Zhipu GLM-5.3-Flash
cargo run -- --list                     # list sessions and exit
```

## Usage

### REPL commands

| Command | Description |
|---|---|
| `/help` | Show available commands |
| `/new` | Start a new session, inheriting the current model settings |
| `/sessions` | List sessions (id, message count, last user message preview) |
| `/resume <id>` | Switch to an existing session, replaying its history to the screen |
| `/exit`, `/quit`, `/q` | Quit |

### Multi-line input

`Enter` submits; `Ctrl-J` inserts a newline, so the whole buffer is sent as
one prompt. Note that once the buffer spans lines, `Enter` only submits when
the cursor sits at the end of the input — elsewhere it inserts a newline.
`Shift-Enter` is not supported: most terminals send the same byte as `Enter`,
and rustyline 18 does not parse CSI-u key reports. Pasting text that contains
newlines also works (bracketed paste).

### CLI flags

| Flag | Description |
|---|---|
| `-p <PROMPT>` | Run one prompt (including the tool loop), then exit |
| `--provider <NAME>` | Backend provider: `deepseek` (default) or `glm` |
| `--model <MODEL>` | Model id; defaults to the provider's default model |
| `--effort <EFFORT>` | Reasoning effort: `low`, `high`, or `max` |
| `-c, --cont` | Continue the most recent session |
| `--resume <ID>` | Resume a specific session by id |
| `--list` | List sessions and exit |
| `--no-status-bar` | Disable the REPL status bar |
| `-h, --help` / `-V, --version` | Print help / version |

With no `-p`, caocli starts a REPL. CLI flags override the settings stored
in a resumed session only when explicitly provided. Resuming a session
(`-c`, `--resume`, `/resume`) replays the stored history to the screen: user
messages with a `›` prefix, assistant reasoning dimmed, tool calls and tool
result summaries as they were rendered live (full tool output is not replayed).

### Status bar

In the REPL, a status bar pinned to the bottom line shows the active model
and the session's cumulative cache hit rate, right-aligned:

```
deepseek-v4.1-flash-expires-on-0910 · cache 98.6% · hit 32384 · miss 461
```

The model segment mirrors the active session's meta and is refreshed on
`/new`, `/resume`, and CLI overrides.

It appears only when stdout is a TTY and the terminal has at least 3 rows;
`--no-status-bar` turns it off. The bar reserves the last terminal line via
a scroll region, so output scrolls above it — the trade-off is that lines
scrolled out of the region do not enter the terminal's scrollback buffer.
Stats reset when you switch sessions (`/new`, `/resume`), and the bar is
restored on exit.

### Providers

`--provider` selects a static preset (endpoint, default model, key env).
It is a per-run choice and is not stored in the session, so resume a GLM
session with `--provider glm` again (e.g. `caocli -c --provider glm`).

| Provider | Endpoint | Default model |
|---|---|---|
| `deepseek` | `api.deepseek.com` | `deepseek-v4.1-flash-expires-on-0910` |
| `glm` | `open.bigmodel.cn` (coding) | `GLM-5.3-Flash` |

Both speak the same `thinking` / `reasoning_content` protocol, so the request
builder and stream parser are shared. Thinking is always on.

### Environment

| Variable | Description |
|---|---|
| `DEEPSEEK_API_KEY` | API key for `--provider deepseek` (the default). |
| `ZAI_API_KEY`, `GLM_API_KEY` | API key for `--provider glm`; either works. |
| `CAOCLI_API_KEY` | If set, overrides the provider-specific key. |
| `NO_COLOR` | If set, disable ANSI colors (block separation is preserved). |

## Tools

The model can call four tools. Tool results are always plain text: failures
are returned to the model as text so it can recover, never as a hard error.

| Tool | Behavior |
|---|---|
| `Bash` | Run one `bash -c` command. 120s timeout; stdout and stderr are each truncated to 10 KiB. |
| `Read` | Read a UTF-8 text file. Output truncated to 10 KiB. |
| `Edit` | Replace `old_string` with `new_string`; `old_string` must match exactly once. Written atomically via tmp + rename. |
| `Write` | Create or fully overwrite a file; parent directories are created automatically. |

Limits: 10 KiB of output per tool result, 10 MB per file read/write.

## Design notes

- **One loop, deliberately minimal.** Providers are a static table, not a
  trait or dynamic registry — the whole agent is still the request → stream
  → tool_calls → execute → continue cycle.
- **Thinking-mode streaming.** `delta.reasoning_content` arrives before
  `delta.content`; they render as separate blocks (dim gray thinking,
  normal-colored answer). `NO_COLOR` or a non-TTY drops the color codes.
- **`reasoning_content` must be replayed.** When a request carries `tools`,
  DeepSeek requires the `reasoning_content` of every historical assistant
  message to be sent back verbatim; omitting it is a 400. Sessions therefore
  store the full assistant messages.
- **Append-only session logs.** Files are never rewritten, so a crash costs
  at most a trailing partial line. Corrupt lines are skipped on load.
- **Prefix-cache friendly.** `SYSTEM_PROMPT` is a compile-time constant and
  history is replayed byte-for-byte — no trimming, reordering, or
  compaction. Injecting volatile data (time, cwd) or changing the tool set
  or its order invalidates the cache. The `tokens:` line after each turn
  reports `hit`/`miss` prompt tokens from `usage`.
- **Cache usage is normalized across providers.** DeepSeek reports flat
  `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`; GLM/OpenAI report
  nested `prompt_tokens_details.cached_tokens` (miss derived as
  `prompt_tokens - cached_tokens`). Both shapes land in the same hit/miss
  counters; a provider that reports neither shows `cache —` rather than a
  fake 0%.
- **Token usage** is attached to the final content chunk of the stream, not
  to a separate SSE event.
- **Two levels of cache visibility.** Every sub-request prints a `tokens:`
  line (`in/total`, `hit/miss`, `out`); the status bar aggregates `hit`/`miss`
  across the whole session.

## Session storage

Sessions live in `~/.caocli/sessions/<YYYYMMDD-HHMMSS>.jsonl`. Each line is
one JSON object tagged by `t`; `meta` lines override earlier ones on load.

```jsonl
{"t":"header","id":"20250101-120000","created_at":1735704000,"model":"deepseek-v4.1-flash-expires-on-0910","reasoning_effort":"high"}
{"t":"msg","message":{"role":"user","content":"check disk usage"}}
{"t":"meta","model":"deepseek-v4-pro","reasoning_effort":"max"}
```

## Development

```bash
cargo fmt                                  # CI runs --check
cargo clippy --all-targets -- -D warnings  # CI gate
cargo test
cargo llvm-cov --fail-under-lines 90       # CI gate; needs cargo-llvm-cov + llvm-tools-preview
cargo audit                                # CI runs rustsec/audit-check
```

End-to-end smoke test (the primary self-check channel after a change):

```bash
DEEPSEEK_API_KEY=... cargo run -- -p "what is 1+1"
```

CI runs on every push to `master` and every pull request: fmt + clippy +
tests + line coverage ≥ 90% + dependency audit. Any red gate blocks merging.

[`AGENTS.md`](AGENTS.md) is the behavior guide for AI agents working in this
repo: the definition of done, verification channels, and the stable invariants
(prefix-cache discipline, API contract, design principles). It deliberately
omits code structure, which lives in the code itself.

## License

[MIT](LICENSE)
