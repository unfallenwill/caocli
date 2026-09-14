//! MCP: other programs' tools, offered to the model as though they were this
//! agent's own.
//!
//! The protocol is spoken in the modules below this one — the transports
//! (`stdio`, `http`), the JSON-RPC vocabulary (`wire`), one connection
//! (`client`) — and what is here is the part the rest of the agent sees: which
//! servers a session has, what their tools are called, how a call finds the
//! server that offers it, and what is said when none of them comes up.
//!
//! Three decisions are made here rather than in any of them:
//!
//! - **A tool keeps its own name, inside a name that says whose it is.** Two
//!   servers may offer a tool called `search`, and the model has to be able
//!   to say which one it means: [`tool_name`] is `mcp__<server>__<tool>`, the
//!   convention every other client of this protocol uses, so a tool the model
//!   learned from one is the same name in another.
//! - **A server that does not come up costs nothing else.** The model is
//!   offered the tools of the servers that did, the session runs, and `/mcp`
//!   says which server is missing and why — a workspace whose `.mcp.json`
//!   names a program this machine does not have is not a workspace nobody
//!   can work in.
//! - **A call that cannot be made is answered in words.** A name nobody
//!   offers, a server that died, a refusal from the far end: all of them come
//!   back as the result text, which is the same discipline the built-in tools
//!   follow and the only one the model can act on.
//!
//! ## Shape
//!
//! [`Hub`] is the public handle: a cheap clone of an `mpsc::Sender`. All
//! state lives in an actor task spawned by [`Hub::spawn`] (or [`Hub::empty`]
//! / [`Hub::of_entries`]); methods on `Hub` send a [`Command`] and await the
//! actor's reply. Nothing on the outside ever holds a `&mut HubInner`, and
//! the actor never shares `HubInner` with anyone — which is what keeps
//! enable / disable / reconnect / disconnect safe to call from any thread.

mod client;
mod config;
mod health;
mod http;
mod inner;
mod stdio;
/// Test fixture: a bash script that speaks the protocol, used by the binary's
/// tests as well as this crate's. Always compiled (not gated behind
/// `#[cfg(test)]`) so a binary test that constructs a stub can reach it
/// through this crate's public surface; the module is `#[doc(hidden)]` so it
/// does not appear in the rendered docs.
#[doc(hidden)]
pub mod stub;
#[cfg(test)]
mod tests;
mod wire;

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use caocli_core::ToolDef;

use inner::HubInner;
pub use inner::{ServerState, ServerStatus};

/// What a tool from a server is named, in front of the tool's own name.
pub const PREFIX: &str = "mcp__";

/// The longest name a tool may have. OpenAI's function names are capped at 64
/// characters, and a session may be talking to any of the providers in the
/// preset table, so every name this client sends is one all of them can read.
const MAX_NAME: usize = 64;

/// Whether a name is one this module answers for. The built-in tools are
/// dispatched by name too, and this is what keeps the two sets apart.
pub fn is_tool(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// What the model sees a server's tool called: `mcp__<server>__<tool>`.
///
/// A name may carry characters a function name may not, and a name may be
/// longer than a backend will take, so both are dealt with here rather than
/// at every use: what is not a letter, a digit, an underscore or a dash
/// becomes an underscore, and a name longer than [`MAX_NAME`] keeps its front
/// and ends with a short hash of the whole. The hash is what keeps two long
/// names from becoming one: cutting a pair of similar names at the same
/// length would otherwise be enough to make them the same name.
pub fn tool_name(server: &str, tool: &str) -> String {
    let name = format!("{PREFIX}{}__{}", sanitize(server), sanitize(tool));
    if name.len() <= MAX_NAME {
        return name;
    }
    let hash = short_hash(&name);
    let mut cut = MAX_NAME - hash.len() - 1;
    // A name is ASCII by construction — [`sanitize`] made it so — and this is
    // belt and braces for a caller that one day does not.
    while cut > 0 && !name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}~{}", &name[..cut], hash)
}

/// Keep what a name may carry and replace the rest. Two different names may
/// come out the same (a space and an underscore are one character to a
/// backend) — that is a collision the hub reports rather than a fix to guess
/// at.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A short, stable hash of a name.
///
/// FNV-1a, written out rather than taken from a crate: the name it is part of
/// is written into the session log, and a later process reading that log has
/// to work out the same name from the same server — so the hash has to be the
/// same on every machine, in every release, forever. A hash that could
/// change is a hash that would silently renumber the tools of a resumed
/// session.
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash & 0xffff_ffff)
}

