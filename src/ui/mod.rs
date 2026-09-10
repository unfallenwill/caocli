mod text;

use std::io::{IsTerminal, Write};

use crate::types::{Message, Role, Usage};

const DIM: &str = "\x1b[2m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Session-level cache statistics (for the status bar). Accumulates the hit/miss
/// of every sub-request within this process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hit: u64,
    pub miss: u64,
}

impl CacheStats {
    pub fn record(&mut self, u: &Usage) {
        // Normalization: DeepSeek's flat fields and GLM's nested details both
        // converge in Usage::cache(). When a provider does not report caching,
        // nothing is recorded, so "unknown" is never displayed as 0% hit.
        if let Some(c) = u.cache() {
            self.hit += c.hit;
            self.miss += c.miss;
        }
    }

    /// Hit rate as a percentage; None while there is no data yet.
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hit + self.miss;
        (total > 0).then(|| self.hit as f64 * 100.0 / total as f64)
    }

    /// Status bar text (without the left padding).
    pub fn label(&self) -> String {
        match self.hit_rate() {
            Some(rate) => format!("cache {rate:.1}% · hit {} · miss {}", self.hit, self.miss),
            None => "cache —".to_string(),
        }
    }
}

/// Terminal size (rows, cols). None on non-unix or when the ioctl fails.
#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: STDOUT_FILENO is a valid fd, and ws has the winsize layout that
    // TIOCGWINSZ requires
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

#[cfg(not(unix))]
fn terminal_size() -> Option<(u16, u16)> {
    None
}

/// Fixed status bar at the bottom: it occupies the terminal's last line and the
/// scroll region is restricted to 1..rows-1, so scrolling output cannot push the
/// bar off the screen.
/// Cost: lines that scroll out of the scroll region never reach the terminal's
/// scrollback buffer (history has to come from the session file).
/// Not enabled at all with `--no-status-bar` or when stdout is not a TTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusBar {
    rows: u16,
    cols: u16,
}

impl StatusBar {
    /// Enabled only with at least 3 rows: the status bar line + at least one line
    /// of output area + one row of slack.
    const MIN_ROWS: u16 = 3;

    /// Give up immediately when `tty=false` or TERM=dumb, without issuing ioctl.
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

    /// Usable width: the last column is left free, so writing there cannot
    /// trigger automatic wrapping.
    fn width(&self) -> usize {
        self.cols as usize - 1
    }

    /// Set the scroll region → move the cursor to its bottom.
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

    /// Reset the scroll region → clear the status bar line → newline, so the
    /// shell prompt lands on a clean line.
    fn teardown(&self, out: &mut dyn Write) {
        let _ = write!(out, "\x1b[r\x1b[{};1H\x1b[2K\r\n", self.rows);
        let _ = out.flush();
    }

    /// Right-aligned redraw: save the cursor → clear the line → write padding +
    /// text → restore the cursor.
    /// `visible` is used to compute the width (it carries no color codes), while
    /// `painted` is what actually gets written. The measurement is in columns,
    /// not chars, so a wide character is charged for both of its columns.
    fn render(&self, out: &mut dyn Write, visible: &str, painted: &str) {
        let pad = self.width().saturating_sub(text::width(visible));
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Reasoning,
    Content,
}

/// The machine → UI notification vocabulary (the Notice channel).
/// Discipline: notifications only, never returning data back; implementations
/// must not block, and a failed terminal write counts as fatal.
/// The machine (agent) depends only on this trait, not on a concrete renderer;
/// the only in-process implementation is [`Renderer`].
pub trait Ui {
    /// A thinking fragment (arrives before the body text).
    fn reasoning_delta(&mut self, s: &str);
    /// A body-text fragment.
    fn content_delta(&mut self, s: &str);
    /// One streaming round is over; blocks are reset.
    fn finish_turn(&mut self);
    /// A tool is starting: echo the tool name and an argument summary.
    fn tool_start(&mut self, name: &str, args: &str);
    /// A tool result summary.
    fn tool_result(&mut self, result: &str);
    /// Token usage for a sub-request (also accumulates session-level cache stats).
    fn usage(&mut self, u: &Usage);
    /// The turn was cancelled by the user (Ctrl-C): close the streaming block and
    /// print an interruption notice.
    fn interrupted(&mut self);
    /// Approval gate question: echo the tool and an argument summary and prompt
    /// y/N (the answer is read through the Input channel, not through this trait —
    /// a Notice never returns data).
    fn approval_requested(&mut self, name: &str, args: &str);
}

/// Streaming renderer. Thinking and body text are two independent render blocks:
/// - thinking is gray (DIM), body text is the normal color
/// - blocks are separated by a newline; switching from thinking to body adds an
///   extra blank line
/// - with the NO_COLOR environment variable or when not a TTY no color codes are
///   emitted, but the block separation is kept
pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    mode: Mode,
    stats: CacheStats,
    /// Model id shown in the status bar's left segment (updated when a session is
    /// created or switched).
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

