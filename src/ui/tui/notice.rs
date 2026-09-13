//! The notices: the machine's vocabulary, the application's vocabulary, and the
//! secret prompt's question. Three senders on purpose, so the channel a
//! receiver reads is the only kind of thing it can read.
//!
//! Until now this module held one [`Notice`] enum of seventeen variants that
//! every channel carried. The machine's notifications and the application's
//! calls (and the secret prompt's question, which carries a `oneshot`) all
//! went through one sender and were sorted back out at the loop. The sort
//! was a one-line match that worked because it was right every time; the
//! trouble was that a type mistake in the sort would not have been caught:
//! the receiver would still have been a `Vec<Notice>`, and a wrong notice
//! would have travelled the same path as a right one.
//!
//! Three senders mean three typed receivers. The machine cannot accidentally
//! be told about an application's `Info` line, and the application cannot
//! accidentally be told about the model's reasoning stream. The split costs
//! three channels where there was one, which is the same cost as three
//! separate locks.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::types::{Message, Usage};
use crate::ui::{Front, Ui};

/// One thing the machine tells the front end.
///
/// Everything that the plain front end would draw on the spot becomes one of
/// these, so that the only code touching the terminal is the event loop.
///
/// It is not `Clone`: a notice carries text that is moved into the state.
#[derive(Debug)]
pub(super) enum MachineNotice {
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
    /// An answer stopped before it was finished -- the model reached
    /// `max_tokens`, or the conversation outgrew the context window. The
    /// string is a sentence the wire writes for itself, so every front end
    /// shows the same one. Carried as its own variant rather than funnelled
    /// into [`MachineNotice::Truncated`]'s sibling because the cause is a
    /// normal event, not a failure, and the transcript's error styling is
    /// the wrong place for it.
    Truncated(String),
    Approval {
        name: String,
        args: String,
    },
}

/// One thing the application tells the front end.
///
/// The application's vocabulary is smaller than the machine's -- it has no
/// streaming blocks to manage, and no tool calls to echo -- and lives on its
/// own channel so a session that asks the machine to render a tool call
/// cannot accidentally deliver an `Info` line at the same address.
#[derive(Debug)]
pub(super) enum AppNotice {
    /// Replay the messages on the screen the same way a live turn would have
    /// rendered them -- the same cells, the same painter, the same gap rule.
    /// Carried as a notice rather than as a call into the screen because the
    /// front end that owns the screen has folded its render path through the
    /// loop's channel, not through direct calls.
    Replay(Vec<Message>),
    /// A dim informational line.
    Info(String),
    /// A failure. The plain front end writes these to the error stream; a
    /// front end owning the screen has to place them in its own output
    /// instead.
    Error(String),
    /// Adopt the model id the status line reports.
    SetModel(String),
    /// Adopt the reasoning effort tier the status line reports.
    SetEffort(String),
    /// Clear the session's cache statistics.
    ResetStats,
}

/// A question the secret prompt asked: the prompt to show, and the channel the
/// answer comes back on.
///
/// Has to carry a oneshot because nothing else pairs the prompt with its
/// answer -- the approval gate's answer arrives on a channel of its own
/// because the machine asks for it, and the question tool's questions travel
/// as a value through the loop; this one is asked for by the line being
/// handled, and only the line that asked it knows what to do with the
/// answer.
#[derive(Debug)]
pub(super) struct SecretAsk {
    pub(super) prompt: String,
    pub(super) reply: oneshot::Sender<Option<String>>,
}

/// The handle both the machine and the application hold while a turn runs.
///
/// Holds three senders: one for the machine's notifications, one for the
/// application's calls, one for the secret prompt's question. The senders
/// travel together so that `&mut dyn Front` (which both `Ui` and `Front`
/// reduce to here) is one object the loop can hand to the machine, but the
/// channels they write to are independent: a notification sent on the
/// machine's channel cannot be received by the application's loop.
pub(super) struct Notifier {
    pub(super) machine: mpsc::UnboundedSender<MachineNotice>,
    pub(super) app: mpsc::UnboundedSender<AppNotice>,
    pub(super) secret: mpsc::UnboundedSender<SecretAsk>,
}

impl Notifier {
    /// Send a machine notice that wraps a single `&str`. The eight `&str`
    /// -> `String` wrappers below all look like this; the helper is what lets
    /// their bodies stay one line.
    fn send_str(&self, ctor: fn(String) -> MachineNotice, text: &str) {
        // A closed channel means the front end is gone; the turn still running
        // will be dropped with it.
        let _ = self.machine.send(ctor(text.to_owned()));
    }
}

