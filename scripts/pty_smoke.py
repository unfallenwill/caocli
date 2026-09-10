#!/usr/bin/env python3
"""pty 交互冒烟：伪终端里跑 REPL，验证只有 TTY 才会执行的分支。

覆盖：状态栏（scroll region + cache 标签）、提示符往返、
回合中 Ctrl-C 优雅取消（cooked 模式 SIGINT → 取消标记 → 回到提示符）。
退出码 0 = 通过。需要 DEEPSEEK_API_KEY（真实 API）。
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


def spawn():
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    # 新分配的 pty termios 默认全零：ISIG/ICANON/ECHO 都没开。
    # 必须显式设成常规 cooked 模式，否则 \x03 不会转成 SIGINT，
    # 与真实用户终端的行为不一致。
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
        os.execvp(BIN, ["caocli"])
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
        print(f"  ✗ 超时等待 {needle!r}；已见尾部: {self.buf[-300:]!r}")
        return False

    def send(self, data: bytes):
        os.write(self.master, data)


def main() -> int:
    if not os.path.exists(BIN):
        print(f"✗ 未找到 {BIN}，先 cargo build")
        return 1
    pid, master = spawn()
    scr = Screen(master)
    ok = True
    try:
        ok &= scr.expect("caocli · 会话", 30)          # 启动提示行
        ok &= scr.expect("› ", 15)                      # 提示符
        ok &= scr.expect("cache", 15)                   # 状态栏 TTY 分支
        scr.send("用 Bash 工具执行 sleep 20\r".encode())  # \r = Enter 提交
        ok &= scr.expect("▸ Bash", 60)                  # 工具回显（流式已开始）
        # 注意：不能往 pty 写 \x03——ISIG 会把 SIGINT 发给整个前台进程组，
        # bash/sleep 先死、execute 带结果完成，biased select 会保留结果。
        # 这里只对 caocli 进程发 SIGINT，子进程由 kill_on_drop 负责收割。
        os.kill(pid, signal.SIGINT)
        ok &= scr.expect("已中断", 20)                  # 取消 Notice
        # 必须等「新的」提示符：expect 扫的是累计缓冲，取消前残留的 ›
        # 会立即假阳性，导致 /exit 在 readline 切 raw 模式前发出——
        # cooked 模式的 ICRNL 把 \r 转成 \n，rustyline 当 Ctrl-J（换行）
        # 而非 Enter，永远等不到提交。新提示符在 raw 模式之后渲染，
        # 因此以「最后一次已中断」为基准找其后的 › 才是真正就绪的信号。
        # （不能以 expect 返回时的 len(buf) 为基准：收尾只有 ~2ms，
        # ⏹ 和提示符常落在同一读块里，基准会越过提示符。）
        fresh = False
        deadline = time.time() + 15
        while time.time() < deadline:
            i = scr.buf.rfind("已中断".encode())
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
            print("  ✗ 取消后未见新提示符")
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
            print("  ✗ 进程未在期限内退出")
            os.kill(pid, signal.SIGKILL)
            ok = False
        elif status != 0:
            print(f"  ✗ 退出码 {status}")
            ok = False
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    print("pty 冒烟:", "✅ 通过" if ok else "❌ 失败")
    return 0 if ok else 1


import signal  # noqa: E402  (waitpid 兜底杀进程用)

if __name__ == "__main__":
    sys.exit(main())
