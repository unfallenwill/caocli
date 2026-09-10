//! One turn: the interpreter loop and the beats it runs.
//!
//! The machine decides and this executes: `machine::next_action` folds the log
//! into the next beat, the loop runs it and writes the result back into the log —
//! which is the only history there is. The loop therefore carries no memory
//! between beats beyond [`Turn`], and `Turn` lives no longer than the turn.
//!
//! Every await point a turn has goes through [`race`]: a cancel is decided by
//! which arm won, never by comparing the text an effect returned.

use std::future::Future;

use anyhow::Result;

use crate::machine::{self, Action, Marker};
use crate::session::Session;
use crate::tools;
use crate::types::{Message, ToolCall};
use crate::ui::{Approve, Cancel, Ui, Verdict};

use super::{Agent, Approval};

/// What one beat did to the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The log moved on; ask the machine for the next beat.
    Again,
    /// The turn is over: the model answered, the user cancelled, or the step
    /// budget ran out.
    Stop,
}

/// The outcome of racing one effect against the user's cancel.
enum Ran<T> {
    /// The effect finished. `biased` polls it first, so a cancel arriving at the
    /// same moment as an answer does not throw the answer away.
    Finished(T),
    /// The cancel won: the effect was dropped (the stream disconnected, the child
    /// process was killed) and nothing was written for it.
    Cancelled,
}

/// Race one effect against the cancel source: the single place a turn decides
/// that it was cancelled.
async fn race<T>(cancel: &mut dyn Cancel, effect: impl Future<Output = T>) -> Ran<T> {
    tokio::select! {
        biased;
        finished = effect => Ran::Finished(finished),
        _ = cancel.wait() => Ran::Cancelled,
    }
}

/// What the approval gate decided about one call.
enum Gate {
    /// Run it.
    Cleared,
    /// The user declined: the call is answered with the denial marker, and the
    /// turn goes on to the next one.
    Denied,
    /// The user cancelled while the question was open. The call was never
    /// approved and has no result of its own; the cleanup closes it.
    Cancelled,
}

/// Persist `marker` for every call that was declared and never answered.
///
/// What the backend checks is the window, not the call: a log that leaves one
/// result missing is answered with a 400 (the same reason `machine::heal` exists
/// for crashes). Every call still open gets a result, so the history stays a
/// valid request prefix.
fn close_open_calls(session: &mut Session, ui: &mut dyn Ui, marker: Marker) -> Result<()> {
    for id in machine::open_call_ids(&session.messages) {
        session.append_message(&Message::tool(&id, marker.text()))?;
        ui.tool_result(marker.text());
    }
    Ok(())
}

/// The turn in flight: what a beat needs to run, and what it may spend.
///
/// It holds the borrows the caller handed in — the agent, the watcher and the two
/// answers — so a beat is `turn.beat(action)` rather than a call with six
/// arguments, and so the step budget that belongs to the turn cannot be mistaken
/// for state of the agent's own. Nothing here outlives the turn.
struct Turn<'a> {
    agent: &'a mut Agent,
    ui: &'a mut dyn Ui,
    cancel: &'a mut dyn Cancel,
    approve: &'a mut dyn Approve,
    /// Tool steps left. Every `ExecTool` action costs one, including a call the
    /// gate denied: a decision the user had to make is a step spent.
    steps_left: usize,
    /// Set by a cancel arm, read once when the turn ends.
    cancelled: bool,
}

impl Agent {
    /// One conversational turn: may contain several sub-requests (the model keeps
    /// going after calling tools, until finish_reason=stop).
    /// The renderer is held by the caller and passed in: the status bar and the
    /// streaming output must go through the same `Ui` implementation, otherwise
    /// the cache stats recorded by `usage()` never reach the instance that
    /// already drew the bar.
    ///
    /// Exactly one cancel source is subscribed per turn, and the caller
    /// subscribes it before calling this: tokio's signal notifications ride on a
    /// watch, so if a listener were created inside each `select`, a SIGINT
    /// arriving in the gap between two `select`s would be broadcast away before
    /// the new listener subscribed and would be lost forever (measured in the
    /// pty smoke test, a millisecond-scale window). The caller owning the
    /// subscription also keeps the interpreter independent of how a cancel
    /// arrives: `Sigint` is only one possible source.
    ///
    /// `cancel` is the source of an out-of-band Command (Ctrl-C): every await
    /// point races against it. When it fires, the current effect is dropped, the
    /// calls that were declared but not answered are persisted with a
    /// cancellation marker to close the window, and the history stays
    /// `is_request_valid` so the next turn continues from a valid prefix.
    /// Cancellation is handled here and never enters `next_action`: it is
    /// out-of-band, and every state is reachable without it.
    ///
    /// `approve` answers the approval gate. Both are supplied by the caller
    /// rather than built here because both depend on which front end is running:
    /// a front end that owns the terminal in raw mode leaves no SIGINT to listen
    /// for, so it answers both channels from its own event loop.
    pub async fn turn(
        &mut self,
        input: &str,
        ui: &mut dyn Ui,
        cancel: &mut dyn Cancel,
        approve: &mut dyn Approve,
    ) -> Result<()> {
        self.session.append_message(&Message::user(input))?;
        let steps_left = self.max_tool_steps;
        let mut turn = Turn {
            agent: self,
            ui,
            cancel,
            approve,
            steps_left,
            cancelled: false,
        };
        while let Some(action) = machine::next_action(&turn.agent.session.messages) {
            if turn.beat(action).await? == Step::Stop {
                break;
            }
        }
        turn.finish()
    }
}

