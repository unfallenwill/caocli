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
- **The interactive front end needs a pty**; that is the only place it exists at
  all. `scripts/tui_smoke.py` is that pty, and its live section (`SMOKE_LIVE=1`) is
  the only automated route to a real turn, the running status line, the approval
  gate and the queue a line typed during a turn goes into. What a tool call did is
  asserted on disk rather than on screen: a model's own account of having run
  something reads the same as the thing itself. A *queued* line is asserted on the
  front end's own answer (a command, whose reply only the front end can write),
  for the same reason.
- **A screen drawn by difference does not write what is on it**, only what changed
  from the frame before: any assertion made on the byte stream is an assertion about
  the diff, and a character that happened to be in the same column already never
  appears in it at all. Read the screen back instead -- the smoke harness keeps a
  grid and applies cursor moves, erases and text to it.
- **A test that calls the handler directly does not show that the front end can reach
  it.** The approval gate's state machine was fully covered and green while the gate
  was unreachable from a running turn — the answer was dropped on the way in and the
  turn waited forever.

## Design principles (do not violate)

- **Deliberate minimalism**: one main loop. Providers are expressed as a static preset
  table — no dynamic registration, plugin systems, or other indirect layers; write it
  directly when you can. The traits a front end implements (`ui::contract`) are the only
  exception — they are not an abstraction layer but the machine's own vocabulary: what it
  notifies with, and the three questions it asks. There is one implementation per front end
  and the front ends are mutually exclusive (the plain prompt when stdout is not a
  terminal, or `--no-tui`, or a terminal that will not take raw mode; the interactive one
  otherwise), so the machine never chooses between them.
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
- **A cell's marker is columns of its own, not a prefix on its text.** It lives in
  `Cell::gutter`, so that the layer doing the wrapping is the one that sets in every
  line of a wrapped block — a prefix in the spans would set in the first line only, and
  the rest would come back to the left edge. Both front ends read the same gutter, and
  the plain one writes it for the block's first line alone because the terminal wraps
  the rest.
- **The answer is the only cell on the left edge.** `Cell::Content` is the one cell
  with no gutter; everything else is set in two columns behind a marker, so a reader
  can run their eye down column zero and find what the model said rather than what it
  did. A new kind of cell has to choose a marker, and the ones in use are a short
  vocabulary: `›` the user, `▸` a call about to run, `┆` thinking, `·` a result.
- **The input box is part of the same grid.** It draws the user's marker itself, from
  `cell::USER_MARKER`, and the editor is rendered into a field inset by
  `cell::MARKER_COLUMNS` — so a line being typed sits in the column the same line will
  sit in once it is submitted, and neither the marker nor that column may be carried in
  a placeholder string instead. The box's rules are drawn by the front end for the same
  reason: a block on the editor would be inset with it and the rules would stop short of
  the edges the transcript is read against.
- **Anything pinned to the bottom is paid for out of the transcript, and the box is the
  last to shrink.** A pinned region (the standing task list, the queue, the status line)
  takes rows the transcript does not get, and a box that is short by a row can still be
  typed in where a transcript with no rows left is a session you cannot read. A pinned
  region is measured off the lines it actually laid out, not off its item count: a task
  is as many rows as its words take, and a block whose height was guessed from the
  number of items is a block that either leaves a row blank or is silently clipped.
  What a cut list leaves out is counted, and the row a reader is waiting on — for a task
  list, the task in hand — is the one row that cannot be the one that gives way.
- **Ctrl-C during a turn is an out-of-band Command (graceful cancel), not a process
  kill**: the current effect is dropped (stream disconnected, child process
  `kill_on_drop`), unanswered calls are persisted with a deterministic cancellation
  marker, and the history still satisfies `is_request_valid`; cancellation is handled
  in the interpreter layer and never enters `next_action`.
  **The cancel channel is supplied by the front end, not built by the agent.** A front
  end owning the terminal runs it in raw mode, and raw mode clears `ISIG`: Ctrl-C then
  arrives as a key event and there is no SIGINT left to listen for (measured under a
  pty — `isig_after_raw=false`, the handler never fires). The listener must also exist
  before the turn's first await point: constructing one inside each `select` leaves a
  window between two `select`s in which a signal arriving exactly then is swallowed
  forever (tokio watch semantics, measured in the pty smoke test).
