# caocli

[![CI](https://github.com/unfallenwill/caocli/actions/workflows/ci.yml/badge.svg)](https://github.com/unfallenwill/caocli/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A minimal terminal coding agent in Rust, backed by an OpenAI-compatible
`/chat/completions` API: DeepSeek by default, Z.AI's GLM coding endpoint via
`--provider zai-coding-cn`. It streams the model's thinking (`reasoning_content`) in
dim gray, then runs a tool loop over four tools: `Bash`, `Read`, `Edit`,
and `Write`. Sessions are append-only JSONL logs under `~/.caocli/sessions/`,
resumable across runs and replayed byte-for-byte so the backend's prefix
cache keeps hitting.

## Requirements

- Rust 1.85+ (edition 2024)
- An API key, which is stored by `/login` (see below)

## Quick start

```bash
cargo run --                            # interactive REPL: /login, then chat
cargo run -- -c -p "check disk usage"   # one-shot, continuing the latest session
cargo run -- --effort max --model deepseek/deepseek-v4-pro -p "..."
cargo run -- --provider zai-coding-cn -p "1+1"   # Z.AI Coding CN, glm-5.3-flash
cargo run -- --list                     # list sessions and exit
```

The first run has no key yet: `/login` lists the providers by name, takes the
key without echoing it, and writes it to `~/.caocli/settings.json`. That file is
where a key lives — there is no environment variable to set up, and so no second
place for a key to hide in.

## Usage

### REPL commands

| Command | Description |
|---|---|
| `/help` | Show available commands |
| `/new` | Start a new session, inheriting the current model settings |
| `/sessions` | List sessions (id, message count, last user message preview) |
| `/resume <id>` | Switch to an existing session, replaying its history to the screen |
| `/login` | Choose a provider and store its API key. The list shows each provider by *name* (`DeepSeek`, `Z.AI Coding CN`); the plain prompt prints the id beside it, since that is what `/login <id>` takes |
| `/model` | Choose a model, named `<provider id>/<modelid>` (`deepseek/deepseek-v4-pro`, `zai-coding-cn/glm-5.3`); it switches the model and, when the name carries another provider, the backend with it |
| `/exit`, `/quit`, `/q` | Quit |

`/login` asks for the key as a question rather than as a line: the prompt is
drawn, the answer is typed with the text masked in the full-screen front end and
with the terminal's echo off in the plain one, and it is never written to the
transcript, the session log, or the input history. A login for the provider the
session is already talking to takes effect on the next turn.

A model is named by the provider that serves it — `deepseek/deepseek-v4-pro`,
`zai-coding-cn/glm-5.3` — both on the status line and in `/model`. A bare id
(`/model deepseek-v4-pro`, `--model deepseek-v4-pro`) belongs to the provider
the session is running on. Switching to a model whose provider has no key yet is
refused, with `/login <provider id>` as the reason.

A provider has an id and a name, and they are used for different things: the id
(`deepseek`, `zai-coding-cn`) is what addresses it — `--provider`, `/login <id>`,
`<id>/<modelid>`, the session meta, `settings.json` — and the name is what a
person reads in `/login` and in an error (`no API key for Z.AI Coding CN: run
/login zai-coding-cn`).

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
| `--provider <NAME>` | Backend provider: `deepseek` (default) or `zai-coding-cn`. Settles the endpoint of a new session; a resumed session keeps the one its meta names |
| `--model <MODEL>` | Model id as `<provider>/<modelid>`, or bare for `--provider`; defaults to the provider's default model |
| `--effort <EFFORT>` | Reasoning effort: `low`, `high`, or `max` (default `max`); other values are rejected locally. On GLM, `low` answers without emitting `reasoning_content`. |
| `-c, --cont` | Continue the most recent session |
| `--resume <ID>` | Resume a specific session by id |
| `--list` | List sessions and exit |
| `--ask` | Approval gate: ask y/N before Bash/Edit/Write (Read always allowed). Denials are recorded as deterministic markers the model can see and adapt to. |
| `--no-tui` | Keep the plain prompt instead of the full-screen front end |
| `--no-status-bar` | Disable the plain prompt's status bar |
| `-h, --help` / `-V, --version` | Print help / version |

With no `-p`, caocli starts a REPL. CLI flags override the settings stored
in a resumed session only when explicitly provided. Resuming a session
(`-c`, `--resume`, `/resume`) replays the stored history to the screen: user
messages with a `›` prefix, assistant reasoning dimmed, tool calls and tool
result summaries as they were rendered live (full tool output is not replayed).

### The screen front end

By default caocli takes the whole screen. The transcript fills it, and the last
rows are the pinned region: the input box, and under it the status line.
The box is as tall as what is in it — `Ctrl-J` adds a line and room for it — and
the transcript takes those rows back when the line is submitted. It is ruled off
above and below rather than boxed in, so a line of the session and a line being
typed start in the same column.

```
▸ Read Cargo.toml
ok: package.name = caocli (312 bytes)

› run the tests and fix what fails
› and bump the version
──────────────────────────────────────────────────────────────────────────
›  the turn is running · Enter queues this line
──────────────────────────────────────────────────────────────────────────
zai-coding-cn/glm-5.3 · cache 95.3% · hit 846912 · miss 41538
```

- **The status line** is the row under the box, always the session summary:
  the model, the cache hit rate, and the raw hit/miss counts. A new session
  opens with the defaults (`cache 0.0% · hit 0 · miss 0`) and the counts move
  as the provider reports usage; what a turn is doing is the transcript's to
  say, not the status line's.
- **A line typed while a turn runs is queued, not dropped.** The box takes the
  next line as usual — Enter puts it after the current turn, drawn dimmed at the
  foot of the transcript while it waits, and the box's placeholder says so. When
  the turn ends the head of the queue runs next, so stopping a turn with `Ctrl-C`
  redirects to what was queued rather than throwing it away; a `Ctrl-C` during a
  queued turn stops that one and moves on to the next, which is how a queue is
  abandoned from the front. Commands queue too — a queued `/exit` leaves when it
  reaches the head — and a queued `/resume` with no id opens its picker when it
  runs, with the rest of the queue waiting behind the choice, since the choice is
  what is typed next.
- **The transcript is the application's**, not the terminal's scrollback: the
  alternate screen is entered on startup, so nothing drawn here reaches the
  terminal's own history. The wheel and `PageUp`/`PageDown` move through the
  session — a notch is three lines, a page is a screen; a new prompt returns to
  the end. When a draft spans several lines, the two keys scroll the box instead.
  The front end asks the terminal for the mouse so that a notch arrives as a
  notch, rather than as the `Up` and `Down` a terminal sends in its place — which
  the box reads as its own history. What that costs is the terminal's own
  selection: hold `Shift` to select text, as with any full-screen program that
  takes the mouse.
- **Ctrl-C** stops a running turn; the model is told it was stopped. What was
  queued behind it runs next rather than being lost.

`--no-tui` keeps the plain prompt instead (it is used anyway when stdout is not
a terminal), which is the front end the status bar below belongs to.

### Status bar

In the plain REPL (`--no-tui`), a status bar pinned to the bottom line shows the active model
and the session's cumulative cache hit rate, right-aligned:

```
deepseek/deepseek-flash · cache 98.6% · hit 32384 · miss 461
```

The model segment names the model by its provider and is refreshed on
`/new`, `/resume`, `/model` and CLI overrides.

It appears only when stdout is a TTY and the terminal has at least 3 rows;
`--no-status-bar` turns it off. The bar reserves the last terminal line via
a scroll region, so output scrolls above it — the trade-off is that lines
scrolled out of the region do not enter the terminal's scrollback buffer.
Stats reset when you switch sessions (`/new`, `/resume`) or models
(`/model`), and the bar is restored on exit.

### Providers

`--provider` selects a static preset: an endpoint, the models it serves and the
answer ceiling. It is settled per run, and `--model <provider id>/<modelid>` or `/model`
names a provider of its own. A session records the provider its model belongs
to, so continuing one (`caocli -c`) resumes on the same backend — no flag
needed — and `--provider zai-coding-cn` is how you move it to the other one.

| Provider id | Name | Endpoint | Models | Max answer |
|---|---|---|---|---|
| `deepseek` | DeepSeek | `api.deepseek.com` | `deepseek-flash` (default), `deepseek-v4-pro` | 384k tokens |
| `zai-coding-cn` | Z.AI Coding CN | `open.bigmodel.cn` (coding) | `glm-5.3-flash` (default), `glm-5.3` | 128k tokens |

The id is what the flags, the menus' arguments, the session meta and
`settings.json` carry; the name is what `/login` lists and what an error message
calls the provider.

The model list is what `/model` offers; naming one the table does not list is
allowed, and up to the backend to accept or reject.

Both speak the same `thinking` / `reasoning_content` protocol, so the request
builder and stream parser are shared. Thinking is always on.

Both models take 1M tokens of context, so nothing here trims history. The
"max answer" column is `max_tokens`, which is sent on every request: it bounds
a *single* completion, not the conversation, and the backends' own default is
far below what these models emit — a long `Write` would otherwise be cut off
mid-file, which the model cannot see and the next `Edit` cannot repair.

### Settings

`~/.caocli/settings.json` is written by `/login` and is the only place an API
key lives. It is keyed by provider **id**:

```json
{
  "providers": {
    "deepseek": { "api_key": "sk-..." },
    "zai-coding-cn": { "api_key": "..." }
  }
}
```

It is rewritten whole, `0600`, and there is exactly one mechanism: no
environment variable, no flag, no second location. Anything else the file holds
is kept as it was found, so it is safe to edit by hand. A key that is missing is
reported where it matters (the start of an interactive session, `/model`, or the
first turn of `-p`) with `/login <provider>` as what to do about it.

With no key in it, an interactive session still starts — `/login` is inside it —
while a `-p` run says what to do instead of failing at the first request.

### Environment

| Variable | Description |
|---|---|
| `NO_COLOR` | If set, disable ANSI colors (block separation is preserved). |

There are no API key variables: a key is only ever what `/login` stored.

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

- **One loop, deliberately minimal.** Providers are a static table, not a trait
  or dynamic registry. The turn loop is an interpreter over a pure decision
  function: `machine::next_action` folds the committed history and returns the
  next step (call the model / run the next declared tool / done). The loop
  itself carries no state — the session log is the only source of truth.
- **The machine decides, it never executes.** Tool execution, streaming, and
  persistence live in the interpreter; the decision function is pure and
  table-testable. The `Ui` trait is the machine's notification vocabulary —
  callbacks receive notices and never return decisions.
- **Thinking-mode streaming.** `delta.reasoning_content` arrives before
  `delta.content`; they render as separate blocks (dim gray thinking,
  normal-colored answer). `NO_COLOR` or a non-TTY drops the color codes.
- **`reasoning_content` must be replayed.** When a request carries `tools`,
  DeepSeek requires the `reasoning_content` of every historical assistant
  message to be sent back verbatim; omitting it is a 400. Sessions therefore
  store the full assistant messages.
- **Append-only session logs.** Files are never rewritten, so a crash costs
  at most a trailing partial line. Corrupt lines are skipped on load.
- **Ctrl-C mid-turn cancels gracefully.** The turn interpreter races every
  await against SIGINT. On cancel the running command is killed
  (`kill_on_drop`), unfinished tool calls get deterministic cancellation
  markers committed to the log, and history stays request-valid — so the
  next prompt simply continues from a clean, honest state instead of a
  400-ing one. In the screen front end the next prompt may already be
  written: lines typed while a turn runs are queued and the head runs when
  the turn ends, interrupted or not.
- **Approval gate and step cap.** With `--ask`, Bash/Edit/Write wait for an
  explicit y/N (Read never blocks); a denial is committed as a tool result
  the model reads and adapts to. Every turn is also capped at
  `machine::MAX_TOOL_STEPS` (500) tool-call steps so a looping model cannot
  burn tokens forever — the cap closes the turn with deterministic markers.
- **Crash healing at load.** A session interrupted between an assistant
  message with `tool_calls` and its tool results would otherwise produce an
  invalid request on resume. `Session::load` synthesizes deterministic
  placeholder results for the interrupted calls — in memory only; the file is
  never rewritten, and re-loading recomputes the same view byte-for-byte. The
  validity spec itself is executable (`machine::is_request_valid`), and
  bounded-exhaustive checks pin that every prefix of a valid history heals
  back to a valid one.
- **One answer is capped, the conversation is not.** `max_tokens` comes from the
  provider preset and is sent on every request; a short answer is unaffected
  (it is a ceiling, not a reservation). History is never trimmed: both backends
  take 1M tokens of context, and the prefix cache depends on replaying it
  byte-for-byte.
- **Prefix-cache friendly.** `SYSTEM_PROMPT` is a compile-time constant and
  history is replayed byte-for-byte — no trimming, reordering, or
  compaction. Injecting volatile data (time, cwd) or changing the tool set
  or its order invalidates the cache. The `tokens:` line after each turn
  reports `hit`/`miss` prompt tokens from `usage`.
- **Cache usage is normalized across providers.** DeepSeek reports flat
  `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`; GLM/OpenAI report
  nested `prompt_tokens_details.cached_tokens` (miss derived as
  `prompt_tokens - cached_tokens`). Both shapes land in the same hit/miss
  counters; a session that has reported nothing keeps the zero defaults rather
  than a fake-looking rate.
- **Token usage** is attached to the final content chunk of the stream, not
  to a separate SSE event.
- **Two levels of cache visibility.** Every sub-request prints a `tokens:`
  line (`in/total`, `hit/miss`, `out`); the status line aggregates `hit`/`miss`
  across the whole session.

## Session storage

Sessions live in `~/.caocli/sessions/<YYYYMMDD-HHMMSS>.jsonl`. Each line is
one JSON object tagged by `t`; `meta` lines override earlier ones on load.

```jsonl
{"t":"header","id":"20250101-120000","created_at":1735704000,"provider":"deepseek","model":"deepseek-flash","reasoning_effort":"high"}
{"t":"msg","message":{"role":"user","content":"check disk usage"}}
{"t":"meta","provider":"zai-coding-cn","model":"glm-5.3","reasoning_effort":"max"}
```

`provider` names the backend the model belongs to. It is optional: a session
written before it existed has none, and such a session runs on whichever
provider the run selected — which is what every session did then.

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
cargo run -- -p "what is 1+1"              # needs a key: /login stores one
python3 scripts/tui_smoke.py               # the full-screen front end, under a pty
python3 scripts/pty_smoke.py               # the plain prompt, under a pty
```

CI runs on every push to `master` and every pull request: fmt + clippy +
tests + line coverage ≥ 90% + dependency audit. Any red gate blocks merging.

[`AGENTS.md`](AGENTS.md) is the behavior guide for AI agents working in this
repo: the definition of done, verification channels, and the stable invariants
(prefix-cache discipline, API contract, design principles). It deliberately
omits code structure, which lives in the code itself.

## License

[MIT](LICENSE)
