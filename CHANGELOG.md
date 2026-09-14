# Changelog

All notable changes to caocli are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project tags
semantic-version bumps per release.

## [Unreleased]

### Added — session file format v1

The session log gains an envelope (`type`, `sequence`, `timestamp_ms`,
`turn`, `kind`, `source_sequences`, `ignorable`) and an event vocabulary
that records the wire trace of every request: `request_prefix`,
`request`, `reply`, plus `gate`, `call_end`, `stop` for the harness.
The schema is documented in `docs/session-format.md`.

- Every `request` event carries a `body_sha256` over the canonical
  (RFC 8785) JSON of the body the agent sent. A reader rebuilding the
  body from the trace recipe and the prefix events hashes to the same
  digest — the end-to-end check that the schema stayed faithful.
- `--migrate <id>` lifts a v0 file to v1 by writing a new file beside it;
  the original is never touched. `--migrate --all` does the same for
  every v0 file in the sessions directory.
- `canonicalize()` and `canonical_sha256_hex()` (in `src/canonical.rs`)
  are the wire-fidelity primitives the trace builds on.

### Changed — defensive hardening of session files

- New session files are created with mode `0600` (was the process umask,
  `0644` in practice). A session log holds commands, their output and
  pasted secrets — it is the owner's file.
- `--resume <id>` rejects ids containing `/`, `\`, `..`, `..` segments,
  `%`, NUL bytes, or the empty string. A bad id would have let a
  resume reach outside `sessions_dir`; it now fails at the point the
  path is built.

### Changed — session files live under per-workspace directories

- `~/.caocli/sessions/` now contains a subdirectory per working
  directory (`<escaped cwd>/`), so `--continue` means "the most
  recent session of *this* workspace" and `--list --all` shows
  every workspace's history. A session is byte-for-byte the same
  shape regardless of which workspace it ran in — the change is
  entirely in where the file is filed.
- `--resume <id>` looks in the current workspace first and walks
  every other workspace on a miss, so a session named by its stem
  is still reachable across workspaces. A collision is the first hit
  wins; `--list --all` is how a script disambiguates.
- `--list --all` is the new "every workspace" form of `--list`. The
  default (`--list` without `--all`) is this workspace only.
- `--migrate --all` walks every workspace under `~/.caocli/sessions/`
  and lifts v0 files in each. Same contract as before: the original
  is never touched.
- The escape helper (`escape_working_directory` /
  `unescape_working_directory`) is now part of the hot path; it is
  documented in `docs/session-format.md`. `%` is reserved in `--resume`
  ids because it is the escape character.

### Fixed

- Tests in `repl.rs` and `session.rs` that touch HOME/PWD now share a
  process-wide `RUST_TEST_THREADS=1` runner. The CI workflow sets
  this explicitly; local development can override.

### Added — runtime control over MCP servers

- The session used to connect its MCP servers once at startup and tear them
  down on exit; the only verbs were "report" and "no, really, what did the
  server say". Five new methods on `Hub` give the session a real control
  surface, all exposed through `/mcp` subcommands:

  | Subcommand | Effect |
  |---|---|
  | `/mcp list` | One row per server: name, state (`ready` / `failed` / `disabled` / `disconnected`), count of tools currently offered |
  | `/mcp disable <name>` | Park a ready server; its tools leave the offered set; calls to them come back with state-aware errors |
  | `/mcp enable <name>` | Reopen a disabled server with the stored configuration |
  | `/mcp reconnect <name>` | Reopen any server regardless of state; the operator one-stop for "try again" |
  | `/mcp disconnect <name>` | Close a ready server's connection without touching its configuration |

  `disable` and `disconnect` are distinct: the first is for "I do not want
  this server at all right now" (config kept on file, easy to bring back),
  the second is for "I want to take this connection down but keep the
  setup intact". State is in-memory only -- nothing is written back to
  `.mcp.json`, and the next session starts with whatever the file says.
  A call to a tool whose server is `disabled` or `disconnected` returns
  `error: … is disabled; enable it with /mcp enable <name>` (or the
  `disconnected` equivalent) rather than "no such tool", so the model
  knows what changed.

### Changed — MCP client is its own workspace crate

- The MCP layer used to live at `src/mcp/*` as a sibling of the rest of
  the agent. It moved to `crates/caocli-mcp/`, alongside `caocli-core`
  (which now holds `ToolDef` and `FunctionDef` -- the types the hub
  builds and the agent's request builder consumes, the same on both
  sides of the crate boundary). The change makes one thing explicit
  that was implicit: the binary owns the user's settings file, not the
  protocol layer. `Hub::connect` takes the already-extracted
  `mcpServers` value (`Option<&Value>`); the binary reads it from
  `settings.json` before connecting. A missing or unreadable settings
  file is not the hub's problem; the hub gets `None` and carries on.
  Behaviour is otherwise unchanged.

### Changed — Hub is an actor

- The `Hub` used to be a struct with borrowed access from `tools`,
  `repl`, and the agent. Adding `enable`/`disable`/`reconnect`/`disconnect`
  on top of that would have meant either wrapping the inner state in a
  `Mutex` (sharing mutable state across call sites is exactly what
  Rust warns against) or infecting every call site with `&mut Hub`
  (and the borrow-checker failures that come with it). The hub is now
  an actor: a thin `mpsc::Sender` wrapping, the actor task owns the
  only `HubInner`, and every public method sends a `Command` and
  awaits the reply. There is no shared `&mut`, no lock, and the type
  signature (`async + &self`) tells the whole story.
- `definitions()` now returns `Arc<Vec<ToolDef>>` rather than
  `&[ToolDef]`. The request builder calls `.as_slice()`; the `Arc`
  means the caller can hold the result across turns without re-asking
  the actor.
- The tool order is kept as a per-server list
  (`BTreeMap<String, Vec<String>>`) rather than a flat `Vec<String>`,
  so `enable`/`disable` mutate one server's block without touching
  the others and the rebuilt order stays deterministic. The order is
  part of the request prefix and affects KV-cache stability.
- Two sync tests that constructed an `Agent` (`effort_label_reads`,
  `prompt_metadata_pairs`) became `#[tokio::test]` -- the actor model
  requires a runtime, and `Hub::empty()` is what surfaces that
  requirement.

### Added — render markdown in the answer and the thinking block

- The screen and plain front ends used to lay the model's output down as
  raw text: a heading rendered as `# Title`, a list as `- one\n- two`,
  a fenced code block as a fence line of backticks followed by the body.
  Both front ends now parse a CommonMark-flavoured fragment (paragraphs,
  emphasis, strong, strikethrough, inline code, fenced and indented
  code blocks, headings, ordered and unordered lists, task-list
  markers, block quotes, thematic rules, links, images, hard and soft
  line breaks) and emit the styled spans the cell layer already speaks:
  bold runs paint yellow, code runs paint dim, headings lead with `##`,
  list items with `- ` or `1. `, blockquote lines with `> `. Tables fall
  back to the indented raw source, because column-aligned rendering is
  the one thing our wrap step cannot do and pretending otherwise would
  draw a worse table than the source already is. The parser lives at the
  cell layer (`src/ui/cell/markdown.rs`), so both front ends, live
  streaming and session replay, see the same answer to the same source.
  HTML, footnotes and math are dropped silently -- emitting the raw
  bytes would put angle brackets on the screen.

### Changed — system prompt tells the model what the TUI renders

- The system prompt used to be one paragraph: the role, the routing
  rule, and a `Keep answers concise` reminder. The model defaulted to
  GitHub-Flavored Markdown regardless, so a heading or a code block
  landed on the screen as raw markers when the cell layer had not been
  told how to read them. The prompt now carries a short `Formatting:`
  paragraph that says: use GFM where it makes the answer easier to
  scan; reserve lists, fenced code blocks, and headings for substantive
  answers and leave short replies as plain sentences; inline commands,
  file paths, and env vars between backticks; the TUI renders fenced
  and inline code, emphasis, lists, blockquotes, and headings, tables
  show as raw indented markdown, HTML and footnotes do not render so
  emit plain text instead. The two frozen wire-prefix snapshots
  (`the_request_prefix_is_frozen` and
  `the_anthropic_request_prefix_is_frozen`) were updated to match: by
  design they fail on any change to the system prompt, so the cache
  contract is revisited every time the wording moves.

### Changed — the transcript fills the terminal

- The screen front end used to lay every line out to at most 100 columns,
  whatever the terminal's width. On a wider terminal the transcript
  stopped short of the right edge while the input box's rules and the
  status line ran the full width, so the text read as a column wedged
  into the left of the screen. A line now breaks at the region's edge and
  nowhere else; the standing task list, the queue waiting to run and the
  question panel were capped by the same call and are uncapped with it.
- The label a tool call is drawn under is no longer cut at 80 columns
  either. A call the summariser cannot name -- an MCP tool, arguments that
  are not JSON -- used to show the first 80 columns of its arguments and
  drop the rest without a mark; it now shows all of them, wrapped by the
  region like every other long line. `HINT_COLUMNS` is gone.

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