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

/// 流式渲染器。思维链与正文是两个独立渲染块：
/// - 思维链灰色（DIM），正文正常色
/// - 块与块之间换行分隔；思维链转正文时额外空一行
/// - NO_COLOR 环境变量或非 TTY 时不发色码，但块分隔保留
pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    mode: Mode,
}

impl Renderer {
    pub fn new() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        Self {
            out: Box::new(std::io::stdout()),
            color,
            mode: Mode::Idle,
        }
    }

    #[cfg(test)]
    fn with_buffer(color: bool) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let r = Self {
            out: Box::new(SharedBuf(buf.clone())),
            color,
            mode: Mode::Idle,
        };
        (r, buf)
    }

    fn raw(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes());
        let _ = self.out.flush();
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("{code}{s}{RESET}")
        } else {
            s.to_owned()
        }
    }

    /// 结束当前块：复位颜色 + 换行；blank=true 时再补一个空行（块间距）。
    fn close_block(&mut self, blank: bool) {
        if self.mode != Mode::Idle {
            if self.color {
                self.raw(RESET);
            }
            self.raw("\n");
            if blank {
                self.raw("\n");
            }
        }
    }

    pub fn reasoning_delta(&mut self, s: &str) {
        if self.mode != Mode::Reasoning {
            self.close_block(self.mode == Mode::Content);
            if self.color {
                self.raw(DIM);
            }
            self.mode = Mode::Reasoning;
        }
        self.raw(s);
    }

    pub fn content_delta(&mut self, s: &str) {
        if self.mode != Mode::Content {
            self.close_block(self.mode == Mode::Reasoning);
            self.mode = Mode::Content;
        }
        self.raw(s);
    }

    /// 一轮流式输出结束，复位到 Idle。
    pub fn finish_turn(&mut self) {
        self.close_block(false);
        self.mode = Mode::Idle;
    }

    /// 工具调用回显：黄色工具名 + 提取出的 shell 命令。
    pub fn tool_start(&mut self, name: &str, args: &str) {
        let hint = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| {
                v.get("command")
                    .or_else(|| v.get("file_path"))
                    .and_then(|c| c.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| args.chars().take(80).collect());
        self.raw(&format!(
            "\n{}",
            self.paint(YELLOW, &format!("▸ {name} {hint}"))
        ));
        self.raw("\n");
    }

    /// 工具结果摘要：exit 行 + 字节数。
    pub fn tool_result(&mut self, result: &str) {
        let exit_line = result.lines().next().unwrap_or("").to_owned();
        let total = result.len();
        self.raw(&self.paint(DIM, &format!("{exit_line} · {total} bytes")));
        self.raw("\n");
    }

    pub fn usage(&mut self, u: &Usage) {
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

    pub fn info(&mut self, s: &str) {
        self.raw(&self.paint(DIM, s));
        self.raw("\n");
    }

    pub fn error(&self, s: &str) {
        eprintln!("{}", self.paint("\x1b[31m", s));
    }
}

#[cfg(test)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_then_content_are_separate_blocks() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("thinking...");
        r.content_delta("answer");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\x1b[2mthinking...\x1b[0m\n\nanswer\x1b[0m\n"
        );
    }

    #[test]
    fn content_then_reasoning_second_subturn_separated() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.content_delta("partial");
        r.reasoning_delta("more thinking");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "partial\x1b[0m\n\n\x1b[2mmore thinking\x1b[0m\n"
        );
    }

    #[test]
    fn no_color_still_separates_blocks() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.reasoning_delta("thought");
        r.content_delta("text");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "thought\n\ntext\n"
        );
    }

    #[test]
    fn content_only_has_no_leading_separator() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.content_delta("hi");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "hi\x1b[0m\n"
        );
    }

    #[test]
    fn reasoning_only_block_closes_cleanly() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("hmm");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\x1b[2mhmm\x1b[0m\n"
        );
    }
}
