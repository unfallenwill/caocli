//! The agent: one session, one provider, and the policies a turn runs under.
//!
//! It owns the state the interpreter works on — the session (the log *is* the
//! state), the client bound to a provider, whether the approval gate is on and
//! how many tool steps one turn may spend — and it offers the control plane the
//! shell drives: bind a provider, adopt another session, name the model. What it
//! does with that state is `turn`, what it sends is `request`, and what comes
//! back is `stream`.

use std::sync::Arc;

use crate::api::Client;
use crate::machine;
use crate::mcp::Hub;
use crate::provider;
use crate::session::Session;
use crate::types::WireRequest;

/// `pub(crate)` for the wire-mismatch test in `api`, which builds a request
/// from a preset to check the client takes it.
pub(crate) mod request;
mod stream;
mod turn;
pub struct Agent {
    api: Client,
    pub session: Session,
    /// The provider the client is bound to. The endpoint, the key and the
    /// answer ceiling all come from it, and the model is named by it: the
    /// status line reads `<provider>/<modelid>` from here.
    provider: provider::Provider,
    /// Whether the approval gate is on: with it, the calls that change something
    /// on disk ask the user before running, and the ones that do not never do.
    pub approval: Approval,
    /// Per-turn tool step cap (product-level termination guarantee). The turn in
    /// flight counts against it (`turn::Turn`), and the next turn gets it whole.
    pub max_tool_steps: usize,
    /// The MCP servers this session connected to, and the tools they offer.
    ///
    /// One hub for the session rather than one per request: what it holds is a
    /// process on the other end of a pipe, and a connection per request would be
    /// a server started and killed for every turn. It is also why the tool list
    /// is in the request prefix rather than in the session — the session is the
    /// log, and the log does not start programs.
    pub mcp: Arc<Hub>,
}

/// Whether a tool call that changes something runs or is asked about first.
///
/// An enum rather than the command-line flag itself: the loop reads it as a
/// policy, and a reader should not have to remember which way a `bool` pointed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Execution is trusted; every call runs.
    Trusted,
    /// Bash/Edit/Write ask the user first (Read and Glob never do).
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
            // Nobody to talk to until the shell says otherwise: a session with
            // no servers configured is a session with no MCP tools, and the
            // same machine either way.
            mcp: Arc::new(Hub::empty()),
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

    fn build_request(&self) -> WireRequest {
        request::build_request(
            &self.provider,
            &self.session.meta,
            &self.session.messages,
            self.mcp.definitions(),
        )
    }
}

#[cfg(test)]
mod tests;
