#!/usr/bin/env python3
"""Interactive front-end smoke test: run caocli inside a pseudo-terminal, which is
the only place the front end that owns the screen exists at all.

It covers what only a terminal can show: the alternate screen being entered and
given back, the input box, the pinned status line, the transcript being drawn and
paged, and the terminal handed back as it was found. Nothing has to answer a
cursor query any more -- that was the inline viewport's -- so this is a pty with a
size and nothing else.

Only local commands are sent -- no provider is called -- so a placeholder API key
is enough and the test stays offline. Exit code 0 = pass.

With `SMOKE_LIVE=1` and a real key, it goes on to a live turn, which is the only
way to see the parts that need a model to move: the status line running a clock of
its own, a line queued behind a running turn and run when it ends, and the
approval gate that asks before a tool runs.
"""

import codecs
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time
import unicodedata

BIN = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "caocli")
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
ROWS, COLS = 24, 80

# ANSI escapes, stripped for the diagnostics: a line of text is written with a
# cursor move and a style change around it, so a needle would otherwise be split
# across escape sequences.
ESCAPES = re.compile(rb"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[=>]|\x1b\][^\x07]*\x07")

# A control sequence, split into its parameters (with the private prefix some of
# them carry) and its final byte, which is what says what it does.
CSI = re.compile(r"\[([?]?[0-9;]*)([a-zA-Z])")


def cell_width(ch: str) -> int:
    """The columns a character takes -- the same rule the terminal applies, and
    the reason a wide character is worth a test of its own."""
    return 2 if unicodedata.east_asian_width(ch) in "WF" else 1

# Rendered by the interactive front end and by nothing else: the plain prompt has
# no box, no placeholder and no pinned line.
VIEWPORT = "type a message"
BANNER = "caocli · session"
STATUS = "cache"
PICKER = "show this"  # the /help row of the command picker
# One line of what /help commits. Not the first one: the pinned region takes rows
# from the bottom, so a long answer has its head off the top of the screen before
# the last row of it is even drawn -- what is looked for here has to be a line the
# window can still hold.
HELP = "Startup flags:"

# Live marks: what the box says while a turn runs, and the gate that stands
# between a tool call and its execution.
RUNNING = "the turn is running"
GATE = "run it? [y/N]"

# The word the queued line asks for and expects back: nonsense, so that what is
# found on the screen is what was sent and never anything else that happened to
# be there. It is looked for twice -- as the line, and as the answer to it.
QUEUED_TOKEN = "zqx7"

# A session with an edit already in it. Resuming it draws the change, which is the
# replay half of the same cell a live turn produces; the id and the lines are what
# the assertions below look for.
SEED = "20260910-120000"

# What `/login` is given below. Nonsense on purpose: it is never sent anywhere,
# and looking for exactly it is how the checks below know that what they found
# is what was typed and never anything the model wrote.
FAKE_KEY = "sk-tui-smoke-not-a-real-key"


def api_key() -> str:
    """A key for the run: keys are configured in `settings.json` and nowhere else
    now, so the smoke has to put one there.

    The one `/login` stored on this machine. A placeholder when there is none:
    it is enough for everything here that does not call the backend.
    """
    try:
        with open(os.path.expanduser("~/.caocli/settings.json")) as f:
            return json.load(f)["providers"]["deepseek"]["api_key"]
    except (OSError, KeyError, ValueError):
        return "smoke-test-placeholder"


def seed_settings(home: str, key: str) -> None:
    """Write the file `/login` writes, so the run has a key to find. Without it
    `/model` could not switch anything: a model with no key behind it is refused."""
    directory = os.path.join(home, ".caocli")
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, "settings.json"), "w") as f:
        json.dump({"providers": {"deepseek": {"api_key": key}}}, f)


