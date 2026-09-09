use std::io::{IsTerminal, Write};

use crate::types::{Message, Role, Usage};

const DIM: &str = "\x1b[2m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// 会话级缓存统计（状态栏用）。累加本次进程内每个子请求的 hit/miss。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hit: u64,
    pub miss: u64,
}

impl CacheStats {
    pub fn record(&mut self, u: &Usage) {
        // 归一化：DeepSeek 扁平字段 / GLM 嵌套 details 都在 Usage::cache() 里收敛。
        // 供应商不报告缓存时不记，避免把"未知"显示成 0% 命中。
        if let Some(c) = u.cache() {
            self.hit += c.hit;
            self.miss += c.miss;
        }
    }

    /// 命中率百分比；尚无数据时 None。
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hit + self.miss;
        (total > 0).then(|| self.hit as f64 * 100.0 / total as f64)
    }

    /// 状态栏文本（不含左填充）。
    pub fn label(&self) -> String {
        match self.hit_rate() {
            Some(rate) => format!("cache {rate:.1}% · hit {} · miss {}", self.hit, self.miss),
            None => "cache —".to_string(),
        }
    }
}

/// 终端尺寸（行, 列）。非 unix 或 ioctl 失败时 None。
#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: STDOUT_FILENO 是有效 fd，ws 是 TIOCGWINSZ 要求的 winsize 布局
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

#[cfg(not(unix))]
fn terminal_size() -> Option<(u16, u16)> {
    None
}

/// 底部固定状态栏：占用终端最后一行，滚动区域限制为 1..rows-1，
/// 所以输出滚动不会把状态栏顶掉。
/// 代价：滚动区域内的行滚出屏幕后不进终端回滚缓冲（历史需靠会话文件）。
/// 用 `--no-status-bar` 或非 TTY 时完全不启用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusBar {
    rows: u16,
    cols: u16,
}

impl StatusBar {
    /// 至少 3 行才启用：最后一行状态栏 + 至少一行输出区 + 一行余量。
    const MIN_ROWS: u16 = 3;

    /// `tty=false` 或 TERM=dumb 时直接放弃，不发 ioctl。
    fn detect(tty: bool) -> Option<Self> {
        if !tty || std::env::var("TERM").is_ok_and(|t| t == "dumb") {
            return None;
        }
        Self::from_size(terminal_size())
    }

    fn from_size(size: Option<(u16, u16)>) -> Option<Self> {
        match size {
            Some((rows, cols)) if rows >= Self::MIN_ROWS && cols > 1 => Some(Self { rows, cols }),
            _ => None,
        }
    }

    /// 可用宽度：留出最后一列，避免在末列写字触发自动换行。
    fn width(&self) -> usize {
        self.cols as usize - 1
    }

    /// 设滚动区域 → 光标移到区域底部。
    fn setup(&self, out: &mut dyn Write) {
        let _ = write!(
            out,
            "\x1b[{};1H\x1b[2K\x1b[1;{}r\x1b[{};1H",
            self.rows,
            self.rows - 1,
            self.rows - 1
        );
        let _ = out.flush();
    }

    /// 复位滚动区域 → 清掉状态栏行 → 换行，让 shell 提示符落在干净行。
    fn teardown(&self, out: &mut dyn Write) {
        let _ = write!(out, "\x1b[r\x1b[{};1H\x1b[2K\r\n", self.rows);
        let _ = out.flush();
    }

    /// 右对齐重绘：保存光标 → 清行 → 写填充+文本 → 恢复光标。
    /// `visible` 用于算宽度（不含色码），`painted` 是实际写出的内容。
    fn render(&self, out: &mut dyn Write, visible: &str, painted: &str) {
        let pad = self.width().saturating_sub(visible.chars().count());
        let _ = write!(
            out,
            "\x1b7\x1b[{};1H\x1b[2K{}{}\x1b8",
            self.rows,
            " ".repeat(pad),
            painted
        );
        let _ = out.flush();
    }
}

/// 按字符数截断，保证不会写超出状态栏宽度。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Reasoning,
    Content,
}

