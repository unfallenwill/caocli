#!/usr/bin/env python3
"""pty interactive smoke test: run the plain front end inside a pseudo-terminal to
exercise the branches that only execute on a TTY.

Covers: the status bar (scroll region + cache label), the prompt round trip,
graceful Ctrl-C cancellation during a turn (cooked-mode SIGINT -> cancellation
marker -> back to the prompt), and `/login` -- which is the one place the plain
front end asks a question of its own, with the terminal's echo off. The front end
that owns the screen is `tui_smoke.py`'s subject, so `--no-tui` picks this one
out.
Exit code 0 = pass. The live half needs a real API key, which lives in
`settings.json` and nowhere else: it is taken from the one `/login` stored on
this machine and written into the run's own HOME. With none, the live half is
skipped.
"""

import fcntl
import json
import os
import pty
import select
import struct
import sys
import tempfile
import termios
import time

BIN = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "caocli")
ROWS, COLS = 24, 80

# Exact notices rendered by the UI, matched as whole strings rather than
# fragments so that model output cannot trigger a false positive.
STARTUP = "caocli · session"
INTERRUPTED = "⏹ interrupted (Ctrl-C)"

# A key for the login case: nonsense, because it is never sent anywhere. Looking
# for exactly it is how the case knows that what it found is what was typed.
FAKE_KEY = "sk-pty-smoke-not-a-real-key"


def api_key() -> str | None:
    """A key to run a real turn with: the one `/login` stored on this machine."""
    try:
        with open(os.path.expanduser("~/.caocli/settings.json")) as f:
            return json.load(f)["providers"]["deepseek"]["api_key"]
    except (OSError, KeyError, ValueError):
        return None


def seed_settings(home: str, key: str) -> None:
    """Write the file `/login` writes, so the run has a key to find and the
    user's own settings are neither read nor written."""
    directory = os.path.join(home, ".caocli")
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, "settings.json"), "w") as f:
        json.dump({"providers": {"deepseek": {"api_key": key}}}, f)


def spawn(home: str | None = None, extra: list[str] | None = None):
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
        env = dict(os.environ)
        # A HOME of the run's own keeps the run out of the user's sessions and
        # settings, and makes what it finds in its own files its own doing.
        if home is not None:
            env["HOME"] = home
        os.execve(BIN, ["caocli", "--no-tui", *(extra or [])], env)
    os.close(slave)
    return pid, master


# The plain prompt, as bytes: what rustyline draws when it is reading.
PROMPT = "› ".encode()


class Screen:
    def __init__(self, master):
        self.master = master
        self.buf = b""
        # How far the buffer had been read when the last expected thing was
        # found: what `ready` measures a fresh prompt from.
        self.mark = 0

    def ready(self, timeout: float = 15) -> bool:
        """Wait for a prompt drawn after the last thing that was expected, which is
        what says the front end is reading rather than working.

        A line sent while it is not arrives while the terminal is cooked -- which
        is where rustyline leaves it between reads -- and there ICRNL turns the
        Enter into Ctrl-J, which rustyline inserts as a newline instead of
        submitting. The line then sits in the box forever, which reads exactly
        like a front end that is stuck.
        """
        start = self.mark
        end = time.time() + timeout
        while time.time() < end:
            if PROMPT in self.buf[start:]:
                return True
            r, _, _ = select.select([self.master], [], [], 0.5)
            if self.master in r:
                try:
                    self.buf += os.read(self.master, 65536)
                except OSError:
                    return False
        print(f"  ✗ no prompt; tail seen: {self.buf[start:][-300:]!r}")
        return False

    def expect(self, needle: str, timeout: float) -> bool:
        target = needle.encode()
        end = time.time() + timeout
        while time.time() < end:
            if target in self.buf:
                self.mark = self.buf.rindex(target) + len(target)
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


def login_case() -> bool:
    """`/login` in the plain front end: the menu, the hidden answer, and the file
    it is stored in. No API key is needed -- the session starts without one, which
    is the other half of what this checks."""
    ok = True
    with tempfile.TemporaryDirectory(prefix="caocli-pty-login-") as home:
        pid, master = spawn(home)
        scr = Screen(master)
        try:
            # Starting without a key says what to do about it rather than
            # refusing to start: `/login` is inside the session.
            ok &= scr.expect("no API key for DeepSeek", 30)
            ok &= scr.ready()
            scr.send("/login\r".encode())           # the menu, no provider named yet
            # The menu a person reads names the providers; the id beside each one
            # is what they type, which is why this front end shows both.
            ok &= scr.expect("Z.AI Coding CN", 15)
            ok &= scr.expect("run /login <provider id>", 15)
            ok &= scr.ready()
            scr.send("/login zai-coding-cn\r".encode())
            ok &= scr.expect("Z.AI Coding CN API key", 15)  # the question
            scr.send((FAKE_KEY + "\r").encode())
            ok &= scr.expect("stored the Z.AI Coding CN API key", 15)
            # The point of hiding it: it is nowhere on the terminal. (The
            # terminal echoed it while echo was on, which is why the front end
            # turns echo off before reading -- this is what says it did.)
            if FAKE_KEY.encode() in scr.buf:
                print("  ✗ the key was echoed by the terminal")
                ok = False
            settings = json.load(open(os.path.join(home, ".caocli", "settings.json")))
            if settings["providers"]["zai-coding-cn"]["api_key"] != FAKE_KEY:
                print("  ✗ the key did not reach settings.json")
                ok = False
            # A model is named by its provider, and the status line says so. The
            # needle is the status line's own segment: the line being typed is
            # echoed by the terminal, and the command line contains the model name
            # without ever having run anything.
            ok &= scr.ready()
            scr.send("/model zai-coding-cn/glm-5.3\r".encode())
            ok &= scr.expect("zai-coding-cn/glm-5.3 · cache", 15)
            ok &= scr.ready()
            scr.send("/model\r".encode())
            ok &= scr.expect("choose a model", 15)   # the menu, for the plain front end
            ok &= scr.ready()
            scr.send("/exit\r".encode())
            deadline = time.time() + 15
            while time.time() < deadline:
                got = os.waitpid(pid, os.WNOHANG)
                if got[0] == pid:
                    if os.waitstatus_to_exitcode(got[1]) != 0:
                        print("  ✗ login session exited non-zero")
                        ok = False
                    break
                time.sleep(0.2)
            else:
                print("  ✗ login session did not exit in time")
                ok = False
        finally:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    return ok


def main() -> int:
    if not os.path.exists(BIN):
        print(f"✗ {BIN} not found, run cargo build first")
        return 1
    ok = login_case()
    key = api_key()
    if key is None:
        print(
            "  · no API key stored by /login:"
            " the live half is skipped"
        )
        print("pty smoke:", "✅ pass" if ok else "❌ fail")
        return 0 if ok else 1
    with tempfile.TemporaryDirectory(prefix="caocli-pty-") as home:
        seed_settings(home, key)
        return live_case(home, ok)


def live_case(home: str, ok: bool) -> int:
    pid, master = spawn(home)
    scr = Screen(master)
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