def seed_session(home: str) -> str:
    """Write a session that has already made an edit, and return its id.

    The log format is the one the front end writes: a header, then the messages.
    The edit is what a live turn would have produced, so what is drawn from it on
    resume is what would have been drawn when it happened.
    """
    directory = os.path.join(home, ".caocli", "sessions")
    os.makedirs(directory, exist_ok=True)
    arguments = json.dumps(
        {"file_path": "a.txt", "old_string": "one", "new_string": "two"}
    )
    lines = [
        json.dumps(
            {
                "t": "header",
                "id": SEED,
                "created_at": 1_789_000_000,
                "model": "deepseek-flash",
                "reasoning_effort": "max",
            }
        ),
        json.dumps({"t": "msg", "message": {"role": "user", "content": "change a.txt"}}),
        json.dumps(
            {
                "t": "msg",
                "message": {
                    "role": "assistant",
                    "reasoning_content": "weighing the change",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "Edit", "arguments": arguments},
                        }
                    ],
                },
            }
        ),
        json.dumps(
            {
                "t": "msg",
                "message": {"role": "tool", "content": "ok: a.txt", "tool_call_id": "call_1"},
            }
        ),
        # Non-ASCII on purpose: a wide character is two columns and the terminal
        # moves past both, so this is the line that shows whether anything is being
        # written into the column it covers.
        json.dumps(
            {
                "t": "msg",
                "message": {"role": "assistant", "content": "中文宽度测试"},
            }
        ),
    ]
    with open(os.path.join(directory, f"{SEED}.jsonl"), "w") as f:
        f.write("\n".join(lines) + "\n")
    return SEED


def spawn(home: str, extra: list[str]):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    # A freshly allocated pty has an all-zero termios, which is not what a real
    # terminal hands a program: the front end switches raw mode on from cooked
    # mode, and starts from the same place a user's terminal would.
    attrs = termios.tcgetattr(slave)
    attrs[3] |= termios.ISIG | termios.ICANON | termios.ECHO
    attrs[6][termios.VINTR] = 3  # ^C
    termios.tcsetattr(slave, termios.TCSANOW, attrs)
    pid = os.fork()
    if pid == 0:
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        os.dup2(slave, 0)
        os.dup2(slave, 1)
        os.dup2(slave, 2)
        for fd in (master, slave):
            if fd > 2:
                os.close(fd)
        env = dict(os.environ)
        # A temporary HOME keeps the run out of the user's sessions and history.
        env["HOME"] = home
        env.pop("NO_COLOR", None)
        os.execve(BIN, ["caocli", *extra], env)
    os.close(slave)
    return pid, master