/// 机器 → UI 的通知词汇表（Notice 通道）。
/// 纪律：只收通知、不回传数据；实现不得阻塞，终端写失败视为致命。
/// 机器（agent）只依赖此 trait，不依赖具体渲染器；进程内唯一实现是 [`Renderer`]。
pub trait Ui {
    /// 思维链片段（先于正文到达）。
    fn reasoning_delta(&mut self, s: &str);
    /// 正文片段。
    fn content_delta(&mut self, s: &str);
    /// 一轮流式输出结束，块复位。
    fn finish_turn(&mut self);
    /// 工具开始执行：回显工具名与参数摘要。
    fn tool_start(&mut self, name: &str, args: &str);
    /// 工具结果摘要。
    fn tool_result(&mut self, result: &str);
    /// 子请求 token 用量（同时累计会话级缓存统计）。
    fn usage(&mut self, u: &Usage);
    /// 回合被用户取消（Ctrl-C）：闭合流式块，打出中断提示。
    fn interrupted(&mut self);
}

/// 流式渲染器。思维链与正文是两个独立渲染块：
/// - 思维链灰色（DIM），正文正常色
/// - 块与块之间换行分隔；思维链转正文时额外空一行
/// - NO_COLOR 环境变量或非 TTY 时不发色码，但块分隔保留
pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    mode: Mode,
    stats: CacheStats,
    /// 状态栏左段显示的模型 id（建立/切换会话时更新）。
    model: Option<String>,
    bar: Option<StatusBar>,
}