impl Turn<'_> {
    /// Run one beat of the turn.
    async fn beat(&mut self, action: Action) -> Result<Step> {
        match action {
            Action::CallModel => self.call_model().await,
            Action::ExecTool(call) => self.exec_tool(&call).await,
            Action::Done => Ok(Step::Stop),
        }
    }

    /// One sub-request. A cancel drops the stream and writes nothing: the log
    /// stays at a valid prefix, and the cleanup closes what the turn left open.
    async fn call_model(&mut self) -> Result<Step> {
        let request = self.agent.build_request();
        let reply = match race(self.cancel, self.agent.stream_reply(&request, self.ui)).await {
            Ran::Finished(reply) => reply?,
            Ran::Cancelled => return Ok(self.stop()),
        };
        self.agent.session.append_message(&reply.message)?;
        if let Some(usage) = &reply.usage {
            self.ui.usage(usage, reply.stream_time);
        }
        Ok(Step::Again)
    }

    /// One declared call: the budget, the gate, then the tool itself — and each
    /// of the three is where a cancel can land.
    async fn exec_tool(&mut self, call: &ToolCall) -> Result<Step> {
        self.ui
            .tool_start(&call.function.name, &call.function.arguments);
        if self.steps_left == 0 {
            return self.spend_nothing(call);
        }
        self.steps_left -= 1;
        match self.gate(call).await? {
            Gate::Cleared => {}
            Gate::Denied => return Ok(Step::Again),
            Gate::Cancelled => return Ok(self.stop()),
        }
        self.run(call).await
    }

    /// The budget is gone: the call in hand is answered with the marker instead
    /// of running, and so is every call the turn left open. The turn ends here.
    fn spend_nothing(&mut self, call: &ToolCall) -> Result<Step> {
        self.answer_with(call, Marker::StepLimit)?;
        close_open_calls(&mut self.agent.session, self.ui, Marker::StepLimit)?;
        Ok(Step::Stop)
    }

    /// Ask before running, when the policy says to ask and the call changes
    /// something on disk. Reading never asks.
    async fn gate(&mut self, call: &ToolCall) -> Result<Gate> {
        if self.agent.approval != Approval::Ask || call.function.name == tools::READ_NAME {
            return Ok(Gate::Cleared);
        }
        // The question is a notification like any other; the answer comes back
        // through the channel the front end took it from.
        self.ui
            .approval_requested(&call.function.name, &call.function.arguments);
        Ok(match race(self.cancel, self.approve.approve(call)).await {
            Ran::Finished(Verdict::Allowed) => Gate::Cleared,
            Ran::Finished(Verdict::Denied) => {
                self.answer_with(call, Marker::Denied)?;
                Gate::Denied
            }
            Ran::Cancelled => Gate::Cancelled,
        })
    }

    /// Run one tool call, raced against the cancel. The dispatch is one string in
    /// and one string out; a tool that fails says so in its own result text, which
    /// the model reads like any other result.
    async fn run(&mut self, call: &ToolCall) -> Result<Step> {
        let invoked = tools::execute(&call.function.name, &call.function.arguments);
        match race(self.cancel, invoked).await {
            Ran::Finished(tool_output) => {
                self.ui.tool_result(&tool_output);
                self.agent
                    .session
                    .append_message(&Message::tool(&call.id, tool_output))?;
                Ok(Step::Again)
            }
            // Dropped mid-flight, so the call has a result after all: the marker
            // says the user stopped it. The turn ends here.
            Ran::Cancelled => {
                self.answer_with(call, Marker::Cancelled)?;
                Ok(self.stop())
            }
        }
    }

    /// Give one call the result it is owed and keep the turn going.
    fn answer_with(&mut self, call: &ToolCall, marker: Marker) -> Result<()> {
        self.ui.tool_result(marker.text());
        self.agent
            .session
            .append_message(&Message::tool(&call.id, marker.text()))?;
        Ok(())
    }

    /// End the turn on a cancel.
    fn stop(&mut self) -> Step {
        self.cancelled = true;
        Step::Stop
    }

    /// What the turn still owes when it is over: a cancelled turn closes the calls
    /// it left unanswered and says so. A turn that ended by answering, or by
    /// spending its budget, has nothing left to write.
    fn finish(&mut self) -> Result<()> {
        if !self.cancelled {
            return Ok(());
        }
        close_open_calls(&mut self.agent.session, self.ui, Marker::Cancelled)?;
        self.ui.interrupted();
        Ok(())
    }
}