class Screen:
    """What the terminal is showing.

    A full-screen front end is drawn by difference: a cell that did not change is
    never written, so the byte stream is not what the screen says -- it is the
    difference between one frame and the next. Reading it back therefore means
    putting the frames together, which is what this does: cursor moves, erases and
    text, with a wide character taking the two columns it takes.
    """

    def __init__(self, rows: int, cols: int):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = self.col = 0
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pending = ""

    def lines(self) -> list[str]:
        return ["".join(row).rstrip() for row in self.grid]

    def find(self, needle: str) -> bool:
        return any(needle in line for line in self.lines())

    def feed(self, chunk: bytes):
        self.pending += self.decoder.decode(chunk)
        while self.pending:
            if self.pending[0] == "\x1b":
                eaten = self.escape(self.pending[1:])
                if eaten is None:
                    return  # a sequence split across two reads: wait for the rest
                self.pending = self.pending[1 + eaten :]
                continue
            ch, self.pending = self.pending[0], self.pending[1:]
            self.put(ch)

    def escape(self, rest: str) -> int | None:
        """Consume one escape sequence, or say how much of it is still missing."""
        if not rest:
            return None
        if rest[0] == "[":
            m = CSI.match(rest)
            if m is None:
                return None
            self.csi(m.group(1) + m.group(2))
            return m.end()
        return 1  # ESC 7 / ESC 8 / ESC = / ESC >: nothing this reads depends on them

    def csi(self, params: str):
        final, body = params[-1], params[:-1]
        if body.startswith("?"):
            # The alternate screen coming and going: both leave a blank screen.
            if body[1:] == "1049":
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
                self.row = self.col = 0
            return
        args = [int(a) if a else 0 for a in body.split(";") if a != ""] or [0]
        match final:
            case "H" | "f":  # position
                self.row = min(max((args[0] or 1) - 1, 0), self.rows - 1)
                col = args[1] if len(args) > 1 else 0
                self.col = min(max((col or 1) - 1, 0), self.cols - 1)
            case "A":
                self.row = max(self.row - (args[0] or 1), 0)
            case "B":
                self.row = min(self.row + (args[0] or 1), self.rows - 1)
            case "C":
                self.col = min(self.col + (args[0] or 1), self.cols - 1)
            case "D":
                self.col = max(self.col - (args[0] or 1), 0)
            case "G":
                self.col = min(max((args[0] or 1) - 1, 0), self.cols - 1)
            case "d":
                self.row = min(max((args[0] or 1) - 1, 0), self.rows - 1)
            case "J":  # erase in the screen
                if args[0] == 2:
                    self.grid = [[" "] * self.cols for _ in range(self.rows)]
                else:
                    self.erase(self.row, self.col, self.row + 1, self.cols)
            case "K":  # erase in the line
                if args[0] == 2:
                    self.erase(self.row, 0, self.row + 1, self.cols)
                elif args[0] == 1:
                    self.erase(self.row, 0, self.row + 1, self.col + 1)
                else:
                    self.erase(self.row, self.col, self.row + 1, self.cols)
            case "X":
                self.erase(self.row, self.col, self.row + 1, self.col + (args[0] or 1))
            case "m":  # style: what is on the screen does not depend on it
                pass

    def erase(self, y0: int, x0: int, y1: int, x1: int):
        for y in range(y0, min(y1, self.rows)):
            for x in range(x0, min(x1, self.cols)):
                self.grid[y][x] = " "

    def put(self, ch: str):
        if ch == "\n":
            return  # nothing here writes a newline: rows move by escape
        if ch == "\r":
            self.col = 0
            return
        if ch < " ":
            return
        width = cell_width(ch)
        if self.col + width > self.cols:
            return  # past the edge: a wide character in the last column is dropped
        self.grid[self.row][self.col] = ch
        for dx in range(1, width):
            self.grid[self.row][self.col + dx] = ""
        self.col += width


