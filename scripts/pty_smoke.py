#!/usr/bin/env python3
"""pty interactive smoke test: run the plain front end inside a pseudo-terminal to
exercise the branches that only execute on a TTY.

Covers: the status bar (scroll region + cache label), the prompt round trip, and
graceful Ctrl-C cancellation during a turn (cooked-mode SIGINT -> cancellation
marker -> back to the prompt). The front end that owns the screen is
`tui_smoke.py`'s subject, so `--no-tui` picks this one out.
Exit code 0 = pass. Requires DEEPSEEK_API_KEY (real API).
"""

import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

BIN = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "caocli")
ROWS, COLS = 24, 80

# Exact notices rendered by the UI, matched as whole strings rather than
# fragments so that model output cannot trigger a false positive.
STARTUP = "caocli · session"
INTERRUPTED = "⏹ interrupted (Ctrl-C)"


def spawn():
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    # A freshly allocated pty has an all-zero termios: ISIG/ICANON/ECHO are all
    # off. It has to be set to normal cooked mode explicitly, otherwise \x03 is
    # not translated into SIGINT, which would not match a real user terminal.
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
        os.execvp(BIN, ["caocli", "--no-tui"])
    os.close(slave)
    return pid, master


class Screen:
    def __init__(self, master):
        self.master = master
        self.buf = b""

    def expect(self, needle: str, timeout: float) -> bool:
        target = needle.encode()
        end = time.time() + timeout
        while time.time() < end:
            if target in self.buf:
                return True
            r, _, _ = select.select([self.master], [], [], 0.5)
            if self.master in r:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    return False
                if not chunk:
                    return False
                self.buf += chunk
        print(f"  ✗ timed out waiting for {needle!r}; tail seen: {self.buf[-300:]!r}")
        return False

    def send(self, data: bytes):
        os.write(self.master, data)


def main() -> int:
    if not os.path.exists(BIN):
        print(f"✗ {BIN} not found, run cargo build first")
        return 1
    pid, master = spawn()
    scr = Screen(master)
    ok = True
    try:
        ok &= scr.expect(STARTUP, 30)                   # startup banner line
        ok &= scr.expect("› ", 15)                      # prompt
        ok &= scr.expect("cache", 15)                   # status bar TTY branch
        scr.send("use the Bash tool to run sleep 20\r".encode())  # \r = Enter submits
        ok &= scr.expect("▸ Bash", 60)                  # tool echo (streaming has started)
        # Note: do not write \x03 to the pty — ISIG sends SIGINT to the whole
        # foreground process group, so bash/sleep die first and execute completes
        # with a result, which the biased select then keeps.
        # Here SIGINT goes to the caocli process only; kill_on_drop reaps the child.
        os.kill(pid, signal.SIGINT)
        ok &= scr.expect(INTERRUPTED, 20)               # cancellation notice
        # The "new" prompt must be awaited: expect scans the accumulated buffer, so
        # a leftover › from before the cancellation would immediately give a false
        # positive and /exit would be sent before readline switches to raw mode —
        # cooked-mode ICRNL turns \r into \n, rustyline treats it as Ctrl-J
        # (newline) rather than Enter, and the submission never arrives. The new
        # prompt is rendered after raw mode is entered, so anchoring on the last
        # interruption and looking for a › after it is the real ready signal.
        # (Anchoring on len(buf) at the moment expect returns does not work: the
        # wind-down takes only ~2ms, so ⏹ and the prompt usually land in the same
        # read chunk and the anchor would skip past the prompt.)
        fresh = False
        deadline = time.time() + 15
        while time.time() < deadline:
            i = scr.buf.rfind(INTERRUPTED.encode())
            if i >= 0 and "› ".encode() in scr.buf[i:]:
                fresh = True
                break
            r, _, _ = select.select([master], [], [], 0.5)
            if master in r:
                try:
                    scr.buf += os.read(master, 65536)
                except OSError:
                    break
        if not fresh:
            print("  ✗ no fresh prompt after cancellation")
            ok = False
        scr.send("/exit\r".encode())
        deadline = time.time() + 15
        status = None
        while time.time() < deadline:
            got = os.waitpid(pid, os.WNOHANG)
            if got[0] == pid:
                status = os.waitstatus_to_exitcode(got[1])
                break
            time.sleep(0.2)
        if status is None:
            print("  ✗ process did not exit in time")
            os.kill(pid, signal.SIGKILL)
            ok = False
        elif status != 0:
            print(f"  ✗ exit code {status}")
            ok = False
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    print("pty smoke:", "✅ pass" if ok else "❌ fail")
    return 0 if ok else 1


import signal  # noqa: E402  (used by the waitpid fallback that kills the process)

if __name__ == "__main__":
    sys.exit(main())
