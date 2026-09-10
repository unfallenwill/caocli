//! The notice: one thing the machine or the application tells the front end,
//! and the handle that turns `Ui` and `Front` calls into notices on a channel.
//!
//! This is the vocabulary of the conversation between the machine and the
//! screen. It carries values in one direction only -- the machine never sees
//! anything back -- so the object that draws can live on the other side of a
//! channel from the object that thinks.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::types::{Message, Usage};
use crate::ui::{Front, Ui};

/// One thing the machine or the application tells the front end.
///
/// Everything that the plain front end would draw on the spot becomes one of
/// these, so that the only code touching the terminal is the event loop.
///
/// It is not `Clone`: a notice carries text that is moved into the state.
#[derive(Debug)]
pub(super) enum Notice {
    Reasoning(String),
    Content(String),
    FinishTurn,
    ToolStart {
        name: String,
        args: String,
    },
    ToolResult(String),
    Usage(Usage, Duration),
    Interrupted,
    Approval {
        name: String,
        args: String,
    },
    /// A question whose answer must not be shown by the box, let alone kept:
    /// the reply handle travels with the prompt, because there is nothing to
    /// pair it with here -- the approval gate's answer arrives on a channel of
    /// its own only because the machine asks for it, and this is asked for by
    /// the line being handled.
    Secret {
        prompt: String,
        reply: oneshot::Sender<Option<String>>,
    },
    Replay(Vec<Message>),
    Info(String),
    Error(String),
    SetModel(String),
    ResetStats,
}

/// The handle both the machine and the application hold while a turn runs. It
/// implements [`Ui`] and [`Front`] by turning every call into a notice, because
/// the object that draws them owns the terminal on the other side of the channel.
pub(super) struct Notifier {
    pub(super) tx: mpsc::UnboundedSender<Notice>,
}

impl Notifier {
    fn send(&self, notice: Notice) {
        // A closed channel means the front end is gone; the turn still running
        // will be dropped with it.
        let _ = self.tx.send(notice);
    }
}

impl Ui for Notifier {
    fn reasoning_delta(&mut self, s: &str) {
        self.send(Notice::Reasoning(s.to_owned()));
    }
    fn content_delta(&mut self, s: &str) {
        self.send(Notice::Content(s.to_owned()));
    }
    fn finish_turn(&mut self) {
        self.send(Notice::FinishTurn);
    }
    fn tool_start(&mut self, name: &str, args: &str) {
        self.send(Notice::ToolStart {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }
    fn tool_result(&mut self, result: &str) {
        self.send(Notice::ToolResult(result.to_owned()));
    }
    fn usage(&mut self, u: &Usage, stream: Duration) {
        self.send(Notice::Usage(u.clone(), stream));
    }
    fn interrupted(&mut self) {
        self.send(Notice::Interrupted);
    }
    fn approval_requested(&mut self, name: &str, args: &str) {
        self.send(Notice::Approval {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }
}

impl Front for Notifier {
    fn replay(&mut self, messages: &[Message]) {
        self.send(Notice::Replay(messages.to_vec()));
    }
    fn info(&mut self, s: &str) {
        self.send(Notice::Info(s.to_owned()));
    }
    fn error(&mut self, s: &str) {
        self.send(Notice::Error(s.to_owned()));
    }
    fn set_model(&mut self, model: &str) {
        self.send(Notice::SetModel(model.to_owned()));
    }
    fn reset_stats(&mut self) {
        self.send(Notice::ResetStats);
    }
    fn ask_secret(&mut self, prompt: &str) -> Pin<Box<dyn Future<Output = Option<String>> + '_>> {
        let (reply, answer) = oneshot::channel();
        self.send(Notice::Secret {
            prompt: prompt.to_owned(),
            reply,
        });
        Box::pin(async move {
            // A front end that has gone away takes the question with it: an
            // answer that will never come is a cancellation.
            answer.await.unwrap_or(None)
        })
    }
}

/// Everything that has arrived on a channel, without waiting for more.
pub(super) fn drain<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> Vec<T> {
    let mut drained = Vec::new();
    while let Ok(item) = rx.try_recv() {
        drained.push(item);
    }
    drained
}