    /// Sync the status bar against the current terminal size: called when the REPL
    /// starts and before each input, which also handles window resizes (if the
    /// size changed, the bar is torn down and rebuilt).
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

    /// Redraw the status bar (without re-detecting the size).
    fn redraw_status_bar(&mut self) {
        let Some(bar) = self.bar else { return };
        let label = self.status_label();
        let visible = text::truncate(&label, bar.width());
        let painted = self.paint(DIM, visible);
        bar.render(self.out.as_mut(), visible, &painted);
    }

    /// Status bar text: `<model> · cache ...`; falls back to pure cache stats when
    /// the model is unknown.
    fn status_label(&self) -> String {
        let cache = self.stats.label();
        match &self.model {
            Some(m) if !m.is_empty() => format!("{m} · {cache}"),
            _ => cache,
        }
    }

    /// Set the model id shown in the status bar (called when a session is created
    /// or switched, since it follows the session meta).
    pub fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_owned());
        self.redraw_status_bar();
    }

    /// Cache statistics accumulated for the current session (the status bar's data
    /// source). Read by tests only.
    #[cfg(test)]
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// Clear the cache statistics when switching sessions.
    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
        self.redraw_status_bar();
    }

    /// Restore the terminal before exiting (reset the scroll region, clear the
    /// status bar line). Idempotent.
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

    /// End the current block: reset the color + newline; when blank=true add one
    /// more blank line (block spacing).
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

    /// Replay history messages (used when resuming a session), styled consistently
    /// with live rendering:
    /// user messages get a `›` prefix, assistant thinking is gray, body text is
    /// normal, tool calls are yellow, and
    /// tool messages show only a summary (same as live, without flushing the 10KB
    /// original text).
    /// The session file contains no system message (SYSTEM_PROMPT is a compile-time
    /// constant), so there is nothing to filter.
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
        self.raw(&self.paint(YELLOW, "⏹ interrupted (Ctrl-C)"));
        self.raw("\n");
    }

    fn approval_requested(&mut self, name: &str, args: &str) {
        let hint = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| {
                v.get("command")
                    .or_else(|| v.get("file_path"))
                    .and_then(|c| c.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| args.chars().take(80).collect());
        self.raw(&self.paint(YELLOW, &format!("▸ {name} {hint} — run it? [y/N] ")));
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
        r.reasoning_delta("b"); // still Reasoning: no repeated color code
        r.content_delta("x");
        r.content_delta("y"); // still Content: no new block
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
        assert!(s.contains("▸ Write not json at all"), "{s}"); // bad JSON falls back to raw text
    }

    #[test]
    fn replay_renders_history_compactly_with_colors() {
        use crate::types::{ToolCall, ToolCallFunction};
        let (mut r, buf) = Renderer::with_buffer(true);
        r.replay(&[
            Message::user("take a look"),
            Message {
                role: Role::Assistant,
                content: Some("running it".into()),
                reasoning_content: Some("let me think".into()),
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
            Message::system("must not appear"),
        ]);
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("\x1b[2m› \x1b[0mtake a look"), "{s}");
        assert!(
            s.contains("\x1b[2mlet me think\x1b[0m"),
            "thinking is gray: {s}"
        );
        assert!(s.contains("running it"), "{s}");
        assert!(s.contains("▸ Bash ls -la"), "tool calls are yellow: {s}");
        // tool messages only get a summary, never the full text
        assert!(s.contains("exit_code: 0 · 39 bytes"), "{s}");
        assert!(
            !s.contains("SECRET_BODY"),
            "tool output must not be replayed: {s}"
        );
        assert!(
            !s.contains("must not appear"),
            "system messages are not replayed: {s}"
        );
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
        r.reasoning_delta("thinking");
        r.interrupted();
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("interrupted"), "{s}");
        assert!(s.ends_with("\x1b[0m\n"), "color reset closes the line: {s}");
    }

    #[test]
    fn approval_requested_asks_without_newline() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.approval_requested("Bash", r#"{"command":"rm -rf /"}"#);
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("▸ Bash rm -rf /"), "{s}");
        assert!(
            s.ends_with("[y/N] "),
            "ends on the y/N prompt without a newline: {s:?}"
        );
    }

    #[test]
    fn tool_result_shows_exit_line_and_bytes() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.tool_result("exit_code: 3\n--- stdout ---\nhello");
        r.tool_result("");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("exit_code: 3 · 33 bytes"), "{s}");
        assert!(s.contains(" · 0 bytes"), "{s}"); // empty result
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
        r.info("session abc");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("tokens: in 10/15 (hit 6/miss 4) · out 5"), "{s}");
        assert!(s.contains("session abc"), "{s}");
    }

    #[test]
    fn color_variants_render_codes_and_plain() {
        // color=true: info/usage take paint's colored branch; error goes red
        let (mut r, buf) = Renderer::with_buffer(true);
        r.info("ok");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(s, "\x1b[2mok\x1b[0m\n");
        r.error("boom"); // eprintln, does not write to buf; only checks it does not
        // panic and that the red paint call is covered
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
        // GLM shape: only prompt_tokens_details.cached_tokens is given, so miss
        // has to be derived.
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
        assert_eq!(StatusBar::from_size(Some((2, 80))), None); // too few rows
        assert_eq!(StatusBar::from_size(Some((24, 1))), None); // too narrow
        assert_eq!(
            StatusBar::from_size(Some((24, 80))),
            Some(StatusBar { rows: 24, cols: 80 })
        );
    }

    #[test]
    fn status_bar_detect_rejects_non_tty() {
        assert_eq!(StatusBar::detect(false), None);
    }

    /// Regression: the bar used to measure its label in chars, so every wide
    /// character was undercharged by one column and the label spilled past the
    /// column the bar deliberately leaves free (which is what keeps the write
    /// from triggering autowrap on the last row).
    #[test]
    fn status_bar_render_charges_wide_chars_two_columns() {
        let bar = StatusBar { rows: 10, cols: 20 }; // width = 19
        let (mut r, buf) = Renderer::with_buffer(false);
        // two ideographs + space + rocket = 7 columns but only 4 chars
        let label = "\u{6df1}\u{5ea6} \u{1f680}";
        assert_eq!(label.chars().count(), 4);
        assert_eq!(text::width(label), 7);
        let visible = text::truncate(label, bar.width());
        bar.render(r.out.as_mut(), visible, visible);
        let s = buf_of(&buf);
        // 19 - 7 = 12 columns of padding. Counting chars would have written 15
        // and pushed the label 3 columns past the right edge.
        assert!(
            s.contains(&format!("\x1b[2K{}{label}\x1b8", " ".repeat(12))),
            "{s:?}"
        );
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
        let label = "cache 98.6% · hit 32384 · miss 461"; // 34 characters
        let painted = r.paint(DIM, label);
        bar.render(r.out.as_mut(), label, &painted);
        let s = buf_of(&buf);
        // 39 - 34 = 5 spaces of left padding, right aligned
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
        let label = CacheStats {
            hit: 32384,
            miss: 461,
        }
        .label();
        let visible = text::truncate(&label, bar.width());
        bar.render(r.out.as_mut(), visible, visible);
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

        // (None, None): no output
        let (mut r, buf) = Renderer::with_buffer(false);
        r.apply_status_bar(None);
        assert!(buf_of(&buf).is_empty());

        // (None, Some): create the bar + first draw
        r.apply_status_bar(Some(a));
        let s = buf_of(&buf);
        assert!(s.contains("\x1b[1;9r"), "{s:?}");
        assert!(s.contains("cache —"), "{s:?}");
        assert_eq!(r.bar, Some(a));

        // (Some, Some(same)): redraw only
        let before = buf_of(&buf).len();
        r.apply_status_bar(Some(a));
        assert!(buf_of(&buf)[before..].contains("\x1b[10;1H\x1b[2K"));

        // (Some, Some(different)): tear down the old one, build the new one
        let before = buf_of(&buf).len();
        r.apply_status_bar(Some(b));
        let tail = &buf_of(&buf)[before..];
        assert!(tail.contains("\x1b[r"), "{tail:?}");
        assert!(tail.contains("\x1b[1;11r"), "{tail:?}");
        assert_eq!(r.bar, Some(b));

        // (Some, None): tear the bar down
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
        // session accumulation: 18 hit / 12 miss = 60.0%
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

        // switching models: after the redraw only the new model is left
        r.set_model("deepseek-v4-pro");
        let tail = &buf_of(&buf)[s.len()..];
        assert!(tail.contains("deepseek-v4-pro · cache 60.0%"), "{tail:?}");
        assert!(!tail.contains("deepseek-v4-flash"), "{tail:?}");

        // reset_stats clears only the cache stats and keeps the model
        let before = buf_of(&buf).len();
        r.reset_stats();
        let tail = &buf_of(&buf)[before..];
        assert!(tail.contains("deepseek-v4-pro · cache —"), "{tail:?}");
    }

    #[test]
    fn refresh_status_bar_without_tty_is_noop_and_teardown_idempotent() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.refresh_status_bar(); // cargo test's stdout is a pipe → not enabled
        assert!(buf_of(&buf).is_empty());
        r.teardown(); // idempotent when not enabled
        assert!(buf_of(&buf).is_empty());
    }
}
