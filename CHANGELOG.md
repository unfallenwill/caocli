# Changelog

All notable changes to caocli are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project tags
semantic-version bumps per release.

## [Unreleased]

## [0.1.6] — 2026-09-14

### Changed — the transcript is one cell per tool call

- A tool call is one `Step` cell now, rather than three cells (the
  `ToolCall`, its `ToolOutput` children, and the `ToolResult`). Three
  cells for one thing made the lifecycle a property of the renderer, so
  the start, the children and the verdict could disagree on shape. The
  cell carries the verb and its subject, the children as they stream,
  and the verdict; a settled success folds to one line
  (`✔ Bash ls · exit_code: 0`), and a failure auto-expands the body that
  caused it, so the reason is visible without a toggle. Edit and Write
  keep their diff in both modes, because the change is what the call is
  for. Live rendering and replay fold the same messages into the same
  steps, so a resumed session reads back what the session drew.
- `Ctrl-O` toggles verbose: a settled-Done step shows its children when
  the flag is on, Running and Failed are unchanged (their children are
  the verdict), Denied never. The laid cache is keyed on the flag
  alongside the width, so flipping it re-wraps every cell.
- The model and the reasoning effort tier move out of the pinned row to
  a metadata row above each user prompt (`deepseek-v4-pro · effort max`),
  pushed on submit and on replay, so a session that switched models
  mid-history reads the way the user asked it. The pinned row, and the
  plain front end's status bar with it, are cache-only from here on --
  the cache counts are the one segment that really is session-level --
  and a new `/debug` command prints the model, the effort and the
  provider. The plain front end echoes the same metadata line before
  each turn, having no transcript to carry a row.
- The border above the box says what the agent is doing: `thinking`
  while a reasoning block streams, `running Bash` (or whatever verb)
  while a tool call is in flight, `working` otherwise. The spinner
  character and the elapsed seconds are unchanged; only the word between
  them moves.

### Changed — UI layer refactor
- The plain front end's writer half is split into its own module
  (`ui::plain_writer`). `Renderer` now holds a `PlainWriter` for cells and
  keeps only the status bar, status line and raw-mode flag -- the
  responsibilities that belong to a session rather than to a writer. The
  `Front`/`Ui` surface is unchanged.
- The `Notice` channel is split into three typed channels: `MachineNotice`
  carries the model's vocabulary, `AppNotice` carries the application's,
  and `SecretAsk` carries the secret prompt's oneshot question. The
  machine cannot accidentally be told about an `Info` line and vice
  versa, because each receiver's type is the only thing it can read.
- `State` is split into four pieces -- `View` (cells, scroll, laid
  cache, status line), `Edit` (textarea, history, held draft), `Overlay`
  (picker, panel, answer channel), and `Turn` (queue, running flag,
  speed-estimate counters) -- with the revision counter on `State` itself
  since every half can change what the screen shows. Methods are split
  across files by the half they touch.
- The word-wrap algorithm (`wrapped_lines`/`break_line`) and the
  row-budget constants (`TODO_ROWS`, `QUEUE_ROWS`, `TODO_HEADS`,
  `Window`, `selection_window`, `todo_window`) move from
  `ui::paint`/`tui::layout` into `ui::cell::wrap` and `ui::cell::layout`.
  A front-end-specific layout (TUI box geometry, screen-row split)
  stays in `ui::tui::layout`.
- `Span::text` becomes `Cow<'static, str>`: a literal marker can carry
  no allocation, while constructed text is owned as before. The painter
  still copies every span while wrapping (the wrap step's lifetime is
  independent of the spans); the data-layer shape is what changed.
- `Cell::tool_call` becomes the simple `Cell::ToolCall` builder; the
  `AskUserQuestion`/`TodoWrite` dispatch moves to `Cell::from_tool_call`
  so the cell layer no longer needs to know about specific tool names.
- `working.rs` is split: `panel_key` lives with `panel.rs`, `enqueue`/
  `dequeue` live in a new `queue.rs`, and what remains is just the
  turn-in-flight key routing.

### Changed — the startup phase, the front ends, and the MCP lifecycle

- `main::run` reads as resolve-then-dispatch now: `startup::resolve_startup`
  bundles the session, provider, client, approval gate, API key and mode
  into one `Startup`, and what is left in `main` dispatches on it. The
  decisions that used to be implicit in a chain of `if`s are types --
  `Mode` (list sessions / one-shot / interactive), `SessionSource`
  (resume / continue latest / fresh, with the priority in `from_cli`) and
  `ApiKey` (present / missing, with the hint a banner wants).
