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
its own, and the approval gate that asks before a tool runs.
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
HELP = "Commands:"  # the first line of what /help commits

# Live marks: the state word the working line carries, the frames it moves through
# on its own, and the gate that stands between a tool call and its execution.
SPINNER = "thinking"
SPINNER_FRAMES = "·✢✳✶✽✻"
GATE = "run it? [y/N]"

# A session with an edit already in it. Resuming it draws the change, which is the
# replay half of the same cell a live turn produces; the id and the lines are what
# the assertions below look for.
SEED = "20260910-120000"


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
        env.setdefault("DEEPSEEK_API_KEY", "smoke-test-placeholder")
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

    def working_row(self) -> str:
        """The working line: the row above the tip line, which is the row above the
        input box."""
        rows = self.screen.lines()
        for y in range(len(rows) - 1, 1, -1):
            if rows[y].startswith("┌"):
                return rows[y - 2]
        return ""

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

    def box_rows(self) -> list[str]:
        """The input box as drawn: its border rows and what is between them, which
        is what says how tall it is."""
        rows = self.screen.lines()
        top = next(
            (y for y in range(len(rows) - 1, 1, -1) if rows[y].startswith("┌")), None
        )
        if top is None:
            return []
        bottom = next(
            (y for y in range(top, len(rows)) if rows[y].startswith("└")), top
        )
        return rows[top : bottom + 1]

    def spinner_moved(self, timeout: float) -> bool:
        """Watch the working line's first column: a spinner that only ever shows
        one frame is not a spinner, and this is the only path that runs one."""
        seen = set()
        end = time.time() + timeout
        while time.time() < end and len(seen) < 3:
            head = self.working_row()[:1]
            if head in SPINNER_FRAMES:
                seen.add(head)
            self.pump(0.2)
        if len(seen) < 3:
            print(f"  ✗ the spinner never advanced: {sorted(seen)}")
            return False
        return True

    def quiet(self, silence: float, timeout: float) -> bool:
        """Wait until the terminal has been silent for `silence` seconds.

        A running turn repaints the status line on every tick, so a still
        terminal is an idle one -- and this is the signal that says a key can be
        sent: one pressed during a turn is dropped by design, because a turn is
        not the place to start composing the next line. A line sent without
        waiting therefore arrives half-eaten, and the fragment left in the box is
        what gets submitted.
        """
        end = time.time() + timeout
        last = time.time()
        while time.time() < end:
            if self.pump(0.2):
                last = time.time()
            elif time.time() - last >= silence:
                return True
        print("  ✗ the terminal never went quiet")
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
    # The status line reports the turn while it runs, on a clock of its own: it
    # moves without anything else about the screen changing.
    # Note: what the user said is in here, but it is not asserted on screen. A
    # turn that fails or answers quickly writes more than the window shows before
    # the first frame is painted -- and the screen is written incrementally, so a
    # line that was never painted never appears in the byte stream either. The
    # cell a submitted line produces is covered by unit tests and by the resumed
    # turn above, which draws one out of the log.
    term.send(f"use the Read tool to read {ROOT}/Cargo.toml\r")
    ok &= term.expect(SPINNER, 15)
    ok &= term.spinner_moved(5)
    ok &= term.expect("▸ Read", 180)  # the call itself, once the turn gets there

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
    for attempt in (1, 2):
        term.send(f"Run the shell command `touch {marker}` with the Bash tool, then stop.\r")
        if term.expect(GATE, 120, quiet=attempt > 1):
            term.send("y\r")  # the answer comes out of the box, like any line
            return ok and term.wait_for_file(marker, 90)
        print(f"  · no Bash call on attempt {attempt}")
        if not term.quiet(2.0, 60):
            return False
    print("  ✗ no tool call ever needed approval")
    return False


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
        pid, master = spawn(home, ["-c", *(["--ask"] if is_live else [])])
        term = Terminal(master)
        try:
            # Startup: the box, the pinned line. Both are drawn before the first
            # key, so seeing them is seeing that the screen was entered -- and the
            # alternate screen is the whole reason none of it reaches the terminal's
            # own scrollback.
            ok &= term.expect(VIEWPORT, 30)
            ok &= term.expect(STATUS, 15)
            ok &= term.expect("⎿  Tip:", 15)
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
            # The thinking of that turn is drawn as a block with a ground of its
            # own, not as faint text: this is the terminal being told to paint one.
            ok &= term.expect("weighing the change", 15)
            if b"48;5;236" not in term.raw:
                print("  ✗ the thinking block has no ground")
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
            term.quiet(2.0, 30)
            empty = term.box_rows()
            term.send("alpha\nbeta")  # Ctrl-J between the two lines
            ok &= term.expect("beta", 15)
            grown = term.box_rows()
            if len(grown) != len(empty) + 1 or not any("alpha" in row for row in grown):
                print(f"  ✗ Ctrl-J did not grow the box: {grown}")
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
            term.quiet(2.0, 30)
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
            ok &= term.quiet(2.0, 30)
            term.send("/new\r")
            ok &= term.expect("new session", 30)
            ok &= term.quiet(2.0, 30)
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
            ok &= term.quiet(2.0, 30)
            if not term.screen.find("Startup flags"):
                print("  ✗ /help did not reach the screen")
                ok = False
            if term.screen.find(BANNER):
                print("  ✗ the transcript did not scroll: the banner is still up")
                ok = False
            term.send("\x1b[5~")  # PageUp
            ok &= term.expect(BANNER, 15)

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
            ok &= term.quiet(1.0, 30)
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

            if is_live:
                ok &= live(term, home)

            # Leaving: the process ends, and the terminal is handed back -- raw mode
            # off and the alternate screen gone -- rather than left in the state the
            # front end put it in. Sent again while it is not taken, because a key
            # that arrives during a turn is dropped rather than queued.
            term.quiet(2.0, 120)
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