impl Ui for Notifier {
    fn reasoning_delta(&mut self, text: &str) {
        self.send_str(MachineNotice::Reasoning, text);
    }
    fn content_delta(&mut self, text: &str) {
        self.send_str(MachineNotice::Content, text);
    }
    fn finish_turn(&mut self) {
        let _ = self.machine.send(MachineNotice::FinishTurn);
    }
    fn tool_start(&mut self, name: &str, args: &str) {
        let _ = self.machine.send(MachineNotice::ToolStart {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }
    fn tool_output(&mut self, chunk: &str) {
        self.send_str(MachineNotice::ToolOutput, chunk);
    }
    fn tool_result(&mut self, result: &str) {
        self.send_str(MachineNotice::ToolResult, result);
    }
    fn instructions(&mut self, dir: &str) {
        self.send_str(MachineNotice::Instructions, dir);
    }
    fn usage(&mut self, usage: &Usage, stream: Duration) {
        let _ = self
            .machine
            .send(MachineNotice::Usage(usage.clone(), stream));
    }
    fn interrupted(&mut self) {
        let _ = self.machine.send(MachineNotice::Interrupted);
    }
    fn truncated(&mut self, notice: &str) {
        self.send_str(MachineNotice::Truncated, notice);
    }
    fn approval_requested(&mut self, name: &str, args: &str) {
        let _ = self.machine.send(MachineNotice::Approval {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }
}

impl Front for Notifier {
    fn replay(&mut self, messages: &[Message]) {
        let _ = self.app.send(AppNotice::Replay(messages.to_vec()));
    }
    fn info(&mut self, text: &str) {
        let _ = self.app.send(AppNotice::Info(text.to_owned()));
    }
    fn error(&mut self, text: &str) {
        let _ = self.app.send(AppNotice::Error(text.to_owned()));
    }
    fn set_model(&mut self, model: &str) {
        let _ = self.app.send(AppNotice::SetModel(model.to_owned()));
    }
    fn set_effort(&mut self, effort: &str) {
        let _ = self.app.send(AppNotice::SetEffort(effort.to_owned()));
    }
    fn reset_stats(&mut self) {
        let _ = self.app.send(AppNotice::ResetStats);
    }
    fn ask_secret(&mut self, prompt: &str) -> Pin<Box<dyn Future<Output = Option<String>> + '_>> {
        let (reply, answer) = oneshot::channel();
        let _ = self.secret.send(SecretAsk {
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
    fn every_ui_call_becomes_the_machine_notice_it_names() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier {
            machine: tx,
            app: mpsc::unbounded_channel().0,
            secret: mpsc::unbounded_channel().0,
        };
        notifier.reasoning_delta("think");
        notifier.content_delta("say");
        notifier.finish_turn();
        notifier.tool_start("Bash", "{}");
        notifier.tool_output("out");
        notifier.tool_result("done");
        notifier.usage(&Usage::default(), Duration::from_secs(1));
        notifier.interrupted();
        notifier.truncated("cut off");
        let expected = [
            MachineNotice::Reasoning("think".into()),
            MachineNotice::Content("say".into()),
            MachineNotice::FinishTurn,
            MachineNotice::ToolStart {
                name: "Bash".into(),
                args: "{}".into(),
            },
            MachineNotice::ToolOutput("out".into()),
            MachineNotice::ToolResult("done".into()),
            MachineNotice::Usage(Usage::default(), Duration::from_secs(1)),
            MachineNotice::Interrupted,
            MachineNotice::Truncated("cut off".into()),
        ];
        for notice in expected {
            let got = rx.try_recv().expect("every call sends its notice");
            assert_eq!(format!("{got:?}"), format!("{notice:?}"), "in order");
        }
        assert!(rx.try_recv().is_err(), "and nothing more");
    }

    #[test]
    fn every_front_call_becomes_the_app_notice_it_names() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier {
            machine: mpsc::unbounded_channel().0,
            app: tx,
            secret: mpsc::unbounded_channel().0,
        };
        notifier.info("note");
        notifier.error("bad");
        notifier.set_model("glm-4.6");
        notifier.set_effort("high");
        notifier.reset_stats();
        let expected = [
            AppNotice::Info("note".into()),
            AppNotice::Error("bad".into()),
            AppNotice::SetModel("glm-4.6".into()),
            AppNotice::SetEffort("high".into()),
            AppNotice::ResetStats,
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
        let mut notifier = Notifier {
            machine: mpsc::unbounded_channel().0,
            app: tx,
            secret: mpsc::unbounded_channel().0,
        };
        notifier.replay(&[Message::user("hi")]);
        match rx.try_recv().expect("the message arrives") {
            AppNotice::Replay(messages) => assert_eq!(messages, vec![Message::user("hi")]),
            other => panic!("a replay, not {other:?}"),
        }
    }

    #[test]
    fn approval_requested_sends_the_call_for_the_gate_to_show() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier {
            machine: tx,
            app: mpsc::unbounded_channel().0,
            secret: mpsc::unbounded_channel().0,
        };
        notifier.approval_requested("Bash", r#"{"command":"ls"}"#);
        match rx.try_recv().expect("the gate's question arrives") {
            MachineNotice::Approval { name, args } => {
                assert_eq!(name, "Bash");
                assert_eq!(args, r#"{"command":"ls"}"#);
            }
            other => panic!("an approval, not {other:?}"),
        }
    }

    #[test]
    fn a_secret_is_asked_by_notice_and_answered_by_its_own_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut notifier = Notifier {
            machine: mpsc::unbounded_channel().0,
            app: mpsc::unbounded_channel().0,
            secret: tx,
        };
        let ask = notifier.ask_secret("the API key");
        let SecretAsk { prompt, reply } = rx.try_recv().expect("the question arrives as a notice");
        assert_eq!(prompt, "the API key");
        reply.send(Some("sk-test".into())).expect("an answer");
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
        let mut notifier = Notifier {
            machine: mpsc::unbounded_channel().0,
            app: mpsc::unbounded_channel().0,
            secret: tx,
        };
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
        let mut notifier = Notifier {
            machine: tx,
            app: mpsc::unbounded_channel().0,
            secret: mpsc::unbounded_channel().0,
        };
        notifier.reasoning_delta("anything");
        notifier.finish_turn();
        notifier.info("app-anything");
    }

    #[test]
    fn a_drained_channel_leaves_nothing_behind() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(MachineNotice::FinishTurn).expect("send");
        tx.send(MachineNotice::Interrupted).expect("send");
        let drained = drain(&mut rx);
        assert_eq!(drained.len(), 2);
        assert!(drain(&mut rx).is_empty(), "and the channel is empty");
    }
}