- `front::FrontEnd` owns the two shapes a run can take. `OneShot` and
  `Interactive` each carry the data their shape needs (banner, history,
  status-bar flag) and their own teardown, and the plain prompt's read
  loop moved in with them. A fifth front end is one file and one match
  arm, rather than another branch threaded through `main`.
- MCP connections are closed by `McpGuard`'s `Drop` rather than by a
  `shutdown()` call at each exit. The old discipline was load-bearing:
  any exit that forgot the call leaked child processes, and the panic
  path had already forgotten it. The guard hands the agent a shared
  handle and spawns the async close on the runtime when it drops.

### Changed
- The plain REPL's input loop is now built on crossterm raw mode and a
  `ratatui_textarea::TextArea` used as a pure in-memory buffer: every
  keystroke is delivered as a `KeyEvent`, the renderer redraws the prompt
  in place, and there is no longer a kernel line-discipline buffer to drain
  between turns. The previous design relied on `rustyline` and a
  non-blocking read of stdin to recover lines submitted while a turn ran;
  in raw mode the editor owns the tty, so what is not consumed has not been
  typed. `repl::Queue` is gone, and `rustyline` is no longer a dependency.
  Tab completion of slash commands is removed in this change: the plain
  prompt never had the visual context the TUI's picker relies on, and
  the source-of-truth `repl::completions` is still the one the TUI uses.
  The `/login` secret prompt now reads key-by-key in raw mode (with a
  `•` mask) instead of toggling the terminal's echo in cooked mode.
- Status line's cache segment now reads `N/M` instead of `hit N · miss M`
  (e.g. `cache 98.6% · 32384/461`). The two counts are read as a fraction
  the way other tools report cache hit/miss, and the line is six columns
  shorter.
- The approval gate's y/N rule now lives in one place
  (`ui::answers::allows`): "Y", "Yes", "yeah" all allow on both the plain
  prompt and the TUI. Before, the plain prompt only allowed "y" or
  lowercase-prefixed lines.
- `/help` shows the input keys the current front end actually accepts:
  the plain prompt lists `Ctrl-C` clears the line, `Up/Down` browse
  history; the TUI lists `PageUp/PageDown` and the mouse wheel for
  scrolling the transcript, `Esc` for dismissing the menu, and so on.
  The command list and the startup flags are still shared.

## [0.1.5] — 2026-09-13

### Added — MCP (Model Context Protocol)

caocli now speaks MCP, both transports the specification defines
(revision 2025-06-18):

- **stdio** servers: started as child processes, JSON-RPC framed one
  message per line, the standard input closed on shutdown.
- **streamable HTTP** servers: `POST` with `application/json` or
  `text/event-stream` responses, the `MCP-Protocol-Version` and
  `Mcp-Session-Id` headers, `DELETE` to end the session.

A server's tools are offered to the model beside the built-in ones,
named `mcp__<server>__<tool>`. Two files configure servers:

- `~/.caocli/settings.json` under `"mcpServers"` for the ones you want
  in every workspace;
- the workspace's `.mcp.json` for the ones a project ships (the file
  other clients read; the project wins for a name both files define).

`${VAR}` and `${VAR:-default}` are expanded when the file is read. A
reference with neither resolves to a warning and the text stays as
written, so a value turned into nothing is never what starts a server.

The new `/mcp` REPL command lists the servers, what they call
themselves, the protocol version both ends settled on, and the names
the model sees.

### Changed

- `Agent` now holds an `Arc<Hub>` over the MCP servers this session
  connected to. Tool dispatches go through it. Tool lists are appended
  after the built-in ones so a session's earlier requests stay a prefix
  of its later ones.
- `tools::execute_live` carries the hub, so a call to a server's tool
  is dispatched like any other.

### Implementation notes

- The MCP client declares no capabilities of its own (no roots, sampling
  or elicitation). A server that asks is refused by name rather than
  left waiting.
- `--ask` applies to MCP tools as well: what a server does with a call
  is the server's own, and the spec's own note about annotations is why
  the server's word does not settle it.
- A server that does not come up costs nothing else: the session runs
  with the servers that did, and `/mcp` says which is missing and why.
- OAuth 2.1 (dynamic client registration, PKCE, RFC 8707 resource
  parameters) is not implemented. HTTP servers that need it accept a
  static Bearer token in `headers`; servers that require the dance
  will refuse a session without it.
- JSON-RPC batching, removed from the spec in 2025-06-18, is not sent
  or parsed.
- Server-emitted notifications (`logging`, `progress`, `list_changed`,
  `cancelled`, `message`) are read past silently. Spec marks these
  MAY/SHOULD rather than MUST on the client side; nothing this client
  does depends on any of them.

## [0.1.4] and earlier

Pre-0.1.5 history is not recorded here. See `git log` for the
changes between earlier releases.