/// A message to the hub's actor task: do this one thing, then send the result
/// back over `reply`. `None` of these are constructed by callers outside this
/// crate — every method on [`Hub`] builds the one its name implies.
enum Command {
    Call {
        tool: String,
        args: String,
        reply: oneshot::Sender<String>,
    },
    Definitions {
        reply: oneshot::Sender<Arc<Vec<ToolDef>>>,
    },
    Notes {
        reply: oneshot::Sender<Vec<String>>,
    },
    Report {
        reply: oneshot::Sender<Vec<String>>,
    },
    List {
        reply: oneshot::Sender<Vec<ServerStatus>>,
    },
    Enable {
        name: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Disable {
        name: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Reconnect {
        name: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Disconnect {
        name: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// A handle to the hub's actor task.
///
/// `Hub` is `Clone` — every clone is another sender into the same mailbox — so
/// callers can hand it out the way they would an `Arc<T>`, without an `Arc`
/// of their own. Methods are `async + &self` because the actor model is
/// "send a message, wait for the reply": there is no shared `&mut`, no lock,
/// and no surprise about who owns what.
#[derive(Clone)]
pub struct Hub {
    tx: mpsc::Sender<Command>,
}

impl Hub {
    /// A hub with no servers behind it. The actor task is started up at once
    /// and answers every call as though no server were configured: the
    /// "unknown tool" message is the only thing the model ever sees.
    ///
    /// Synchronous because there is nothing to connect to: the actor's inner
    /// state is ready the moment the future starts polling.
    pub fn empty() -> Self {
        spawn_with(async { HubInner::empty() })
    }

    /// Read the workspace and the user-supplied mcpServers, then start an
    /// actor task that connects to everything they name.
    ///
    /// Synchronous: the actor connects in the background, and calls made
    /// before the actor has finished connecting wait in the actor's mailbox
    /// until the connection step is done. A caller that wants the notes up
    /// front calls [`Hub::notes`] after — that is the moment the actor's
    /// inner state has settled into "every server either came up or said why
    /// it didn't".
    pub fn spawn(workspace: &Path, user_settings: Option<&Value>) -> Self {
        let workspace = workspace.to_path_buf();
        let user_settings = user_settings.cloned();
        spawn_with(
            async move { HubInner::from_workspace(&workspace, user_settings.as_ref()).await },
        )
    }

    /// Build a hub over a list of entries the caller already has — the test
    /// path that does not want a configuration file written. Async because
    /// the actor has to finish connecting before the caller can rely on the
    /// hub being ready, and a test that races its own fixture against the
    /// connection step would flake.
    #[doc(hidden)]
    pub async fn of_entries(entries: Vec<config::Entry>) -> Self {
        spawn_with(async move { HubInner::from_entries(entries, Vec::new()).await })
    }

    /// One tool call, answered with the text the model reads. Never fails —
    /// see the module-level third decision.
    pub async fn call(&self, tool: &str, args: &str) -> String {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Call {
                tool: tool.to_string(),
                args: args.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return "error: mcp hub is shut down".into();
        }
        rx.await
            .unwrap_or_else(|_| "error: mcp hub dropped the reply".into())
    }

    /// The tools currently offered to the model. Returned as an `Arc` so the
    /// request builder can hand the slice to the request body without
    /// re-asking the actor every turn.
    pub async fn definitions(&self) -> Arc<Vec<ToolDef>> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::Definitions { reply }).await.is_err() {
            return Arc::new(Vec::new());
        }
        rx.await.unwrap_or_default()
    }

    /// What the session says about MCP when it starts: one line per server
    /// that came up, one per server that did not, and the warnings picked up
    /// along the way.
    pub async fn notes(&self) -> Vec<String> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::Notes { reply }).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// What `/mcp` prints: every server, what it offers, and what is wrong.
    pub async fn report(&self) -> Vec<String> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::Report { reply }).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// One row per server, with the state it stands in and how many of its
    /// tools are being offered. What `/mcp list` prints.
    pub async fn list(&self) -> Vec<ServerStatus> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::List { reply }).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Bring a `Disabled` server back online: open a fresh connection with
    /// its stored configuration, and route its tools to the model again.
    /// Errors out if the server is in any other state (`Ready`, `Failed`,
    /// `Disconnected`) or is not configured.
    pub async fn enable(&self, name: &str) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Enable {
                name: name.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return Err("mcp hub is shut down".into());
        }
        rx.await
            .unwrap_or_else(|_| Err("mcp hub dropped the reply".into()))
    }

    /// Take a `Ready` server offline: close its connection and pull its
    /// tools out of the offered set. The configuration is kept so an
    /// `enable` can bring the server back.
    pub async fn disable(&self, name: &str) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Disable {
                name: name.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return Err("mcp hub is shut down".into());
        }
        rx.await
            .unwrap_or_else(|_| Err("mcp hub dropped the reply".into()))
    }

    /// Reconnect a server in any state: close any live connection, then
    /// open a fresh one with the stored configuration.
    pub async fn reconnect(&self, name: &str) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Reconnect {
                name: name.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return Err("mcp hub is shut down".into());
        }
        rx.await
            .unwrap_or_else(|_| Err("mcp hub dropped the reply".into()))
    }

    /// Close a `Ready` server's connection without touching its
    /// configuration. The server sits in `Disconnected` until `reconnect`
    /// brings it back.
    pub async fn disconnect(&self, name: &str) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Disconnect {
                name: name.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return Err("mcp hub is shut down".into());
        }
        rx.await
            .unwrap_or_else(|_| Err("mcp hub dropped the reply".into()))
    }

    /// End every connection. Used by [`McpGuard`] on the way out — there is
    /// no public reason to shut a hub down from the inside.
    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::Shutdown { reply }).await.is_ok() {
            let _ = rx.await;
        }
    }
}

