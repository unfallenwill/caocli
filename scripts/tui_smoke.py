#!/usr/bin/env python3
"""Interactive front-end smoke test: run caocli inside a pseudo-terminal that
answers the cursor query an inline viewport needs.

`pty_smoke.py` deliberately does not answer it, so it covers the fallback to the
plain prompt. This one does, which is the only way to exercise the viewport
itself: the input box, the pinned status line, committing a finished line into
scrollback, and leaving the terminal as it was found.

Only local commands are sent -- no provider is called -- so a placeholder API key
is enough and the test stays offline. Exit code 0 = pass.

With `SMOKE_LIVE=1` and a real key, it goes on to a live turn, which is the only
way to see the parts that need a model to move: the spinner that runs while the
turn does, and the approval gate that asks before a tool runs.
"""

import fcntl
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

BIN = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "caocli")
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
ROWS, COLS = 24, 80

# ANSI escapes, stripped before matching: a line of text is written with a cursor
# move and a style change around it, so a needle would otherwise be split across
# escape sequences.
ESCAPES = re.compile(rb"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[=>]|\x1b\][^\x07]*\x07")

# Rendered by the interactive front end and by nothing else: the plain prompt has
# no box, no placeholder and no pinned line.
VIEWPORT = "type a message"
STATUS = "cache"
PICKER = "show this"  # the /help row of the command picker
HELP = "Commands:"  # the first line of what /help commits

# Live marks: the spinner the status line moves on its own, and the gate that
# stands between a tool call and its execution.
SPINNER = "thinking"
SPINNER_FRAMES = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
GATE = "run it? [y/N]"


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


class Terminal:
    """The other end of the pty: it reads, and answers what a terminal answers."""

    def __init__(self, master: int):
        self.master = master
        self.raw = b""
        self.queries = 0

    def text(self) -> str:
        return ESCAPES.sub(b"", self.raw).decode("utf-8", "replace")

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
        # `\x1b[6n` is the terminal being asked where the cursor is. The viewport
        # cannot place itself without an answer, so it is answered with a row
        # that leaves it room to lay out above the current line. Answering is the
        # whole point of this harness: without it the front end declines and the
        # plain prompt runs instead.
        for _ in range(chunk.count(b"\x1b[6n")):
            os.write(self.master, f"\x1b[{ROWS - 4};1R".encode())
            self.queries += 1
        return True

    def expect(self, needle: str, timeout: float, quiet: bool = False) -> bool:
        end = time.time() + timeout
        next_note = time.time() + 15
        while True:
            if needle in self.text():
                if not quiet:
                    print(f"  ✓ {needle!r}")
                return True
            if time.time() >= end:
                if not quiet:
                    print(f"  ✗ timed out waiting for {needle!r}")
                    print(f"    tail seen: {self.text()[-400:]!r}")
                return False
            if time.time() >= next_note:
                # A live turn spends a long time waiting on a model, so silence
                # would be indistinguishable from a hang.
                print(f"  · still waiting for {needle!r}")
                next_note = time.time() + 15
            self.pump(0.5)

    def spinner_moved(self, timeout: float) -> bool:
        """Watch the status line: a spinner that only ever shows one frame is not
        a spinner, and this is the only path that runs it."""
        seen = set()
        end = time.time() + timeout
        while time.time() < end and len(seen) < 3:
            seen |= {c for c in self.text() if c in SPINNER_FRAMES}
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
        pid, master = spawn(home, ["--ask"] if is_live else [])
        term = Terminal(master)
        try:
            # Startup: the box, the pinned line. Both are drawn before the first
            # key, so seeing them is seeing that the viewport was entered.
            ok &= term.expect(VIEWPORT, 30)
            ok &= term.expect(STATUS, 15)
            if term.queries == 0:
                print("  ✗ the viewport never asked where the cursor was")
                ok = False

            # Typing a command prefix opens the picker, which draws over the live
            # area: it is the one widget that is not part of either the
            # transcript or the box.
            term.quiet(2.0, 30)
            term.send("/he")
            ok &= term.expect(PICKER, 15)

            # Completing and submitting it: the answer is committed into
            # scrollback, above the viewport, in the terminal's own history.
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

            if is_live:
                ok &= live(term, home)

            # Leaving: the process ends, and the terminal is handed back (raw mode
            # off) rather than left in the state the viewport put it in. Sent again
            # while it is not taken, because a key that arrives during a turn is
            # dropped rather than queued.
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
            # back leaves the shell without echo. The child is gone; what can be
            # checked here is that it said it was leaving, which it does by
            # showing the cursor again.
            if b"\x1b[?25h" not in term.raw:
                print("  ✗ the cursor was never shown again on the way out")
                ok = False
        finally:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    print(f"tui smoke: {'✅ pass' if ok else '❌ fail'} ({term.queries} cursor queries answered)")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