class Terminal:
    """The other end of the pty: it reads, and holds what is on the screen."""

    def __init__(self, master: int, rows: int = ROWS, cols: int = COLS):
        self.master = master
        self.raw = b""
        self.screen = Screen(rows, cols)

    def text(self) -> str:
        """Everything written, escapes stripped. For diagnostics only: the front
        end writes what changed, not what is there."""
        return ESCAPES.sub(b"", self.raw).decode("utf-8", "replace")

    def shown(self) -> str:
        return "\n".join(f"    | {line}" for line in self.screen.lines())

    def pump(self, timeout: float) -> bool:
        r, _, _ = select.select([self.master], [], [], timeout)
        if self.master not in r:
            return False
        try:
            chunk = os.read(self.master, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        self.raw += chunk
        self.screen.feed(chunk)
        return True

    def expect(self, needle: str, timeout: float, quiet: bool = False) -> bool:
        """Wait for `needle` to be on the screen."""
        end = time.time() + timeout
        next_note = time.time() + 15
        while True:
            if self.screen.find(needle):
                if not quiet:
                    print(f"  \u2713 {needle!r}")
                return True
            if time.time() >= end:
                if not quiet:
                    print(f"  \u2717 timed out waiting for {needle!r}")
                    print("    screen:\n" + self.shown())
                return False
            if time.time() >= next_note:
                # A live turn spends a long time waiting on a model, so silence
                # would be indistinguishable from a hang.
                print(f"  \u00b7 still waiting for {needle!r}")
                next_note = time.time() + 15
            self.pump(0.5)

    def status_row(self) -> str:
        """The status line: the last row of the screen, under the input box."""
        rows = self.screen.lines()
        return rows[-1] if rows else ""

    def transcript_top(self) -> str:
        """The first row of the transcript: what the window over it is scrolled to.
        The pinned region is at the bottom, so row zero is the transcript's."""
        return self.screen.lines()[0]

    def until(self, wanted, timeout: float) -> bool:
        """Wait for the screen to satisfy `wanted`.

        A screen drawn by difference is a screen that has to be given the time to
        be drawn: what is asked for here is not in the next byte, it is in the
        frame after the key that moved it.
        """
        end = time.time() + timeout
        while True:
            if wanted():
                return True
            if time.time() >= end:
                print("    screen:\n" + self.shown())
                return False
            self.pump(0.3)

    def left_edge(self, needle: str) -> int | None:
        """The column `needle` starts in, or None when it is not on the screen.

        The column rather than the row, because the left edge is what the layout is
        read down: it is the one thing the answer and the machinery around it are
        told apart by, and so the one thing worth asking a real terminal about.
        """
        for line in self.screen.lines():
            at = line.find(needle)
            if at >= 0:
                return at
        return None

    def box_rows(self) -> list[str]:
        """The input box as drawn: the two rules that close it in and what lies
        between them, which is what says how tall it is. The box has no vertical
        edges, so its rules are rows of nothing but rule -- there are no corners
        to look for -- and the status line is the row under the bottom one."""
        rows = self.screen.lines()
        rules = [y for y in range(len(rows) - 2, 0, -1) if set(rows[y]) == {"─"}]
        if len(rules) < 2:
            return []
        bottom, top = rules[0], rules[1]
        return rows[top : bottom + 1]

    def idle(self, settle: float, timeout: float) -> bool:
        """Wait until the box is idle: its placeholder says so, and the
        terminal has been silent for `settle` seconds after it.

        Nothing on the screen moves on its own, so silence alone is not
        evidence that a turn has ended -- a running turn is a still terminal
        too. The placeholder is what says a key can be sent: a key pressed
        during a turn is taken as the next line, so a line sent without
        waiting arrives half-composed at the prompt and the fragment left in
        the box is what gets submitted. (It would be *queued* rather than
        dropped, which is a different way to be wrong: the assertions below
        about the prompt would be about a line waiting behind a turn.)
        """
        end = time.time() + timeout
        last = time.time()
        while time.time() < end:
            if self.pump(0.2):
                last = time.time()
            elif self.screen.find(VIEWPORT) and time.time() - last >= settle:
                return True
        print("  ✗ the front end never went idle")
        return False

    def wait_for_file(self, path: str, timeout: float) -> bool:
        """Wait for a file the approved tool call was asked to create.

        The proof that the gate let a call through is on disk: an approval the
        front end only thought it sent would leave no file behind, and no answer
        the model writes can fake one.
        """
        end = time.time() + timeout
        while time.time() < end:
            if os.path.exists(path):
                return True
            self.pump(0.3)
        print(f"  ✗ the approved command never ran: {path} was not created")
        return False

    def send(self, data: str):
        os.write(self.master, data.encode())


def live(term: "Terminal", home: str) -> bool:
    """A real turn, the only way to see what needs a model to move.

    `/tmp`-style probing used to be how these paths were looked at; a script that
    cannot be run twice is not a test, so it lives here. `--ask` is passed on the
    command line by the caller, which is what puts the gate in the way.
    """
    ok = True
    # The box is what says a turn is running now: its placeholder changes the
    # moment the turn begins, before any model output has arrived.
    # Note: what the user said is in here, but it is not asserted on screen. A
    # turn that fails or answers quickly writes more than the window shows before
    # the first frame is painted -- and the screen is written incrementally, so a
    # line that was never painted never appears in the byte stream either. The
    # cell a submitted line produces is covered by unit tests and by the resumed
    # turn above, which draws one out of the log.
    term.send(f"use the Read tool to read {ROOT}/Cargo.toml\r")
    ok &= term.expect(RUNNING, 15)

    # A line typed while the turn runs is queued rather than dropped: Enter takes
    # it out of the box, it is drawn above the status line while it waits, and the
    # head of the queue runs when the turn ends.
    #
    # The queued line is a command, whose answer is the front end's own: `new
    # session` is proof that it ran, where a model's account of having done
    # something reads the same whether or not it did. What is asserted first is
    # the queue's own row -- a line that stayed in the box would be in the box
    # instead, which is why the row is matched exactly and the box looked at too.
    term.send("/new\r")
    if term.until(
        lambda: any(line.strip() == "› /new" for line in term.screen.lines()), 30
    ):
        print("  ✓ the line typed during the turn is drawn in the queue")
    else:
        print("  ✗ the line typed during the turn was not queued")
        ok = False
    if any("/new" in line for line in term.box_rows()):
        print("  ✗ the queued line is still sitting in the box")
        ok = False
    # The call the turn was asked for, drawn before the queue runs: what is
    # waiting does not take the turn's place.
    ok &= term.expect("▸ Read", 180)
    ok &= term.expect("new session", 180)  # and the queued line ran after it

    # The gate: a tool call waits for an answer from the input box, and the
    # answer is a line of text like any other -- the part that cannot be checked
    # without a terminal.
    #
    # What the command does is deliberately something that leaves a mark on disk.
    # An earlier version asked for a number and looked for it on screen, which
    # passed the moment the model did the arithmetic in a sentence of its own.
    #
    # Asked twice, because what is under test is the front end and not the
    # model's willingness to reach for a tool.
    marker = os.path.join(home, "smoke-ran")
    gated = False
    for attempt in (1, 2):
        term.send(f"Run the shell command `touch {marker}` with the Bash tool, then stop.\r")
        if term.expect(GATE, 120, quiet=attempt > 1):
            term.send("y\r")  # the answer comes out of the box, like any line
            gated = term.wait_for_file(marker, 90)
            break
        print(f"  · no Bash call on attempt {attempt}")
        if not term.idle(2.0, 60):
            return False
    if not gated:
        print("  ✗ no tool call ever needed approval")
        return False
    ok &= gated

    # Ctrl-C with a line queued behind the turn: the turn stops and the head of
    # the queue runs. The turn asked for is one that would take a while, and the
    # cancel key is sent right behind the line -- the model call is in flight, so
    # the turn cannot have ended on its own, and what is under test is the queue
    # rather than the model's speed.
    #
    # What the queued line was is a prompt, so that running it puts the line in
    # the transcript: QUEUED_TOKEN is matched twice -- once as the line itself,
    # once in what the model answers with. A single match would only mean the row
    # is still waiting in the queue, which is where it was already.
    term.idle(2.0, 120)
    term.send("count from one to a hundred, one number per line\r")
    ok &= term.expect(RUNNING, 30)
    term.send(f"say {QUEUED_TOKEN} and nothing else\r")
    term.send("\x03")

    ok &= term.expect("interrupted (Ctrl-C)", 30)
    if not term.until(
        lambda: sum(1 for line in term.screen.lines() if QUEUED_TOKEN in line) >= 2, 180
    ):
        print("  ✗ the queued line did not run after the turn was interrupted")
        return False
    print("  ✓ the queued line ran after the turn was interrupted")
    return ok


def main() -> int:
    # Unbuffered: a live turn takes minutes, and a progress line that only
    # arrives when the process ends is no progress at all.
    sys.stdout.reconfigure(line_buffering=True)
    if not os.path.exists(BIN):
        print(f"✗ {BIN} not found, run cargo build first")
        return 1
    # Live by request only: it needs a real key and a provider to answer.
    is_live = os.environ.get("SMOKE_LIVE") == "1"
    ok = True
    with tempfile.TemporaryDirectory(prefix="caocli-tui-smoke-") as home:
        # `-c` resumes the session seeded above, so the first thing drawn is a
        # turn that happened in an earlier process.
        seed_session(home)
        seeded = api_key()
        seed_settings(home, seeded)
        pid, master = spawn(home, ["-c", *(["--ask"] if is_live else [])])
        term = Terminal(master)
        try:
            # Startup: the box, the pinned line. Both are drawn before the first
            # key, so seeing them is seeing that the screen was entered -- and the
            # alternate screen is the whole reason none of it reaches the terminal's
            # own scrollback.
            ok &= term.expect(VIEWPORT, 30)
            ok &= term.expect(STATUS, 15)
            if b"\x1b[?1049h" not in term.raw:
                print("  ✗ the alternate screen was never entered")
                ok = False
            # The wheel is asked for by name. A terminal that has not been asked
            # sends Up and Down in place of a notch, and those are the box's
            # history: a wheel that recalls a line rather than reading one.
            if b"\x1b[?1000h" not in term.raw or b"\x1b[?1006h" not in term.raw:
                print("  ✗ the terminal was never asked for the wheel")
                ok = False
            # The resumed turn's edit, line by line, from the log rather than from
            # anything that is running now.
            ok &= term.expect("  - one", 15)
            ok &= term.expect("  + two", 15)
            # The thinking is set in behind a rule of its own -- faint, in the columns
            # past the marker -- and no longer on a ground: the ground was the loudest
            # thing on the screen and the hardest thing on it to read, and the columns
            # say the same thing in a way a terminal cannot lose.
            ok &= term.expect("weighing the change", 15)
            if b"48;5;236" in term.raw:
                print("  ✗ the thinking still paints a ground")
                ok = False
            # The layout contract: the answer is the only thing on the left edge, and
            # everything the model did or was thinking is set in past it. Asked of the
            # columns rather than of the style, because that is what a reader uses.
            edges = {
                needle: term.left_edge(needle)
                for needle in (
                    "中文宽度测试",
                    "weighing the change",
                    "change a.txt",
                    "Edit a.txt",
                    "ok: a.txt",
                )
            }
            if edges["中文宽度测试"] != 0:
                print(f"  ✗ the answer is not the left edge: {edges}")
                ok = False
            for set_in in ("weighing the change", "change a.txt", "Edit a.txt", "ok: a.txt"):
                if edges[set_in] != 2:
                    print(f"  ✗ {set_in!r} is not set in: {edges}")
                    ok = False
            # Those characters have to sit together on the screen: a space in the
            # column a wide character covers is what the bug looks like.
            if not term.screen.find("中文宽度测试"):
                print("  ✗ wide characters are drawn with a gap after each one")
                ok = False
            # The box is the draft's shape. Raw mode sends Ctrl-J as \n, and the
            # box -- three rows while it is empty -- has to grow to keep the line
            # it adds on screen, and give the rows back when the draft goes. The
            # screen is read back rather than the byte stream: a drawn cell that
            # did not change is never written again.
            term.idle(2.0, 30)
            empty = term.box_rows()
            term.send("alpha\nbeta")  # Ctrl-J between the two lines
            ok &= term.expect("beta", 15)
            grown = term.box_rows()
            if len(grown) != len(empty) + 1 or not any("alpha" in row for row in grown):
                print(f"  ✗ Ctrl-J did not grow the box: {grown}")
                ok = False
            # And the draft is set in where the same line will be set in once it is
            # submitted: the marker on the first row, the columns past it on the rows
            # after, which is what the transcript does with the line this becomes. The
            # marker is the box's own rather than a character of the draft, so it is
            # there whether the box holds a line, a hint or nothing at all -- and it
            # does not move when the typing starts.
            if grown[1] != "› alpha":
                print(f"  ✗ the box does not set its first row in: {grown}")
                ok = False
            if grown[2] != "  beta":
                print(f"  ✗ a second draft row came back to the edge: {grown}")
                ok = False
            # ... and the cursor is on the line that key added, with the text it
            # was typed after: a grown box the cursor is not drawn in would be no
            # better than the one-row box it replaced.
            lines = term.screen.lines()
            typed = next((y for y, line in enumerate(lines) if "beta" in line), None)
            if typed is None or (term.screen.row, term.screen.col) != (
                typed,
                lines[typed].index("beta") + len("beta"),
            ):
                print(
                    f"  ✗ the cursor is not on the line Ctrl-J added: "
                    f"{term.screen.row},{term.screen.col}"
                )
                ok = False
            term.send("\x03")  # Ctrl-C clears the line
            ok &= term.expect(VIEWPORT, 15)
            if len(term.box_rows()) != len(empty):
                print("  ✗ the emptied box did not go back to three rows")
                ok = False
            # Typing a command prefix opens the picker, which draws over the live
            # area: it is the one widget that is not part of either the
            # transcript or the box.
            term.idle(2.0, 30)
            term.send("/he")
            ok &= term.expect(PICKER, 15)

            # Completing and submitting it: the answer becomes part of the
            # transcript, which the application holds and draws.
            term.send("\t")  # Tab completes without running the command
            term.send("\r")
            ok &= term.expect(HELP, 15)

            # `/resume` with no id offers the sessions to choose from instead of
            # asking for one, and the choice is submitted as the line the plain
            # prompt would have been given.
            #
            # A second session is needed to switch to: the open one is the single
            # writer of its own file and cannot be resumed, which is a constraint
            # the front end has nothing to do with.
            ok &= term.idle(2.0, 30)
            term.send("/new\r")
            ok &= term.expect("new session", 30)
            ok &= term.idle(2.0, 30)
            term.send("/resume\r")
            ok &= term.expect("messages ·", 15)  # a picker row, not `/sessions`
            term.send("\x1b[B")  # Down: the first row is the session just opened
            time.sleep(0.3)
            term.send("\r")
            ok &= term.expect("switched to session", 30)

            # The transcript is a window that pages in place. `/help` wrote more
            # lines than the screen has rows, so the first line of the session has
            # been pushed off the top of it -- and paging back is the only way to
            # see it again, since nothing writes that banner twice.
            ok &= term.idle(2.0, 30)
            if not term.screen.find("Startup flags"):
                print("  ✗ /help did not reach the screen")
                ok = False
            if term.screen.find(BANNER):
                print("  ✗ the transcript did not scroll: the banner is still up")
                ok = False
            # A page at a time until the banner comes back: how far up it is
            # depends on everything drawn since, which is what this file keeps
            # adding to.
            for _ in range(10):
                term.send("\x1b[5~")  # PageUp
                if term.until(lambda: term.screen.find(BANNER), 3):
                    break
            else:
                print("  \u2717 paging back never reached the banner")
                ok = False

            # The wheel, which is the other way back through the transcript and
            # the one that has a terminal to argue with: while it was left to the
            # terminal, a notch arrived as Up and Down and recalled a line into the
            # box instead. Both halves are asserted -- the window moves, and the box
            # does not -- because only the second one is the bug.
            #
            # Wheel down first, past the end: the window stops there, so where it
            # is scrolled to is known and a notch back can be asked to return to it.
            notches = {"up": "\x1b[<64;10;10M", "down": "\x1b[<65;10;10M"}
            for _ in range(50):
                term.send(notches["down"])
            ok &= term.idle(1.0, 30)
            top = term.transcript_top()
            term.send(notches["up"])
            if not term.until(lambda: term.transcript_top() != top, 15):
                print("  ✗ a wheel notch did not move the transcript")
                ok = False
            if not term.screen.find(VIEWPORT):
                print("  ✗ the wheel put a line from the history in the box")
                ok = False
            term.send(notches["down"])
            if not term.until(lambda: term.transcript_top() == top, 15):
                print("  ✗ a notch down did not return the window")
                ok = False

            # `/model` offers the models the preset table knows, each named
            # `<provider>/<modelid>`. Choosing a row submits it as the line the
            # plain prompt would have been given, and the status line takes the
            # new name -- the whole point of naming a model by its provider.
            ok &= term.idle(2.0, 30)
            term.send("/model\r")
            ok &= term.expect("deepseek/deepseek-v4-pro", 15)
            term.send("\x1b[B")  # Down: the first row is the model in use
            time.sleep(0.3)
            term.send("\r")
            if not term.until(
                lambda: "deepseek/deepseek-v4-pro" in term.status_row(), 15
            ):
                print("  \u2717 the status line did not take the chosen model")
                print("    " + term.shown())
                ok = False

            # `/effort` offers the tiers the provider in use accepts, with the
            # one in effect marked. Choosing a row submits it as the line the
            # plain prompt would have been given, and the status line takes the
            # tier beside the model.
            ok &= term.idle(2.0, 30)
            term.send("/effort\r")
            ok &= term.expect("current", 15)  # the tier in effect, marked
            term.send("\x1b[B")  # Down: from the first tier to the next one
            time.sleep(0.3)
            term.send("\r")
            if not term.until(lambda: "effort high" in term.status_row(), 15):
                print("  \u2717 the status line did not take the chosen tier")
                print("    " + term.shown())
                ok = False

            # `/login` offers the providers, then asks for the key in a box that
            # hides it. Nothing else in this file can check the hiding: the key
            # must not be on the screen, in the byte stream, or in the session --
            # and it must be in settings.json, which is what it is for.
            ok &= term.idle(2.0, 30)
            term.send("/login\r")
            # The rows are the providers' *names*: what the file and the session
            # carry is an id, and that is not what a person is asked to read.
            ok &= term.expect("Z.AI Coding CN", 15)
            ok &= term.expect("no key", 15)  # its detail, not `/help`
            term.send("\x1b[B")  # Down: DeepSeek is first, Z.AI is what is wanted
            time.sleep(0.3)
            term.send("\r")
            ok &= term.expect("Z.AI Coding CN API key", 15)  # the question
            term.send(FAKE_KEY + "\r")
            ok &= term.expect("stored the Z.AI Coding CN API key", 15)
            if FAKE_KEY.encode() in term.raw:
                print("  \u2717 the key was written to the terminal")
                ok = False
            if FAKE_KEY in term.text():
                print("  \u2717 the key was shown in the clear")
                ok = False
            settings = json.load(
                open(os.path.join(home, ".caocli", "settings.json"))
            )
            if settings["providers"]["zai-coding-cn"]["api_key"] != FAKE_KEY:
                print("  \u2717 the key did not reach settings.json")
                ok = False
            if settings["providers"]["deepseek"]["api_key"] != seeded:
                print("  \u2717 logging in dropped another provider's key")
                ok = False
            for path in (SEED + ".jsonl",):
                log = open(os.path.join(home, ".caocli", "sessions", path)).read()
                if FAKE_KEY in log:
                    print("  \u2717 the key reached the session log")
                    ok = False

            if is_live:
                ok &= live(term, home)

            # Leaving: the process ends, and the terminal is handed back -- raw mode
            # off and the alternate screen gone -- rather than left in the state the
            # front end put it in. Sent again while it is not taken, because a key
            # that arrives during a turn would be queued behind it instead.
            term.idle(2.0, 120)
            status = None
            deadline = time.time() + 90
            while time.time() < deadline and status is None:
                term.send("/exit\r")
                sent = time.time()
                while time.time() < sent + 5:
                    got = os.waitpid(pid, os.WNOHANG)
                    if got[0] == pid:
                        status = os.waitstatus_to_exitcode(got[1])
                        break
                    term.pump(0.2)
            if status is None:
                print("  ✗ process did not exit in time")
                os.kill(pid, signal.SIGKILL)
                ok = False
            elif status != 0:
                print(f"  ✗ exit code {status}")
                ok = False
            # Raw mode is process-wide, so a front end that forgets to give it
            # back leaves the shell without echo; the alternate screen would hide
            # everything the user had on it. The child is gone, so what can be
            # checked here is that it said both, on the way out.
            if b"\x1b[?25h" not in term.raw:
                print("  ✗ the cursor was never shown again on the way out")
                ok = False
            if b"\x1b[?1049l" not in term.raw:
                print("  ✗ the alternate screen was never left")
                ok = False
            # The mouse as it was found, too: a terminal still reporting it hands
            # nothing of the wheel to the shell that follows.
            if b"\x1b[?1000l" not in term.raw:
                print("  ✗ the mouse was never given back")
                ok = False
        finally:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    print(f"tui smoke: {'✅ pass' if ok else '❌ fail'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
