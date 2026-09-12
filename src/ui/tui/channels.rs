//! The channels the event loop reads and answers while it runs.
//!
//! Three things cross between the machine and the loop:
//!
//! - the machine's notifications, one kind per thing it has to say
//! - the approval gate's question, which the machine asks for and the
//!   input box answers
//! - the question tool's questions, which the loop puts up on a panel of
//!   their own
//!
//! Each pair is a channel the loop reads on one end and the machine holds
//! the other end of; the sender is what travels into the turn, so that a
//! turn running in this loop is one the machine can talk to without ever
//! touching the terminal.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::{mpsc, oneshot, watch};

use crate::tools::ask::{Answer, Question};
use crate::types::ToolCall;
use crate::ui::{Approve, Ask, Cancel, Verdict};

/// Cancellation as the event loop delivers it: Ctrl-C sets the watch and every
/// await point in the turn races against [`CtrlC::wait`].
///
/// This is why the cancel channel is a parameter of `Agent::turn`: in raw mode
/// there is no SIGINT to subscribe to, so a key event is the only source there is.
pub(super) struct CtrlC(pub(super) watch::Receiver<bool>);

impl Cancel for CtrlC {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        let rx = &mut self.0;
        Box::pin(async move {
            // `borrow_and_update` first: a cancel that arrived before this wait
            // was created must still be seen, which a bare `changed()` would miss.
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    // The front end is gone; never fire again.
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// The approval gate, answered from the input box.
///
/// The question goes to the event loop as a reply handle; the next line the user
/// submits is the answer. Denial is the default, including when the front end has
/// gone away mid-ask.
pub(super) struct Gate {
    pub(super) tx: mpsc::UnboundedSender<oneshot::Sender<Verdict>>,
}

impl Approve for Gate {
    fn approve(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = Verdict> + '_>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if self.tx.send(reply).is_err() {
                // The front end is gone: denying is the answer that runs nothing.
                return Verdict::Denied;
            }
            answer.await.unwrap_or(Verdict::Denied)
        })
    }
}

/// The question tool, answered from the event loop.
///
/// The questions go to the loop as a panel to put up, and the reply handle travels
/// with them: the answers are not a value this object ever sees, they are what the
/// panel sends when the last question has been answered. A front end that has gone
/// away -- the channel closed, or the panel dropped with the turn -- answers
/// nothing, which is the dismissal the interpreter writes down as a marker.
pub(super) struct Questions {
    pub(super) tx: mpsc::UnboundedSender<Asked>,
}

/// What the loop is asked to put on the screen: the questions, and where the
/// answers go.
pub(super) struct Asked {
    pub(super) questions: Vec<Question>,
    pub(super) reply: oneshot::Sender<Option<Vec<Answer>>>,
}

impl Ask for Questions {
    fn ask(
        &mut self,
        questions: &[Question],
    ) -> Pin<Box<dyn Future<Output = Option<Vec<Answer>>> + '_>> {
        let (reply, answers) = oneshot::channel();
        let asked = Asked {
            questions: questions.to_vec(),
            reply,
        };
        Box::pin(async move {
            if self.tx.send(asked).is_err() {
                return None;
            }
            // A dropped sender is the same answer as a closed channel: nobody
            // answered, and the model is told so rather than left waiting.
            answers.await.unwrap_or(None)
        })
    }
}

/// The channels the event loop reads and answers while it runs.
///
/// The machine's notifications arrive on one, the gate's questions on another and
/// the question tool's on a third; the sender of each of the last two is what the
/// machine holds while it waits.
pub(super) struct Channels {
    pub(super) notices: mpsc::UnboundedReceiver<crate::ui::tui::notice::Notice>,
    pub(super) gates: mpsc::UnboundedReceiver<oneshot::Sender<Verdict>>,
    pub(super) gate_tx: mpsc::UnboundedSender<oneshot::Sender<Verdict>>,
    pub(super) questions: mpsc::UnboundedReceiver<Asked>,
    pub(super) question_tx: mpsc::UnboundedSender<Asked>,
}