- **Turn step cap**: at most `machine::MAX_TOOL_STEPS` (500) tool-call steps per turn;
  exceeding it ends the turn with a deterministic marker — a product-level
  termination guarantee.
- **Tool results are always text and are never raised as hard errors**: failures are
  also passed back to the model so it can correct itself.
- **Read is a page, not a copy of the file**: one streaming pass per call — skip to the
  line the call asked for, keep the rows the page has room for, count the lines that
  follow — so a page of a huge file costs a page of memory, and the cap on a file's
  size is about how long the scan behind a page may take rather than about memory. The
  facts about a file's shape that decide whether an Edit will match (CRLF line endings,
  a last line with no newline) are read off the bytes of that same pass and said where
  they are known, rather than left for the model to guess. What cannot be paged at all
  — a directory, a fifo, a device — is refused before a byte is read: a fifo has no end
  to reach and no size to check, and a device can be endless.
- **A write lands on the file the path finally names, and leaves nothing behind**: a
  symlink at the end of the path is followed to its target rather than replaced by a
  regular file, an existing target keeps its own permission bits (the tmp file a rename
  hands over is a new one), content already on disk is answered as unchanged instead of
  rewritten, a directory or anything else that is not a regular file is refused before
  anything is created, and a write that fails removes its temp file. What is refusable
  is refused before a byte is written.
- **Approval gate**: execution is trusted by default; with `--ask`, Bash/Edit/Write
  require a user y/N before running (a call that changes nothing on disk is always
  allowed: reading, and writing the todo list). Denial, cancellation,
  an unanswered question, and the step cap all persist deterministic text markers that
  are passed back to the model so it can adjust on its own; the command echo is
  unchanged. The set of calls that are asked about is a blacklist, so a tool added
  later is asked about until somebody decides otherwise.
- **A question is answered, not executed**: `AskUserQuestion` is the one tool whose
  result is a person's. The interpreter reads its arguments, asks through the front
  end's `Ask` channel (`ui::contract`) and writes what was chosen back as the call's
  tool result — the same shape as the approval gate, with a value for a decision. It
  never passes the gate (asking permission to ask is asking twice about one thing),
  and its questions are the call's own arguments, so the transcript replays them
  exactly as they were asked. Dismissing it is not a hard error: the marker text is
  the result, and the turn carries on.
- **A front end that cannot take an answer declines rather than blocking**: a question
  asked where nobody is listening (an end of input, a front end that has gone away)
  is answered `None`, which the interpreter writes down as the unanswered marker —
  never a wait with no one to end it.
- **The todo list is a fold of the log, not state**: the tool writes the whole list
  every call, so what is standing is the arguments of the last such call and nothing
  has to be kept in step — a resumed session pins exactly the list the watched one
  did. It is executed like any other tool (nothing about its answer needs a person),
  it never passes the gate (asking permission to write down a note is asking about
  nothing), and both surfaces that show it go through the same marks and the same
  summary line, so they cannot say different things about one list. The one count that
  comes back unasked — a call that leaves two tasks in hand — is in the tool's answer
  and not in that summary line, which is a head line for a person who has the list in
  front of them.
- **Session logs are append-only and never rewritten**: a crash loses at most a
  trailing partial line, and corrupt lines are skipped when reading; interrupted tool
  calls are healed **deterministically** at load time (in-memory view only — the
  synthesized text must be a constant, otherwise the prefix cache becomes unstable).
- **Exactly one renderer per process**: streaming output and usage accounting must go
  through the same instance, otherwise counts are lost.
