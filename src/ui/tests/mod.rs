//! The tests for the plain front end, split by concern.
//!
//! The harness comes first: a stand-in terminal that answers what a test tells
//! it to, a writer that keeps what was written, and the two ways of building a
//! renderer over them. Every submodule reaches for those, and none of them is
//! about a terminal unless it says so. Submodules are listed in the order a
//! reader of the front end meets them: the banner a session opens with, the
//! stream it writes, and the bar under it.

use std::io::Write;

use crate::types::Usage;

use super::status::CacheStats;
use super::terminal::{Echo, Terminal};
use super::*;

pub(super) struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A terminal that answers what a test tells it to rather than what the
/// process's own does.
///
/// The facts are the terminal's; the decisions built on them -- is the bar
/// enabled, is the size usable, does the echo go off before the question is
/// asked -- are this front end's, and these are what let a unit test make them.
pub(super) struct StandIn {
    size: Option<(u16, u16)>,
    tty: bool,
    dumb: bool,
    color: bool,
    /// Whether the echo is off right now. The guard turns it back on.
    echo: std::rc::Rc<std::cell::Cell<bool>>,
    /// What `read_line` answers, one per call; empty is an end of input.
    lines: std::cell::RefCell<std::collections::VecDeque<String>>,
}

/// A read on whether the echo is off, held after the stand-in is moved away.
pub(super) struct EchoHandle(std::rc::Rc<std::cell::Cell<bool>>);

impl EchoHandle {
    pub(super) fn is_off(&self) -> bool {
        self.0.get()
    }
}

/// Put back on drop, the way the real terminal's echo is restored.
struct Silence(std::rc::Rc<std::cell::Cell<bool>>);

impl Echo for Silence {}

impl Drop for Silence {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl StandIn {
    /// A 24x80 terminal that can host the bar.
    pub(super) fn new() -> Self {
        Self {
            size: Some((24, 80)),
            tty: true,
            dumb: false,
            color: true,
            echo: std::rc::Rc::new(std::cell::Cell::new(false)),
            lines: std::cell::RefCell::new(std::collections::VecDeque::new()),
        }
    }

    /// The lines this terminal answers with, one per call.
    pub(super) fn answering(self, lines: &[&str]) -> Self {
        let queue = lines.iter().map(|l| (*l).to_owned()).collect();
        *self.lines.borrow_mut() = queue;
        self
    }

    /// The echo's current state, readable after the stand-in has been moved into
    /// a renderer: what a test about a secret has to look at while the answer is
    /// being read.
    pub(super) fn echo_handle(&self) -> EchoHandle {
        EchoHandle(self.echo.clone())
    }

    pub(super) fn tty(mut self, tty: bool) -> Self {
        self.tty = tty;
        self
    }

    pub(super) fn dumb(mut self, dumb: bool) -> Self {
        self.dumb = dumb;
        self
    }

    pub(super) fn sized(mut self, rows: u16, cols: u16) -> Self {
        self.size = Some((rows, cols));
        self
    }

    /// A terminal that cannot be measured at all.
    pub(super) fn unmeasurable(mut self) -> Self {
        self.size = None;
        self
    }
}

impl Terminal for StandIn {
    fn size(&self) -> Option<(u16, u16)> {
        self.size
    }

    fn is_tty(&self) -> bool {
        self.tty
    }

    fn is_dumb(&self) -> bool {
        self.dumb
    }

    fn echo_off(&self) -> Option<Box<dyn Echo>> {
        self.echo.set(true);
        Some(Box::new(Silence(self.echo.clone())))
    }

    fn read_line(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + '_>> {
        let line = self.lines.borrow_mut().pop_front();
        Box::pin(async move { line })
    }

    /// Answered from the stand-in rather than the environment, so a test says
    /// what the terminal wants instead of setting a variable the whole process
    /// shares.
    fn wants_color(&self) -> bool {
        self.color
    }
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

/// A renderer writing into a buffer, on a stand-in that is not a terminal: the
/// shape every test here starts from, since none of them is about a terminal
/// unless it says so.
fn with_buffer(color: bool) -> (Renderer, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    renderer_on(StandIn::new().tty(false), color)
}

/// A renderer writing into a buffer, on a terminal the test describes.
fn renderer_on(
    term: StandIn,
    color: bool,
) -> (Renderer, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        Renderer::on(Box::new(SharedBuf(buf.clone())), color, Box::new(term)),
        buf,
    )
}

mod banner;
mod renderer;
mod status_bar;
