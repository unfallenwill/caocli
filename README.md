# caocli

[![CI](https://github.com/unfallenwill/caocli/actions/workflows/ci.yml/badge.svg)](https://github.com/unfallenwill/caocli/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A minimal terminal coding agent in Rust, backed by an OpenAI-compatible
`/chat/completions` API: DeepSeek by default, Z.AI's GLM coding endpoint via
`--provider zai-coding-cn`. It streams the model's thinking (`reasoning_content`) in
dim gray, then runs a tool loop over six tools: `Bash`, `Read`, `Edit`,
`Write`, `AskUserQuestion` (which asks you rather than the filesystem) and
`TodoWrite` (which writes the plan where you can see it while it works). Both backends see images, which are attached with `/image` or
`--image` and travel inside the message itself. Sessions are append-only JSONL
logs under `~/.caocli/sessions/`, resumable across runs and replayed
byte-for-byte so the backend's prefix cache keeps hitting.

## Requirements

- Rust 1.85+ (edition 2024)
- An API key, which is stored by `/login` (see below)

## Quick start

```bash
cargo run --                            # interactive REPL: /login, then chat
cargo run -- -c -p "check disk usage"   # one-shot, continuing the latest session
cargo run -- --effort max --model deepseek/deepseek-v4-pro -p "..."
cargo run -- --provider zai-coding-cn -p "1+1"   # Z.AI Coding CN, glm-5.3-flash
cargo run -- -p "what is wrong here?" --image shot.png
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
| `/effort` | Choose the reasoning effort tier — the list is the provider in use's own (`low`, `high`, `max`), with the one in effect marked; the choice is stored in the session, so a resume keeps it |
| `/image <path> [text]` | Ask about a picture: the image is read and sent with the text that follows the path (none is fine). A path with spaces in it may be quoted with `"` or `'` |
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

The reasoning effort is the provider's list too: `/effort` offers the tiers the
provider in use accepts, and a tier is checked against that list before it is
stored — the same check `--effort` passes at startup, so neither route can put a
value in the session the backend would reject or quietly ignore.

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
| `--image <PATH>` | Attach an image to `-p`'s prompt; repeat for more than one. In an interactive session, `/image` is how one is attached |
| `--provider <NAME>` | Backend provider: `deepseek` (default) or `zai-coding-cn`. Settles the endpoint of a new session; a resumed session keeps the one its meta names |
| `--model <MODEL>` | Model id as `<provider>/<modelid>`, or bare for `--provider`; defaults to the provider's default model |
| `--effort <EFFORT>` | Reasoning effort: `low`, `high`, or `max` (default `max`); other values are rejected locally. `/effort` switches it inside a session. On GLM, `low` answers without emitting `reasoning_content`. |
| `-c, --cont` | Continue the most recent session |
| `--resume <ID>` | Resume a specific session by id |
| `--list` | List sessions and exit |
| `--ask` | Approval gate: ask y/N before Bash/Edit/Write (Read always allowed, as is `TodoWrite`, which changes nothing; a question reaches you either way). Denials are recorded as deterministic markers the model can see and adapt to. |
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
rows are the pinned region: the standing task list when there is one, the queue,
the input box, and under it the status line.
The box is as tall as what is in it — `Ctrl-J` adds a line and room for it — and
the transcript takes those rows back when the line is submitted. It is ruled off
above and below rather than boxed in, so a line of the session and a line being
typed start in the same column.

```
▸ Read Cargo.toml
ok: package.name = caocli (312 bytes)

› run the tests and fix what fails
› and bump the version

· todos · 1/3 done
✔ Read the failing test
▸ Fix the parser
☐ Bump the version
─────────────────────────────────────────⠸ 12s · ~38 token/s
›  the turn is running · Enter queues · Ctrl-C stops
──────────────────────────────────────────────────────────────────────────
zai-coding-cn/glm-5.3 · effort max · cache 95.3% · hit 846912 · miss 41538
```

- **The status line** is the row under the box, always the session summary:
  the model, the reasoning effort tier, the cache hit rate, and the raw hit/miss
  counts. A new session opens with the defaults (`cache 0.0% · hit 0 · miss 0`)
  and the counts move as the provider reports usage; what a turn is doing is the
  transcript's to say, not the status line's.
- **While a turn runs, the box's top border says so**: a spinner and the seconds
  it has run, and — once the turn has lasted long enough for an average to mean
  anything — an estimated `~N token/s`. Estimated, because the provider reports
  tokens only at the end of a sub-request; the characters-to-tokens ratio the
  estimate runs on is one this session has measured from its own earlier
  sub-requests. A narrow border drops the estimate whole before it hides the
  indicator, and a question standing over the box (the approval gate) takes the
  border back. The exact figure lands where records belong: the plain front end
  appends `N token/s` to each usage line it prints.
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
- **A question is answered from the panel it puts up**, or by typing in the box;
  see [Answering a question](#answering-a-question). While one is open the box
  belongs to the answer, exactly as it does while the approval gate asks.
- **The task list stands above the box** for as long as the model keeps one, so a
  long piece of work can be watched without scrolling: what it planned, and which
  part it is on. See [The task list](#the-task-list).

`--no-tui` keeps the plain prompt instead (it is used anyway when stdout is not
a terminal), which is the front end the status bar below belongs to.

### Status bar

In the plain REPL (`--no-tui`), a status bar pinned to the bottom line shows the active model,
the reasoning effort tier and the session's cumulative cache hit rate, right-aligned:

```
deepseek/deepseek-flash · effort max · cache 98.6% · hit 32384 · miss 461
```

The model segment names the model by its provider; the effort segment is the
tier the next request carries. Both follow the session, so they are refreshed on
`/new`, `/resume`, `/model`, `/effort` and CLI overrides.

It appears only when stdout is a TTY and the terminal has at least 3 rows;
`--no-status-bar` turns it off. The bar reserves the last terminal line via
a scroll region, so output scrolls above it — the trade-off is that lines
scrolled out of the region do not enter the terminal's scrollback buffer.
Stats reset when you switch sessions (`/new`, `/resume`) or models
(`/model`), and the bar is restored on exit.

### Images

Both backends take images. `/image <path> [text]` is one turn about a picture:
the file is read, and the message that goes to the model is the text followed by
the image. With no text the picture is all there is — the model describes it, and
the questions that follow are ordinary lines, because the image stays in the
history and the model can look at it again.

```
› what does this error say?
  [image png · 48213 bytes]

┆ Let me read the screenshot.
The dialog says ...
```

The transcript names the format and the size, never the path or the bytes: what a
message says about itself is what a resumed session can show, and the path is not
part of what was sent. PNG, JPEG, WebP and GIF are recognized **by their bytes**,
not by the file's extension, and at most 10 MiB may be attached.

An attached image is carried in the message itself as a `data:` URL, so the
session log is the whole of it: a resumed session replays the same bytes it sent
the first time — the prefix cache goes on matching — and follow-up questions work
even if the file has since been moved or changed. `--image` is the one-shot
form: `caocli -p "what is wrong here?" --image shot.png` (repeat it for several
images).

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
allowed, and up to the backend to accept or reject. The effort tiers are the
same table's: `/effort` offers exactly the list `--effort` is checked against.

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

The model can call six tools. Tool results are always plain text: failures
are returned to the model as text so it can recover, never as a hard error.

| Tool | Behavior |
|---|---|
| `Bash` | Run one `bash -c` command. 120s timeout; stdout and stderr are each truncated to 10 KiB. |
| `Read` | Read a UTF-8 text file, one numbered row per line. `offset`/`limit` page through a long file, and the file is streamed: a page costs one page of memory however large the file is, and the marker after the last row says which lines were shown, how many lines the file has, and where to read on. A page whose lines end with CRLF says so, and so does a file whose last line has no newline after it: the two things about a file's shape that decide whether an `Edit` will match. |
| `Edit` | Replace `old_string` with `new_string`; `old_string` must match exactly once. Written atomically via tmp + rename. |
| `Write` | Create or fully overwrite a file; parent directories are created automatically. The write lands on the file the path finally names: a symlink is followed to its target rather than replaced by a regular file, and an overwrite keeps the target's own permissions. The text lands in the target's own line endings, so a whole-file rewrite in the other ones is not a change to every line of the file; a file being created, one whose endings are mixed, and one over 10MB are written exactly as they were sent. Content that is already there is left unchanged, and a write that fails takes its temp file with it. |
| `AskUserQuestion` | Ask you to choose: up to four questions, each with up to four options. The answer comes back as the call's result (`<id>: <chosen label>`), so the model continues with what you picked. |
| `TodoWrite` | Record the plan as a list of tasks, up to 20. The whole list is sent every call and replaces the one before it, so each call is the state of the work rather than a change to it. |

Limits: 10 KiB of output per tool result, 10 MB per file written or edited. A read
streams and holds only the page it answers with, so what bounds it is how long the
scan behind a page may take: 256 MB, past which a range of the file is a `Bash` call.

### The task list

`TodoWrite` changes nothing on disk and needs nobody to answer it — writing the
list down *is* the result — so it never passes the approval gate.

The list is a fold of the session log rather than state of its own: what is
standing at any moment is the arguments of the last call, which is why a resumed
session shows exactly the list the live one did. It is drawn in two places, out
of those same arguments:

- **In the transcript**, as the call's own cell: a head line saying how far the
  list has got, then one line per task — `☐` not started, `▸` in hand, `✔` done.
- **Above the input box**, in the screen front end, for as long as the list
  stands: the transcript gives up the rows, and the box gives up rows to it when
  the screen is short. A list too long for the block follows the task in hand and
  counts what it left out at each end, so the one row you are waiting on is never
  the one that was cut. Sending an empty list clears it, and the block goes.

One task is meant to be in hand at a time, and the call's result says so when two
are: `todo list updated (1/3 done; 2 in progress)`. That count is the one thing
the model is told that it could not read off its own call, and it stays out of the
two drawings above, which show the list itself.

### Answering a question

`AskUserQuestion` is the one call whose result is yours, and it is answered
wherever the answer is typed:

- **The screen front end** puts a panel over the transcript: `↑`/`↓` (or the
  digits) move the cursor, `Space` toggles an option for a question that takes
  several, `Enter` confirms and moves to the next question, and `Esc` dismisses
  the whole call — which the model is told, so it can ask again or go on without
  an answer. Typing in the box answers in your own words instead, which is the
  only way to answer a question that offers no options.
- **The plain prompt** (`--no-tui`, or when stdout is not a terminal) prints the
  questions and reads one line per question: a number, an option's label, or a
  comma-separated list of them for a question that takes several. An empty line
  leaves that question blank, and an end of input leaves the call unanswered
  rather than hanging.

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
  explicit y/N (Read never blocks, and neither does `TodoWrite`: a question in
  front of a call that cannot go wrong is a question that teaches you to answer
  without reading); a denial is committed as a tool result
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

CI runs on every push to `main` and every pull request: fmt + clippy +
tests + line coverage ≥ 90% + dependency audit. Any red gate blocks merging.

[`AGENTS.md`](AGENTS.md) is the behavior guide for AI agents working in this
repo: the definition of done, verification channels, and the stable invariants
(prefix-cache discipline, API contract, design principles). It deliberately
omits code structure, which lives in the code itself.

## License

[MIT](LICENSE)