- **A provider has an id and a name, and they do not stand in for each other**: the
  id addresses it (flags, `/login <id>`, `<id>/<modelid>`, the session meta,
  `settings.json`); the name is what a person reads (the `/login` menu, an error
  message). Anything typed or stored takes the id, and the menus are the only place
  the two meet.
- **A secret the user types is answered, not stored**: an API key reaches
  `settings.json` (written by `/login`) and exists nowhere else — not in the
  transcript, the session log, the input history, or an environment variable. It is
  asked for as a question, so the front end hides it while it is typed and nothing
  that is submitted can carry it onwards; a configuration a second mechanism could
  also satisfy is one whose state nobody can see.
- **A front end that cannot take the terminal declines; it does not fail.** Raw mode
  and the alternate screen are process-wide, and a terminal that refuses either leaves
  nothing to draw on. Starting the session must survive that by falling back to the
  plain front end, and an attempt that fails halfway must undo what it did — including
  the alternate screen, which would otherwise hide everything the user had on it.
- **While a turn runs a front end accepts exactly four things**: the cancel key,
  the answer to an open approval question, the answer to an open question tool call,
  and the next line. The last is a queue, not a second turn: Enter takes the line out
  of the box and it runs when the turn in flight ends — from the head, interrupted or
  not. Everything else is dropped.
  The two answers are the ones that are easy to lose: a gate or a panel whose answer
  never arrives is indistinguishable, from the user's side, from a model that has hung
  — so while either is open the box is the answer's and nothing else is typed into it,
  and a line being composed when one arrives is held aside and given back.
  A question with options is answered with the arrow keys (or the digits, or space for
  a question that takes several) and Enter; Esc dismisses the call, which is not the
  same as answering it with the option the cursor happened to be on. The box stays the
  place an answer is typed: a panel that could only answer with the options it was
  given is a panel that cannot ask an open question.
- **Anything that moves on its own must be derived from the clock, never from the
  number of redraws**, and that clock must be part of whatever decides whether to
  redraw at all. A front end that skips an unchanged screen otherwise freezes its own
  animation, and a frame counter can be left behind by a redraw that never happened.
- Truncating or clipping text must land on a UTF-8 character boundary, and any text
  measured against a terminal width must be measured in **display columns**, not
  chars: a CJK ideograph or an emoji is one char but two columns, so char-based
  arithmetic silently overruns the field it was sizing.
- Text that is committed to the terminal instead of being written to a scrolling stream
  must be **wrapped, never clipped**: a fixed-width rendering path cuts an over-long
  line off, where the terminal's own soft wrapping would have kept every column. The
  plain front end may leave wrapping to the terminal; a front end that draws into a
  region may not.

## Prefix cache (must read before changing request or message construction)

The backend matches its cache on the exact request prefix; the hit rate directly
determines cost and latency:

- The system prompt is a fixed constant — it is **forbidden** to inject time, cwd,
  random ids, or any other dynamic content.
- History messages are **replayed byte-for-byte**: no trimming, no reordering, no
  clipping, no compression, no normalization.
- Changing the tool set, the tool order, or any tool schema text (names,
  descriptions, parameter descriptions, parameter keywords) changes the prefix and
  causes a full cache miss — only do it deliberately.
- Any change that "optimizes the history" equals a full cache miss.
- An **attached image is part of the message**: it is stored as the `data:` URL the
  request carried and replayed with it. Reading the file again, or re-encoding the
  bytes, is a different string and so a full cache miss — and the file may not even
  be there any more. Nothing about an image may be re-derived from disk; what the
  message says about itself is also all the transcript may show.
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
- Keep the task list live. It is the only view of a turn's work the user has while it
  runs, so write it before the first change, mark each task completed as it lands rather
  than in one batch at the end, and keep at most one task in progress. A step that is
  worth more than a line still gets its list update first: the reader is watching the
  list, not the diff.
- When behavior is uncertain, first confirm it with a `-p` or pty smoke run, then draw
  conclusions.
- Do not abstract for "we might need it later"; the value of this project is being
  small and direct.
