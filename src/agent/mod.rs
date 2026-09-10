//! The interpreter: it asks the machine what the next beat is, executes it, and
//! writes the result back into the log.
//!
//! The machine decides and never executes (`machine::next_action`), so this is
//! the only place in the crate that turns a decision into IO. It owns the
//! session — the log is the state — and the two things a turn needs from
//! whoever is watching: an answer for the approval gate and a signal to stop.

use crate::api::Client;
use crate::machine;
use crate::provider;
use crate::session::Session;
use crate::types::ChatRequest;

mod request;
mod stream;
mod turn;

pub use request::build_request;

pub struct Agent {
    api: Client,
    pub session: Session,
    /// The provider the client is bound to. The endpoint, the key and the
    /// answer ceiling all come from it, and the model is named by it: the
    /// status line reads `<provider>/<modelid>` from here.
    provider: provider::Provider,
    /// Whether the approval gate is on: with it, Bash/Edit/Write ask the user
    /// before running (Read is always allowed).
    pub approval: Approval,
    /// Per-turn tool step cap (product-level termination guarantee). The turn in
    /// flight counts against it (`turn::Turn`), and the next turn gets it whole.
    pub max_tool_steps: usize,
}

/// Whether a tool call that changes something runs or is asked about first.
///
/// An enum rather than the command-line flag itself: the loop reads it as a
/// policy, and a reader should not have to remember which way a `bool` pointed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Execution is trusted; every call runs.
    Trusted,
    /// Bash/Edit/Write ask the user first (Read never does).
    Ask,
}

impl Approval {
    /// The one place the command-line flag becomes a policy.
    pub fn from_flag(ask: bool) -> Self {
        if ask {
            Approval::Ask
        } else {
            Approval::Trusted
        }
    }
}

impl Agent {
    pub fn new(api: Client, session: Session, provider: provider::Provider) -> Self {
        Self {
            api,
            session,
            provider,
            approval: Approval::Trusted,
            max_tool_steps: machine::MAX_TOOL_STEPS,
        }
    }

    /// The provider the machine is talking to.
    pub fn provider(&self) -> provider::Provider {
        self.provider
    }

    /// The model as the front ends name it: `<provider>/<modelid>`, which is the
    /// form `/model` takes back and the only form that says where it is served.
    pub fn model_label(&self) -> String {
        format!("{}/{}", self.provider.id, self.session.meta.model)
    }

    /// The reasoning effort tier in effect: the one the session stored, or the
    /// provider's default when it stored none — the same fallback the request is
    /// built with. What `/effort` switches and the status line reports.
    pub fn effort_label(&self) -> &str {
        self.session
            .meta
            .reasoning_effort
            .as_deref()
            .unwrap_or(self.provider.default_effort)
    }

    /// Point the machine at a provider: a client for its endpoint and key. The
    /// ceiling the request is sent with comes from the preset itself (see
    /// `request::build_request`), so there is nothing to carry over here. The
    /// session's meta is the caller's to write -- it is a change to the log, and
    /// the interpreter writes the log.
    pub fn bind(&mut self, provider: provider::Provider, api: Client) {
        self.provider = provider;
        self.api = api;
    }

    /// The only control-plane entrance: shell commands such as `/new` and
    /// `/resume` replace the machine's persistent state through here (the machine
    /// = the log, so switching sessions replaces it wholesale). Session-level
    /// rendering state (clearing stats, the status bar's model) is refreshed by
    /// the caller (the shell).
    pub fn adopt(&mut self, session: Session) {
        self.session = session;
    }

    fn build_request(&self) -> ChatRequest {
        build_request(&self.provider, &self.session.meta, &self.session.messages)
    }
}

#[cfg(test)]
mod tests;