impl Renderer {
    pub fn new() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        Self {
            out: Box::new(std::io::stdout()),
            color,
            mode: Mode::Idle,
            stats: CacheStats::default(),
            model: None,
            bar: None,
        }
    }

    #[cfg(test)]
    fn with_buffer(color: bool) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let r = Self {
            out: Box::new(SharedBuf(buf.clone())),
            color,
            mode: Mode::Idle,
            stats: CacheStats::default(),
            model: None,
            bar: None,
        };
        (r, buf)
    }

    /// 按当前终端尺寸同步状态栏：REPL 启动时和每轮输入前调用，
    /// 顺带处理窗口缩放（尺寸变了就拆了重建）。
    pub fn refresh_status_bar(&mut self) {
        let current = StatusBar::detect(std::io::stdout().is_terminal());
        self.apply_status_bar(current);
    }

    fn apply_status_bar(&mut self, current: Option<StatusBar>) {
        match (self.bar, current) {
            (None, None) => {}
            (None, Some(bar)) => {
                bar.setup(self.out.as_mut());
                self.bar = Some(bar);
                self.redraw_status_bar();
            }
            (Some(old), None) => {
                old.teardown(self.out.as_mut());
                self.bar = None;
            }
            (Some(old), Some(bar)) if old == bar => self.redraw_status_bar(),
            (Some(old), Some(bar)) => {
                old.teardown(self.out.as_mut());
                bar.setup(self.out.as_mut());
                self.bar = Some(bar);
                self.redraw_status_bar();
            }
        }
    }

    /// 重绘状态栏（不重新探测尺寸）。
    fn redraw_status_bar(&mut self) {
        let Some(bar) = self.bar else { return };
        let visible = truncate_chars(&self.status_label(), bar.width());
        let painted = self.paint(DIM, &visible);
        bar.render(self.out.as_mut(), &visible, &painted);
    }

    /// 状态栏文本：`<model> · cache ...`；模型未知时退化为纯缓存统计。
    fn status_label(&self) -> String {
        let cache = self.stats.label();
        match &self.model {
            Some(m) if !m.is_empty() => format!("{m} · {cache}"),
            _ => cache,
        }
    }

    /// 设置状态栏显示的模型 id（建立/切换会话时调用，随会话 meta 变化）。
    pub fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_owned());
        self.redraw_status_bar();
    }

    /// 当前会话累计的缓存统计（状态栏数据源）。仅测试读取。
    #[cfg(test)]
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// 切换会话时清零缓存统计。
    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
        self.redraw_status_bar();
    }

    /// 退出前还原终端（复位滚动区域、清掉状态栏行）。幂等。
    pub fn teardown(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.teardown(self.out.as_mut());
        }
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

    /// 回放历史消息（恢复会话时用），样式与实时渲染保持一致：
    /// 用户消息带 `›` 前缀，assistant 思维链灰色、正文正常、工具调用黄色，
    /// tool 消息只显示摘要（与实时一致，不刷 10KB 原文）。
    /// 会话文件不含 system 消息（SYSTEM_PROMPT 是编译期常量），无需过滤。
    pub fn replay(&mut self, messages: &[Message]) {
        for m in messages {
            match m.role {
                Role::User => {
                    if let Some(c) = &m.content {
                        self.raw("\n");
                        self.raw(&self.paint(DIM, "› "));
                        self.raw(c);
                        self.raw("\n");
                    }
                }
                Role::Assistant => {
                    if let Some(r) = &m.reasoning_content
                        && !r.is_empty()
                    {
                        self.raw(&self.paint(DIM, r));
                        self.raw("\n");
                    }
                    if let Some(c) = &m.content
                        && !c.is_empty()
                    {
                        self.raw(c);
                        self.raw("\n");
                    }
                    for call in m.tool_calls.iter().flatten() {
                        self.tool_start(&call.function.name, &call.function.arguments);
                    }
                }
                Role::Tool => {
                    if let Some(c) = &m.content {
                        self.tool_result(c);
                    }
                }
                Role::System => {}
            }
        }
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

impl Ui for Renderer {
    fn reasoning_delta(&mut self, s: &str) {
        if self.mode != Mode::Reasoning {
            self.close_block(self.mode == Mode::Content);
            if self.color {
                self.raw(DIM);
            }
            self.mode = Mode::Reasoning;
        }
        self.raw(s);
    }

    fn content_delta(&mut self, s: &str) {
        if self.mode != Mode::Content {
            self.close_block(self.mode == Mode::Reasoning);
            self.mode = Mode::Content;
        }
        self.raw(s);
    }

    fn finish_turn(&mut self) {
        self.close_block(false);
        self.mode = Mode::Idle;
    }

    fn tool_start(&mut self, name: &str, args: &str) {
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

    fn tool_result(&mut self, result: &str) {
        let exit_line = result.lines().next().unwrap_or("").to_owned();
        let total = result.len();
        self.raw(&self.paint(DIM, &format!("{exit_line} · {total} bytes")));
        self.raw("\n");
    }

    fn interrupted(&mut self) {
        self.close_block(false);
        self.mode = Mode::Idle;
        self.raw(&self.paint(YELLOW, "⏹ 已中断（Ctrl-C）"));
        self.raw("\n");
    }

    fn usage(&mut self, u: &Usage) {
        let cache = match u.cache() {
            Some(c) => format!("hit {}/miss {}", c.hit, c.miss),
            None => "cache —".to_string(),
        };
        let s = format!(
            "tokens: in {}/{} ({cache}) · out {}",
            u.prompt_tokens, u.total_tokens, u.completion_tokens
        );
        self.raw(&self.paint(DIM, &s));
        self.raw("\n");
        self.stats.record(u);
        self.redraw_status_bar();
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
    use crate::types::Usage;

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

    #[test]
    fn same_mode_deltas_do_not_reopen_block() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("a");
        r.reasoning_delta("b"); // 仍是 Reasoning：不重复发色码
        r.content_delta("x");
        r.content_delta("y"); // 仍是 Content：不换块
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\x1b[2mab\x1b[0m\n\nxy\x1b[0m\n"
        );
    }

    #[test]
    fn tool_start_extracts_command_hint() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.tool_start("Bash", r#"{"command":"ls -la"}"#);
        r.tool_start("Read", r#"{"file_path":"/a/b.txt"}"#);
        r.tool_start("Write", "not json at all");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("▸ Bash ls -la"), "{s}");
        assert!(s.contains("▸ Read /a/b.txt"), "{s}");
        assert!(s.contains("▸ Write not json at all"), "{s}"); // 坏 JSON 回落成原文
    }

    #[test]
    fn replay_renders_history_compactly_with_colors() {
        use crate::types::{ToolCall, ToolCallFunction};
        let (mut r, buf) = Renderer::with_buffer(true);
        r.replay(&[
            Message::user("帮我看看"),
            Message {
                role: Role::Assistant,
                content: Some("先执行".into()),
                reasoning_content: Some("想一下".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "Bash".into(),
                        arguments: r#"{"command":"ls -la"}"#.into(),
                    },
                }]),
                tool_call_id: None,
            },
            Message::tool("call_1", "exit_code: 0\n--- stdout ---\nSECRET_BODY"),
            Message::system("不该出现"),
        ]);
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("\x1b[2m› \x1b[0m帮我看看"), "{s}");
        assert!(s.contains("\x1b[2m想一下\x1b[0m"), "思维链灰色: {s}");
        assert!(s.contains("先执行"), "{s}");
        assert!(s.contains("▸ Bash ls -la"), "工具调用黄色: {s}");
        // tool 消息只给摘要，不刷全文
        assert!(s.contains("exit_code: 0 · 39 bytes"), "{s}");
        assert!(!s.contains("SECRET_BODY"), "不应回放工具全文: {s}");
        assert!(!s.contains("不该出现"), "system 消息不回放: {s}");
    }

    #[test]
    fn replay_empty_history_emits_only_blank_line() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.replay(&[]);
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\n"
        );
    }

    #[test]
    fn replay_skips_empty_assistant_fields() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.replay(&[Message {
            role: Role::Assistant,
            content: Some(String::new()),
            reasoning_content: Some(String::new()),
            tool_calls: None,
            tool_call_id: None,
        }]);
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\n"
        );
    }

    #[test]
    fn interrupted_closes_block_and_prints_notice() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("想");
        r.interrupted();
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("已中断"), "{s}");
        assert!(s.ends_with("\x1b[0m\n"), "复位颜色收尾: {s}");
    }

    #[test]
    fn tool_result_shows_exit_line_and_bytes() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.tool_result("exit_code: 3\n--- stdout ---\nhello");
        r.tool_result("");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("exit_code: 3 · 33 bytes"), "{s}");
        assert!(s.contains(" · 0 bytes"), "{s}"); // 空结果
    }

    #[test]
    fn usage_and_info_render_dims() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.usage(&Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            ..Default::default()
        });
        r.info("会话 abc");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("tokens: in 10/15 (hit 6/miss 4) · out 5"), "{s}");
        assert!(s.contains("会话 abc"), "{s}");
    }

    #[test]
    fn color_variants_render_codes_and_plain() {
        // color=true：info/usage 走 paint 的染色分支；error 走红色
        let (mut r, buf) = Renderer::with_buffer(true);
        r.info("ok");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(s, "\x1b[2mok\x1b[0m\n");
        r.error("boom"); // eprintln，不写 buf；仅保证不 panic 且覆盖 paint(红)
    }

    #[test]
    fn no_color_paint_returns_plain_text() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.info("plain");
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "plain\n"
        );
    }

    fn usage_fixture(hit: u64, miss: u64) -> Usage {
        Usage {
            prompt_tokens: hit + miss,
            completion_tokens: 0,
            total_tokens: hit + miss,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            ..Default::default()
        }
    }

    fn buf_of(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn cache_stats_accumulate_and_label() {
        let mut s = CacheStats::default();
        assert_eq!(s.hit_rate(), None);
        assert_eq!(s.label(), "cache —");
        s.record(&usage_fixture(6, 4));
        s.record(&usage_fixture(32378, 457));
        assert_eq!((s.hit, s.miss), (32384, 461));
        assert!((s.hit_rate().unwrap() - 98.6).abs() < 0.05, "{s:?}");
        assert_eq!(s.label(), "cache 98.6% · hit 32384 · miss 461");
    }

    #[test]
    fn cache_stats_read_glm_nested_details() {
        // GLM 形状：只给 prompt_tokens_details.cached_tokens，miss 需推导。
        let mut s = CacheStats::default();
        s.record(&Usage {
            prompt_tokens: 1200,
            completion_tokens: 300,
            total_tokens: 1500,
            prompt_tokens_details: Some(crate::types::PromptTokensDetails { cached_tokens: 800 }),
            ..Default::default()
        });
        assert_eq!((s.hit, s.miss), (800, 400));
        assert_eq!(s.label(), "cache 66.7% · hit 800 · miss 400");
    }

    #[test]
    fn cache_stats_ignore_provider_without_cache_reporting() {
        let mut s = CacheStats::default();
        s.record(&Usage {
            prompt_tokens: 100,
            ..Default::default()
        });
        assert_eq!(s.label(), "cache —");
    }

    #[test]
    fn status_bar_from_size_guards() {
        assert_eq!(StatusBar::from_size(None), None);
        assert_eq!(StatusBar::from_size(Some((2, 80))), None); // 行数不足
        assert_eq!(StatusBar::from_size(Some((24, 1))), None); // 宽度不足
        assert_eq!(
            StatusBar::from_size(Some((24, 80))),
            Some(StatusBar { rows: 24, cols: 80 })
        );
    }

    #[test]
    fn status_bar_detect_rejects_non_tty() {
        assert_eq!(StatusBar::detect(false), None);
    }

    #[test]
    fn truncate_chars_keeps_char_boundaries() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcd", 2), "ab");
        assert_eq!(truncate_chars("命中率", 2), "命中"); // 多字节不切坏
    }

    #[test]
    fn status_bar_setup_teardown_sequences() {
        let bar = StatusBar { rows: 10, cols: 40 };
        let (mut r, buf) = Renderer::with_buffer(false);
        bar.setup(r.out.as_mut());
        bar.teardown(r.out.as_mut());
        let s = buf_of(&buf);
        assert!(s.contains("\x1b[10;1H\x1b[2K\x1b[1;9r\x1b[9;1H"), "{s:?}");
        assert!(s.contains("\x1b[r\x1b[10;1H\x1b[2K\r\n"), "{s:?}");
    }

    #[test]
    fn status_bar_render_right_aligns_and_paints() {
        let bar = StatusBar { rows: 10, cols: 40 }; // width = 39
        let (mut r, buf) = Renderer::with_buffer(true);
        let label = "cache 98.6% · hit 32384 · miss 461"; // 34 字符
        let painted = r.paint(DIM, label);
        bar.render(r.out.as_mut(), label, &painted);
        let s = buf_of(&buf);
        // 39 - 34 = 5 个空格左填充，右对齐
        assert!(
            s.contains(&format!(
                "\x1b7\x1b[10;1H\x1b[2K     \x1b[2m{label}\x1b[0m\x1b8"
            )),
            "{s:?}"
        );
    }

    #[test]
    fn status_bar_render_truncates_to_width() {
        let bar = StatusBar { rows: 10, cols: 20 }; // width = 19
        let (mut r, buf) = Renderer::with_buffer(false);
        let visible = truncate_chars(
            &CacheStats {
                hit: 32384,
                miss: 461,
            }
            .label(),
            bar.width(),
        );
        bar.render(r.out.as_mut(), &visible, &visible);
        let s = buf_of(&buf);
        assert!(
            s.contains("\x1b[10;1H\x1b[2Kcache 98.6% · hit 3\x1b8"),
            "{s:?}"
        );
    }

    #[test]
    fn apply_status_bar_transitions() {
        let a = StatusBar { rows: 10, cols: 40 };
        let b = StatusBar { rows: 12, cols: 50 };

        // (None, None)：无输出
        let (mut r, buf) = Renderer::with_buffer(false);
        r.apply_status_bar(None);
        assert!(buf_of(&buf).is_empty());

        // (None, Some)：建栏 + 首绘
        r.apply_status_bar(Some(a));
        let s = buf_of(&buf);
        assert!(s.contains("\x1b[1;9r"), "{s:?}");
        assert!(s.contains("cache —"), "{s:?}");
        assert_eq!(r.bar, Some(a));

        // (Some, Some(相同))：只重绘
        let before = buf_of(&buf).len();
        r.apply_status_bar(Some(a));
        assert!(buf_of(&buf)[before..].contains("\x1b[10;1H\x1b[2K"));

        // (Some, Some(不同))：拆旧建新
        let before = buf_of(&buf).len();
        r.apply_status_bar(Some(b));
        let tail = &buf_of(&buf)[before..];
        assert!(tail.contains("\x1b[r"), "{tail:?}");
        assert!(tail.contains("\x1b[1;11r"), "{tail:?}");
        assert_eq!(r.bar, Some(b));

        // (Some, None)：拆栏
        let before = buf_of(&buf).len();
        r.apply_status_bar(None);
        assert!(buf_of(&buf)[before..].contains("\x1b[r"));
        assert_eq!(r.bar, None);
    }

    #[test]
    fn usage_paints_session_cache_bar_and_reset_clears_it() {
        let bar = StatusBar { rows: 10, cols: 60 };
        let (mut r, buf) = Renderer::with_buffer(false);
        r.apply_status_bar(Some(bar));
        r.usage(&usage_fixture(6, 4));
        r.usage(&usage_fixture(12, 8));
        let s = buf_of(&buf);
        assert!(
            s.contains("tokens: in 10/10 (hit 6/miss 4) · out 0"),
            "{s:?}"
        );
        // 会话累计：18 hit / 12 miss = 60.0%
        assert!(s.contains("cache 60.0% · hit 18 · miss 12"), "{s:?}");

        r.reset_stats();
        let tail = &buf_of(&buf)[s.len()..];
        assert!(tail.contains("cache —\x1b8"), "{tail:?}");
        assert_eq!(r.stats, CacheStats::default());
    }

    #[test]
    fn status_label_prepends_model_when_set() {
        let (mut r, _buf) = Renderer::with_buffer(false);
        assert_eq!(r.status_label(), "cache —");
        r.set_model("deepseek-v4-flash");
        assert_eq!(r.status_label(), "deepseek-v4-flash · cache —");
        r.usage(&usage_fixture(6, 4));
        assert_eq!(
            r.status_label(),
            "deepseek-v4-flash · cache 60.0% · hit 6 · miss 4"
        );
    }

    #[test]
    fn empty_model_falls_back_to_cache_only_label() {
        let (mut r, _buf) = Renderer::with_buffer(false);
        r.set_model("");
        assert_eq!(r.status_label(), "cache —");
    }

    #[test]
    fn status_bar_shows_model_and_updates_on_switch() {
        let bar = StatusBar { rows: 10, cols: 80 };
        let (mut r, buf) = Renderer::with_buffer(false);
        r.apply_status_bar(Some(bar));
        r.set_model("deepseek-v4-flash");
        r.usage(&usage_fixture(6, 4));
        let s = buf_of(&buf);
        assert!(
            s.contains("deepseek-v4-flash · cache 60.0% · hit 6 · miss 4"),
            "{s:?}"
        );

        // 切换模型：重绘后只剩新模型
        r.set_model("deepseek-v4-pro");
        let tail = &buf_of(&buf)[s.len()..];
        assert!(tail.contains("deepseek-v4-pro · cache 60.0%"), "{tail:?}");
        assert!(!tail.contains("deepseek-v4-flash"), "{tail:?}");

        // reset_stats 只清缓存统计，模型保留
        let before = buf_of(&buf).len();
        r.reset_stats();
        let tail = &buf_of(&buf)[before..];
        assert!(tail.contains("deepseek-v4-pro · cache —"), "{tail:?}");
    }

    #[test]
    fn refresh_status_bar_without_tty_is_noop_and_teardown_idempotent() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.refresh_status_bar(); // cargo test 的 stdout 是管道 → 不启用
        assert!(buf_of(&buf).is_empty());
        r.teardown(); // 未启用时幂等
        assert!(buf_of(&buf).is_empty());
    }
}