/// Start an actor task over `init`: a future that builds the initial
/// `HubInner`. Returns a [`Hub`] the caller can send to.
fn spawn_with<F>(init: F) -> Hub
where
    F: std::future::Future<Output = HubInner> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    tokio::spawn(async move {
        let mut inner = init.await;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Command::Call { tool, args, reply } => {
                    let result = inner.call(&tool, &args).await;
                    let _ = reply.send(result);
                }
                Command::Definitions { reply } => {
                    let _ = reply.send(inner.definitions());
                }
                Command::Notes { reply } => {
                    let _ = reply.send(inner.notes());
                }
                Command::Report { reply } => {
                    let _ = reply.send(inner.report());
                }
                Command::List { reply } => {
                    let _ = reply.send(inner.list());
                }
                Command::Enable { name, reply } => {
                    let _ = reply.send(inner.enable(&name).await);
                }
                Command::Disable { name, reply } => {
                    let _ = reply.send(inner.disable(&name).await);
                }
                Command::Reconnect { name, reply } => {
                    let _ = reply.send(inner.reconnect(&name).await);
                }
                Command::Disconnect { name, reply } => {
                    let _ = reply.send(inner.disconnect(&name).await);
                }
                Command::Shutdown { reply } => {
                    inner.shutdown_all().await;
                    let _ = reply.send(());
                    break;
                }
            }
        }
    });
    Hub { tx }
}

/// A guard around a [`Hub`] that captures the connection notes on the way up
/// and shuts the connections down on the way out.
///
/// Used in `main::run` so the three exit paths (one-shot, TUI, plain REPL)
/// all clean up the same way: by going out of scope.
///
/// `Hub::shutdown` is async, and Rust's `Drop` is sync, so the guard spawns
/// shutdown as a detached task on the current tokio runtime. The connections
/// close on the executor even after the main task has returned. When no
/// runtime is reachable — an early exit that unwinds before the runtime is
/// set up — process exit takes care of the children instead.
pub struct McpGuard {
    hub: Option<Hub>,
    notes: Vec<String>,
}

impl McpGuard {
    /// Wrap a freshly-spawned hub, capturing the notes the actor produced on
    /// the way up. The notes are kept here so they survive any number of
    /// borrows the rest of the program takes of the hub.
    pub async fn new(hub: Hub) -> Self {
        let notes = hub.notes().await;
        Self {
            hub: Some(hub),
            notes,
        }
    }

    /// The hub for the agent to share. The clone keeps the guard alive: the
    /// guard drops last, after the agent and its `Hub` clones are gone.
    pub fn hub(&self) -> Hub {
        self.hub
            .as_ref()
            .expect("hub is taken only on drop")
            .clone()
    }

    /// What the hub said on the way up — for the banner.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

impl Drop for McpGuard {
    fn drop(&mut self) {
        let Some(hub) = self.hub.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                hub.shutdown().await;
            });
        }
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[tokio::test]
    async fn guard_holds_notes_from_a_freshly_connected_hub() {
        let hub = Hub::empty();
        let guard = McpGuard::new(hub).await;
        assert!(guard.notes().is_empty(), "an empty hub has no notes");
    }

    #[test]
    fn drop_without_a_runtime_does_not_panic() {
        // The Hub's connections are owned; a runtime is what would actually
        // shut them down. Without one, the OS reaps the children on exit,
        // and the guard must not panic on its way out.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let hub = Hub::empty();
            let guard = McpGuard::new(hub).await;
            drop(guard);
        });
    }
}
