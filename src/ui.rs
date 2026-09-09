use std::io::{IsTerminal, Write};

use crate::types::Usage;

const DIM: &str = "\x1b[2m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Reasoning,
    Content,
}

/// 流式渲染器。思维链灰色（DIM），正文正常色；工具调用黄色回显。
/// NO_COLOR 环境变量或非 TTY 时禁用 ANSI 码。
pub struct Renderer {
    mode: Mode,
    color: bool,
}

impl Renderer {
    pub fn new() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        Self {
            mode: Mode::Idle,
            color,
        }
    }

    fn raw(&self, s: &str) {
        print!("{s}");
        let _ = std::io::stdout().flush();
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("{code}{s}{RESET}")
        } else {
            s.to_owned()
        }
    }

    pub fn reasoning_delta(&mut self, s: &str) {
        match self.mode {
            Mode::Content => {
                if self.color {
                    self.raw(DIM);
                }
            }
            Mode::Idle => {
                if self.color {
                    self.raw(DIM);
                }
            }
            Mode::Reasoning => {}
        }
        self.mode = Mode::Reasoning;
        self.raw(s);
    }

    pub fn content_delta(&mut self, s: &str) {
        if self.mode == Mode::Reasoning && self.color {
            self.raw(RESET);
        }
        self.mode = Mode::Content;
        self.raw(s);
    }

    /// 一轮输出结束：复位样式，如有输出则换行。
    pub fn finish_turn(&mut self) {
        if self.mode != Mode::Idle {
            if self.color {
                self.raw(RESET);
            }
            self.raw("\n");
        }
        self.mode = Mode::Idle;
    }

    /// 工具调用回显：黄色工具名 + 提取出的 shell 命令。
    pub fn tool_start(&self, name: &str, args: &str) {
        let cmd = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(str::to_owned))
            .unwrap_or_else(|| args.chars().take(80).collect());
        self.raw(&format!(
            "\n{}",
            self.paint(YELLOW, &format!("▸ {name} {cmd}"))
        ));
        self.raw("\n");
    }

    /// 工具结果摘要：exit 行 + 字节数。
    pub fn tool_result(&self, result: &str) {
        let exit_line = result.lines().next().unwrap_or("").to_owned();
        let total = result.len();
        self.raw(&self.paint(DIM, &format!("{exit_line} · {total} bytes")));
        self.raw("\n");
    }

    pub fn usage(&self, u: &Usage) {
        let s = format!(
            "tokens: in {}/{} (hit {}/miss {}) · out {}",
            u.prompt_tokens,
            u.total_tokens,
            u.prompt_cache_hit_tokens,
            u.prompt_cache_miss_tokens,
            u.completion_tokens
        );
        self.raw(&self.paint(DIM, &s));
        self.raw("\n");
    }

    pub fn info(&self, s: &str) {
        self.raw(&self.paint(DIM, s));
        self.raw("\n");
    }

    pub fn error(&self, s: &str) {
        eprintln!("{}", self.paint("\x1b[31m", s));
    }
}
