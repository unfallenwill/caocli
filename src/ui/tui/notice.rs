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
    /// A chunk of a running command's own output, as it arrived.
    ToolOutput(String),
    /// A directory's instructions were picked up mid-session; the string is
    /// the directory, and the cell it becomes is the same one the replay folds.
    Instructions(String),
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
    SetEffort(String),
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
    fn reasoning_delta(&mut self, text: &str) {
        self.send(Notice::Reasoning(text.to_owned()));
    }
    fn content_delta(&mut self, text: &str) {
        self.send(Notice::Content(text.to_owned()));
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
    fn tool_output(&mut self, chunk: &str) {
        self.send(Notice::ToolOutput(chunk.to_owned()));
    }
    fn tool_result(&mut self, result: &str) {
        self.send(Notice::ToolResult(result.to_owned()));
    }
    fn instructions(&mut self, dir: &str) {
        self.send(Notice::Instructions(dir.to_owned()));
    }
    fn usage(&mut self, usage: &Usage, stream: Duration) {
        self.send(Notice::Usage(usage.clone(), stream));
    }
    fn interrupted(&mut self) {
        self.send(Notice::Interrupted);
    }
    fn truncated(&mut self, notice: &str) {
        self.send(Notice::Error(notice.to_owned()));
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
    fn info(&mut self, text: &str) {
        self.send(Notice::Info(text.to_owned()));
    }
    fn error(&mut self, text: &str) {
        self.send(Notice::Error(text.to_owned()));
    }
    fn set_model(&mut self, model: &str) {
        self.send(Notice::SetModel(model.to_owned()));
    }
    fn set_effort(&mut self, effort: &str) {
        self.send(Notice::SetEffort(effort.to_owned()));
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::types::Usage;

    #[test]
    fn every_ui_call_becomes_the_notice_it_names() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier { tx };
        notifier.reasoning_delta("think");
        notifier.content_delta("say");
        notifier.finish_turn();
        notifier.tool_start("Bash", "{}");
        notifier.tool_output("out");
        notifier.tool_result("done");
        notifier.usage(&Usage::default(), Duration::from_secs(1));
        notifier.interrupted();
        notifier.truncated("cut off");
        notifier.info("note");
        notifier.error("bad");
        notifier.set_model("glm-4.6");
        notifier.set_effort("high");
        notifier.reset_stats();
        let expected = [
            Notice::Reasoning("think".into()),
            Notice::Content("say".into()),
            Notice::FinishTurn,
            Notice::ToolStart {
                name: "Bash".into(),
                args: "{}".into(),
            },
            Notice::ToolOutput("out".into()),
            Notice::ToolResult("done".into()),
            Notice::Usage(Usage::default(), Duration::from_secs(1)),
            Notice::Interrupted,
            Notice::Error("cut off".into()),
            Notice::Info("note".into()),
            Notice::Error("bad".into()),
            Notice::SetModel("glm-4.6".into()),
            Notice::SetEffort("high".into()),
            Notice::ResetStats,
        ];
        for notice in expected {
            let got = rx.try_recv().expect("every call sends its notice");
            assert_eq!(format!("{got:?}"), format!("{notice:?}"), "in order");
        }
        assert!(rx.try_recv().is_err(), "and nothing more");
    }

    #[test]
    fn a_replayed_message_reaches_the_screen_through_the_loop() {
        // The front end that owns the screen does not paint: it hands the
        // messages to the loop, which folds them into the transcript the way a
        // resumed session's are folded. A message that became no notice is a
        // message a command such as `/image` built and nobody ever saw.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier { tx };
        notifier.replay(&[Message::user("hi")]);
        match rx.try_recv().expect("the message arrives") {
            Notice::Replay(messages) => assert_eq!(messages, vec![Message::user("hi")]),
            other => panic!("a replay, not {other:?}"),
        }
    }

    #[test]
    fn approval_requested_sends_the_call_for_the_gate_to_show() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier { tx };
        notifier.approval_requested("Bash", r#"{"command":"ls"}"#);
        match rx.try_recv().expect("the gate's question arrives") {
            Notice::Approval { name, args } => {
                assert_eq!(name, "Bash");
                assert_eq!(args, r#"{"command":"ls"}"#);
            }
            other => panic!("an approval, not {other:?}"),
        }
    }

    #[test]
    fn a_secret_is_asked_by_notice_and_answered_by_its_own_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier { tx };
        let ask = notifier.ask_secret("the API key");
        match rx.try_recv().expect("the question arrives as a notice") {
            Notice::Secret { prompt, reply } => {
                assert_eq!(prompt, "the API key");
                reply.send(Some("sk-test".into())).expect("an answer");
            }
            other => panic!("a secret, not {other:?}"),
        }
        assert_eq!(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(ask)
                .as_deref(),
            Some("sk-test")
        );
    }

    #[test]
    fn a_secret_asked_of_a_gone_front_end_is_a_cancellation() {
        // The receiver is dropped unread, so the notice dies with its reply
        // handle: the answer can never come.
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let mut notifier = Notifier { tx };
        let ask = notifier.ask_secret("the API key");
        assert_eq!(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(ask),
            None
        );
    }

    #[test]
    fn sending_to_a_gone_front_end_is_not_an_error() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let mut notifier = Notifier { tx };
        notifier.info("anything");
        notifier.finish_turn();
    }

    #[test]
    fn a_drained_channel_leaves_nothing_behind() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Notice::FinishTurn).expect("send");
        tx.send(Notice::Interrupted).expect("send");
        let drained = drain(&mut rx);
        assert_eq!(drained.len(), 2);
        assert!(drain(&mut rx).is_empty(), "and the channel is empty");
    }
}
