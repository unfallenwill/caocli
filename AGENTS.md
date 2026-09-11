# caocli — agent behavior guide

This repository is developed and maintained by AI agents, and this file is written
for them. It holds what cannot be worked out from the code, the README, or general
Rust and git experience: the gates a change must pass, the channels it is verified
through, and the invariants that are expensive to break.

It is not a description of the code. Structure, responsibilities, signatures and
values belong to the code, and a copy of them here is a copy that goes stale. A name
appears below only where a rule needs an anchor to point at, and never with the value
that name owns: this file says `machine::MAX_TOOL_STEPS`, not a number.

One fact, one home: how to run and verify → here; what the program does and how to use
it → `README.md`; why the code is shaped as it is → the comments beside it. A rule
that can be executed belongs in a test, and this file points at the test rather than
paraphrasing it.

## Definition of done

Any change must pass the following in order before you consider it done:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo llvm-cov --fail-under-lines 90   # line coverage gate
```

- CI runs all four on every pull request and on pushes to `main`, and additionally a
  dependency security audit; any red blocks merging.
- The coverage gate needs `cargo-llvm-cov` and `llvm-tools-preview` installed. A gate
  that cannot be run has not been passed.
- Commit in small steps, in Conventional Commits (`feat(scope): ...`,
  `fix(ui): ...`), English. Every commit must pass the gates on its own.

## Language

Everything committed here is English: code comments, doc comments, identifiers, test
names, assertion and panic messages, user-facing CLI text, the text sent to the model
(tool names, tool descriptions, parameter descriptions, tool results, synthesized
markers), documentation, commit messages.

Two things are not language, and are not defects: a glyph that is UI furniture (a cell
marker, an arrow, a typographic dash) is not text; and test data whose *purpose* is
multi-byte or display-width handling is the one place another script belongs — a CJK
ideograph is the clearest case there, one character and two columns. Chinese prose,
comments, identifiers or messages anywhere else are defects.

## How to verify

Unit tests are not evidence that a change works: this is a program that talks to a
network API and drives an interactive terminal.

- After a change, run `cargo test`, then one smoke run through one-shot mode against
  the real API (`-p`) — that is the agent's primary self-test channel.
- Pure logic (parsing, stream aggregation, truncation, layout arithmetic) is unit
  tested; boundaries with side effects (network, terminal, files) are what the smoke
  runs are for.
- **Terminal behavior only exists under a pty**, and a test's stdout is a pipe, so
  TTY-only branches (the status line, key bindings, escape sequences) never execute
  in one. Two harnesses are that pty: `scripts/pty_smoke.py` for the plain front end
  (its status line, Ctrl-C cancellation, `/login`'s hidden answer) and
  `scripts/tui_smoke.py` for the front end that owns the screen.
  `script -qec "..." /dev/null` is the ad-hoc version of the same thing when a
  one-off run is what is wanted — and **rebuild before you use any of them**: a smoke
  run against a binary older than the change is a run of the code before it, and it
  reads exactly like a bug in the change.
- `scripts/tui_smoke.py`'s live section (`SMOKE_LIVE=1`) is the only automated route
  to a turn *on screen*: the running status line, the approval gate, the queue a line
  typed during a turn goes into, and a running command's own output reaching the
  screen while the command is still running — which is proved by a command that prints
  one token now and another after a wait, neither of them in its own text, since the
  call puts that text on the screen twice over. `scripts/pty_smoke.py` runs a live
  turn of its own — request, tool call, Ctrl-C — on the plain front end.
- **What a tool call did is asserted on disk, not on screen**: a model's own account
  of having run something reads the same as the thing itself. A *queued* line is
  asserted on the front end's own answer (a command, whose reply only the front end
  can write), for the same reason.
- **A screen drawn by difference does not write what is on it**, only what changed
  from the frame before: any assertion made on the byte stream is an assertion about
  the diff, and a character that happened to be in the same column already never
  appears in it at all. Read the screen back instead — the harness keeps a grid and
  applies cursor moves, erases and text to it.
- **A test that calls the handler directly does not show that the front end can reach
  it.** The approval gate's state machine was fully covered and green while the gate
  was unreachable from a running turn — the answer was dropped on the way in and the
  turn waited forever. Exercise the path a keystroke takes, or say what was not
  exercised.

## Design principles (do not violate)

- **Deliberate minimalism**: one main loop, written directly. Providers are a static
  preset table — no dynamic registration, no plugin system. The traits a front end
  implements (`ui::contract`) are the one exception, and they are not an abstraction
  layer but the machine's own vocabulary: what it notifies with, and the questions it
  asks. One implementation per front end, and the front ends are mutually exclusive,
  so the machine never chooses between them.
- **The machine decides, never executes**: turn flow is a pure fold over the persisted
  history (`machine::next_action`); the interpreter executes what it returns and
  writes the result back. To add behavior to the loop, extend the vocabulary first —
  the event table lives in the `machine.rs` module comment — and never stuff implicit
  state into the loop.
- **Callbacks receive notifications only, never return data**: a `Ui` implementation
  must not block and must not decide. The questions that do need an answer — the
  user's cancel, the gate's verdict, the question tool's choice — are asked on
  channels of their own (`ui::Cancel`, `ui::Approve`, `ui::Ask`) and read by the
  interpreter, which decides what to run, never what comes next: what the decision
  function sees is the result, in the log, like every other tool result.
- **State = a fold of the log**: `Session` is the only persistent state, rebuildable
  at any moment by `Session::load`. A second source of truth in memory is not allowed
  to exist.
- **The transcript is cells, and replay is not a second renderer**: every line the UI
  writes is a cell painted by the same painter, whether it arrives live or is folded
  out of the log when resuming, so a resumed session is laid out exactly like the one
  that was watched. Adding a notification means adding a cell, never a second
  formatting path.
- **A cell's marker is columns of its own, not a prefix on its text.** It lives in the
  cell's gutter, so that the layer doing the wrapping is the one that sets in every
  line of a wrapped block — a prefix inside the spans would set in the first line only
  and the rest would come back to the left edge. Both front ends read the same gutter;
  the plain one writes it once per block, because the terminal wraps the rest.
- **The answer is the only cell on the left edge.** `Cell::Content` is the one cell
  with no gutter, so a reader can run their eye down column zero and find what the
  model said rather than what it did. Every other cell chooses a gutter, and the
  markers in use are a short vocabulary: `›` the user, `▸` something about to happen
  (a call, a question, the task in hand), `┆` thinking, `·` a result or a note. The
  columns are the rule; a blank marker is still a choice of columns.
- **The input box is part of the same grid.** It draws the user's marker itself and
  renders the editor into a field inset by the marker's columns, so a line being typed
  sits in the column the same line will sit in once it is submitted — neither the
  marker nor that column may be carried in a placeholder string, which would move as
  the typing started and go away with the hint. The box's rules are drawn by the
  front end for the same reason: a block on the editor would be inset with it, and the
  rules would stop short of the edges the transcript is read against.
- **Anything pinned to the bottom is paid for out of the transcript, and the box is
  the first to give way.** A pinned region (the standing task list, the queue, the
  status line) takes rows the transcript does not get, and a box short by a row can
  still be typed in where a transcript down to no rows is a session that cannot be
  read. A pinned region is measured off the lines it actually laid out, not off its
  item count: a task is as many rows as its words take, and a block whose height was
  guessed from the number of items either leaves a row blank or is silently clipped.
  What a cut list leaves out is counted, and the row a reader is waiting on — for a
  task list, the task in hand — is the one row that cannot be the one that gives way.
- **Ctrl-C during a turn is an out-of-band Command (graceful cancel), not a process
  kill**: the effect in flight is dropped (stream disconnected, child process
  `kill_on_drop`), unanswered calls are persisted with a deterministic cancellation
  marker, and the history is still request-valid. Cancellation is handled in the
  interpreter layer and never enters `next_action`.
  **The cancel channel is supplied by the front end, not built by the agent.** A front
  end owning the terminal runs it in raw mode, and raw mode clears `ISIG`: Ctrl-C then
  arrives as a key event and there is no SIGINT left to listen for. The listener must
  also exist before the turn's first await point — constructing one inside each
  `select` leaves a window between two of them in which a signal arriving exactly then
  is swallowed forever. Both facts were measured under a pty, and the smoke test is
  where they are pinned.
- **Turn step cap**: at most `machine::MAX_TOOL_STEPS` tool-call steps per turn;
  exceeding it ends the turn with a deterministic marker. A product-level termination
  guarantee: a model that goes haywire in a loop burns at most that much.
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
  regular file, an existing target keeps its own permission bits (the tmp file a
  rename hands over is a new one), content already on disk is answered as unchanged
  instead of rewritten, anything that is not a regular file is refused before anything
  is created, and a write that fails removes its temp file. What is refusable is
  refused before a byte is written.
- **A file's line endings are carried over by the call that changes it**, since the
  endings are the one thing a rewrite does not replace deliberately: a whole-file write
  sent in LF keeps a CRLF file CRLF and the other way round, and the text an Edit
  inserts is brought into the file's own endings — so the diff is the size of what the
  model meant to change instead of every line of the file. An `old_string`, being the
  file's own text, still has to match it as it stands, endings included. Where there is
  no one style to follow — a file being created, endings that are mixed, a file with no
  line ending to go by, or one too large to read the endings off — what the call sent
  is what lands. The endings are read off the file itself and never guessed from the
  extension, and the result says when the text was brought into them.
- **A failed Edit says where the trouble is**, since the model cannot see a file it has
  not read and a bare "not found" costs it a round trip through `Read`. A call that
  matched nothing is answered with the text in the file that comes nearest — the window
  of lines it best reads against, numbered as `Read` numbers them — and with every
  difference between the line at fault and the line of the call's that it stands for:
  the endings of the file's lines when they are the whole of it, and otherwise the text,
  the whitespace a line starts or ends with, or the whitespace inside it, each one
  **named** rather than left to be read off a line that cannot show a tab or a trailing
  space. A call that matched more than once is answered with where each occurrence is
  and what stands on either side of it, which is what one of them has to be quoted with
  to be singled out. The report is bounded on purpose — a quoted line to a readable
  width, occurrences up to a count, a long text shown around the line that differs —
  because it is a tool result and the model pays for every byte of it. It is a
  **report and never a match**: a tolerant fallback would edit text the call did not
  send, and an `old_string` is the file's own text or it is a miss.
- **A running command's output is a view, and the result is the record.** What a
  command prints while it runs is streamed to the front end as it arrives, and that
  stream is never part of the log: the result is what the model reads and what a
  resumed session replays, so a watched session shows the output and then the result
  where a resumed one shows the result alone — the one cell a session log cannot
  rebuild, which is why it says what it is. The view is cut at the result's own
  budget and says where it stopped — a command that prints a megabyte of build log
  must not put a megabyte of rows in front of a person — while the result keeps both
  ends of everything it printed. Nor is what a command prints the tool's own record of
  it: the bytes are kept by the readers as they arrive, so a chunk nothing is watching
  for can be dropped without losing it.
- **A command leads a session of its own, and nothing it starts outlives the call by
  accident.** A program that wants to ask a person something opens `/dev/tty` rather
  than reading its standard input, and where the front end's terminal is reachable it
  finds a terminal that answers: measured, `sudo true` waits minutes for a password
  there and fails in a tenth of a second with a session of its own — which is a result
  the model can act on. The same session is what makes the child's pid the id of the
  group that a timeout, a cancel and a background job are killed by, since killing the
  shell alone leaves a build compiling with nobody reading it and nobody waiting for
  it. Every wait a call has is on a budget — the command's own, the grace its output
  is given after the command is gone, the time a killed child is given to be reaped —
  and none of them can be extended by anything the command does.
- **What cannot work here is answered rather than attempted.** A screen editor is
  refused by name, because it does not fail on its own: measured, `vim` with its output
  on a pipe — which is what every call gives it — never exits, so the call runs until
  its timeout kills it and answers with terminal control codes that changed no file.
  The refusal names what to do instead, the editor's own script mode included. Nothing
  else needs refusing: `top`, `sudo` and a bare `read` are each over in a tenth of a
  second on their own, and a command that is long rather than stuck belongs in the
  background, where its output is a file the result names. What no name can catch — a
  program that opens an editor under a variable, as `git commit` does with no message
  — is met by pointing those variables at `true`, so that it gives its own error
  instead of drawing a screen first.
- **A call may ask for what it needs, and is told when it cannot have it.** The
  timeout is the call's to name, within a cap, because a test suite that is slow is not
  a test suite that is stuck; what it may not have is answered with what is possible
  rather than rounded to something it did not ask for. A call that returns at once and
  leaves a job running is the same rule the other way round: what comes back is
  everything needed to follow the job and to end it — its pid, the file its output goes
  to, how to kill the group — and the job is in a session of its own, so its own ending
  is what ends it and not this call's.
- **Approval gate**: execution is trusted by default; with `--ask`, a call that can
  change something on disk waits for a y/N, while a call that changes nothing on disk
  (reading, writing the task list) is always allowed. The set of calls that are asked
  about is a blacklist, so a tool added later is asked about until somebody decides
  otherwise. Denial, cancellation, an unanswered question and the step cap each persist
  a deterministic text marker that is passed back to the model so it can adjust on its
  own; the command echo is unchanged.
- **A question is answered, not executed**: `AskUserQuestion` is the one tool whose
  result is a person's. The interpreter reads its arguments, asks through the front
  end's `Ask` channel and writes what was chosen back as the call's result — the same
  shape as the approval gate, with a value instead of a decision. It never passes the
  gate (asking permission to ask is asking twice about one thing), and its questions
  are the call's own arguments, so the transcript replays them exactly as they were
  asked. Dismissing one is not a hard error: the marker text is the result, and the
  turn carries on.
- **A front end that cannot take an answer declines rather than blocking**: a question
  asked where nobody is listening — an end of input, a front end that has gone away —
  is answered `None`, which the interpreter writes down as the unanswered marker, never
  a wait with no one to end it. The gate's version of having nobody to ask is a
  denial; neither one is an error the turn cannot recover from.
- **The task list is a fold of the log, not state**: the tool writes the whole list
  every call, so what is standing is the arguments of the last such call and nothing
  has to be kept in step — a resumed session pins exactly the list the watched one did.
  It is executed like any other tool (nothing about its answer needs a person) and
  never passes the gate (asking permission to write down a note is asking about
  nothing). Both surfaces that show it go through the same marks and the same summary
  line, so they cannot say different things about one list. The one count that comes
  back unasked — a call that leaves tasks in hand — is in the tool's answer and not in
  that summary line, which is a head line for a person who has the list in front of
  them.
- **Session logs are append-only and never rewritten**: a crash loses at most a
  trailing partial line, and corrupt lines are skipped when reading; interrupted tool
  calls are healed **deterministically** at load time, as an in-memory view only — the
  synthesized text must be a constant, or the prefix cache becomes unstable.
- **Exactly one renderer per process**: streaming output and usage accounting go
  through the same instance, or counts are lost.
- **A provider has an id and a name, and they do not stand in for each other**: the id
  addresses it (flags, `/login <id>`, `<id>/<modelid>`, the session meta,
  `settings.json`); the name is what a person reads (the `/login` menu, an error
  message). Anything typed or stored takes the id, and the menus are the only place the
  two meet.
- **A secret the user types is answered, not stored**: an API key reaches
  `settings.json` (written by `/login`) and exists nowhere else — not in the
  transcript, the session log, the input history, or an environment variable. It is
  asked for as a question, so the front end hides it while it is typed and nothing that
  is submitted can carry it onwards; a configuration a second mechanism could also
  satisfy is one whose state nobody can see.
- **A front end that cannot take the terminal declines; it does not fail.** Raw mode
  and the alternate screen are process-wide, and a terminal that refuses either leaves
  nothing to draw on. Starting the session must survive that by falling back to the
  plain front end, and an attempt that fails halfway must undo what it did — including
  the alternate screen, which would otherwise hide everything the user had on it.
- **While a turn runs a front end accepts exactly four things**: the cancel key, the
  answer to an open approval question, the answer to an open question tool call, and
  the next line. The last is a queue, not a second turn: Enter takes the line out of
  the box and it runs when the turn in flight ends — from the head, interrupted or not.
  Everything else is dropped.
  The two answers are the ones that are easy to lose: a gate or a panel whose answer
  never arrives is, from the user's side, indistinguishable from a model that has hung
  — so while either is open the box is the answer's and nothing else is typed into it,
  and a line being composed when one arrives is held aside and given back.
  A question with options is answered with the arrow keys (or the digits, or space
  where several may be chosen) and Enter; Esc dismisses the call, which is not the same
  as answering it with the option the cursor happened to be on. The box stays the place
  an answer is typed: a panel that could only answer with the options it was given is a
  panel that cannot ask an open question.
- **Anything that moves on its own must be derived from the clock, never from the
  number of redraws**, and that clock must be part of whatever decides whether to
  redraw at all: a front end that skips an unchanged screen otherwise freezes its own
  animation, and a frame counter can be left behind by a redraw that never happened.
- **Truncating or clipping text must land on a UTF-8 character boundary, and any text
  measured against a terminal width must be measured in display columns, not chars**:
  a CJK ideograph or an emoji is one char but two columns, so char-based arithmetic
  silently overruns the field it was sizing.
- **Text committed to the terminal instead of a scrolling stream must be wrapped,
  never clipped**: a fixed-width rendering path cuts an over-long line off, where the
  terminal's own soft wrapping would have kept every column. The plain front end may
  leave wrapping to the terminal; a front end that draws into a region may not.

## Prefix cache (must read before changing request or message construction)

The backend matches its cache on the exact request prefix, and the hit rate is what
cost and latency are made of. A request is pure in `(provider, meta, history)`:
nothing else may reach it.

- The system prompt is a fixed constant. Injecting time, cwd, random ids, or any other
  dynamic content is **forbidden**.
- **The workspace's project instructions are frozen into the session.** They are read
  once — for the directories above where the session started, at creation; for a
  directory below it, on the model's first touch of a file there — and are then sent
  from the session's meta, or replayed from the log, byte-for-byte. Nothing on the
  request path reads the filesystem: re-reading a file, refreshing the instructions on
  resume, or re-framing them is a different prefix and a full miss. A resumed session
  keeps the instructions it was started with; only a new session reads the workspace as
  it stands.
- History messages are replayed **byte-for-byte**: no trimming, no reordering, no
  clipping, no compression, no normalization. Any change that "optimizes the history"
  equals a full cache miss.
- A session's provider, model and effort live in its meta, and switching one appends a
  meta line without touching the history — the conversation is what the cache matches,
  so a switch that rewrote or re-sent it would throw the session away.
- Changing the tool set, the tool order, or any tool schema text (names, descriptions,
  parameter descriptions, parameter keywords) changes the prefix and causes a full
  cache miss — only do it deliberately.
- An **attached image is part of the message**: it is stored as the `data:` URL the
  request carried and replayed with it. Reading the file again, or re-encoding the
  bytes, is a different string and so a full cache miss — and the file may not even be
  there any more. Nothing about an image may be re-derived from disk; what the message
  says about itself is also all the transcript may show.
- Verify with the per-turn hit/miss token counts; do not go by feel.

## Backend API hard constraints (must read before changing request construction)

The official docs are authoritative (`thinking_mode`, the `kv_cache` guide). The ones
easiest to trip over:

- **When a request carries `tools`, the `reasoning_content` of historical assistant
  messages must be passed back verbatim (DeepSeek returns 400 when it is missing).**
- **History validity has an executable specification**: `machine::is_request_valid` —
  every declared call has exactly one result inside the window that immediately
  follows it, and no tool result is stray. Both `heal` and request construction target
  it as their invariant, and its tests pin "any crash prefix, once healed, is valid" by
  enumerating a bounded family of history shapes rather than a handful of examples.
  `cargo test theorem_` runs the two of them; run those before changing `heal` or
  request construction, and take their counts from the tests rather than from this
  file.
- In a streaming response, thinking content precedes the body text; token usage rides
  on the last content block and there is no separate usage event.

## Working style

- Read the code before changing it; this file deliberately omits what the code says.
- Keep the task list live. It is the only view of a turn's work the user has while it
  runs, so write it before the first change, mark each task completed as it lands
  rather than in one batch at the end, and keep at most one task in progress. A step
  that is worth more than a line still gets its list update first: the reader is
  watching the list, not the diff.
- When behavior is uncertain, confirm it with a `-p` or a pty smoke run, then draw
  conclusions; a guess written down as a rule is worse than no rule.
- Do not abstract for "we might need it later"; the value of this project is being
  small and direct.

## Keeping this file honest

- A change that makes a line here false updates that line in the same commit. A rule
  nobody can trust costs more than a rule that is missing.
- Quote no number, count, or path about the code's own shape. Names are anchors that
  survive a rename; values do not survive anything.
- When a rule can be checked mechanically, put it in a test and point at the test —
  or delete the prose, if the test can carry it alone.
- This file is loaded into every session started in this workspace and frozen into it:
  it is paid for on every request, and editing it changes only sessions started after
  the edit. Write for the long-lived version, and keep it short.
