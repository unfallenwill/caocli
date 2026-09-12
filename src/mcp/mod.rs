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
//!   servers may offer a tool called `search`, and the model has to be able to
//!   say which one it means: [`tool_name`] is `mcp__<server>__<tool>`, the
//!   convention every other client of this protocol uses, so a tool the model
//!   learned from one is the same name in another.
//! - **A server that does not come up costs nothing else.** The model is
//!   offered the tools of the servers that did, the session runs, and `/mcp`
//!   says which server is missing and why — a workspace whose `.mcp.json` names
//!   a program this machine does not have is not a workspace nobody can work in.
//! - **A call that cannot be made is answered in words.** A name nobody offers,
//!   a server that died, a refusal from the far end: all of them come back as
//!   the result text, which is the same discipline the built-in tools follow
//!   and the only one the model can act on.

mod client;
mod config;
mod health;
mod http;
mod stdio;
#[cfg(test)]
pub(crate) mod stub;
#[cfg(test)]
mod tests;
mod wire;

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::types::{FunctionDef, ToolDef};

use client::Connection;
use config::Entry;

/// What a tool from a server is named, in front of the tool's own name.
pub const PREFIX: &str = "mcp__";

/// The longest name a tool may have. OpenAI's function names are capped at 64
/// characters, and a session may be talking to any of the providers in the
/// preset table, so every name this client sends is one all of them can read.
const MAX_NAME: usize = 64;

/// How many names a failure or a report lists before it says how many more
/// there are: the model reads a tool list in the request already, and a user
/// reading `/mcp` is looking for a name they have in mind.
const NAMES_SHOWN: usize = 12;

/// Whether a name is one this module answers for. The built-in tools are
/// dispatched by name too, and this is what keeps the two sets apart.
pub fn is_tool(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// What the model sees a server's tool called: `mcp__<server>__<tool>`.
///
/// A name may carry characters a function name may not, and a name may be
/// longer than a backend will take, so both are dealt with here rather than at
/// every use: what is not a letter, a digit, an underscore or a dash becomes an
/// underscore, and a name longer than [`MAX_NAME`] keeps its front and ends with
/// a short hash of the whole. The hash is what keeps two long names from
/// becoming one: cutting a pair of similar names at the same length would
/// otherwise be enough to make them the same name.
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
/// come out the same (a space and an underscore are one character to a backend)
/// — that is a collision the hub reports rather than a fix to guess at.
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
/// is written into the session log, and a later process reading that log has to
/// work out the same name from the same server — so the hash has to be the same
/// on every machine, in every release, forever. A hash that could change is a
/// hash that would silently renumber the tools of a resumed session.
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash & 0xffff_ffff)
}

/// One server as the session has it: what the configuration called it, how to
/// reach it, and how it stands.
struct Server {
    name: String,
    /// What the entry says it is — the command line, or the url — for the
    /// report. Readable rather than parseable: nothing decides anything by it.
    how: String,
    /// The file that named it, so that a server in a project's `.mcp.json` is
    /// told apart from one in the user's own settings.
    from: String,
    state: State,
}

/// How a server stands.
enum State {
    /// It came up, and it offered this many tools. The connection is boxed for
    /// the same reason `WireRequest`'s payloads are: what it carries is a
    /// transport — a client, a process, a map of waiting requests — and a
    /// `Server` has no business being that size to say it did not start.
    Ready {
        connection: Box<Connection>,
        tools: usize,
    },
    /// It did not, and this is what there is to say about it.
    Failed(String),
}

/// Where a tool the model named is: which server offers it, and what it is
/// called there.
struct Route {
    server: usize,
    tool: String,
}

/// Every server a session talks to, and the tools they offer between them.
///
/// The order the tools are offered in is the order the servers are in the
/// configuration — sorted by name — and, within a server, the order it listed
/// them. It is part of every request the session sends, so it has to be the same
/// on every run of the same configuration.
pub struct Hub {
    servers: Vec<Server>,
    tools: Vec<ToolDef>,
    routes: HashMap<String, Route>,
    /// What had to be said on the way: an entry that cannot be used, a variable
    /// that is not set, two tools that would carry one name.
    warnings: Vec<String>,
}

impl Hub {
    /// Nobody to talk to: the hub a session has when there is no configuration,
    /// and the one every test that is not about MCP gets.
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
            tools: Vec::new(),
            routes: HashMap::new(),
            warnings: Vec::new(),
        }
    }

    /// Read the configuration and connect to everything it names.
    ///
    /// All of them at once: a session's first request waits for the tools of
    /// every server, and doing this one server at a time would make that wait
    /// the sum of theirs. A server that is slow to start is slow either way, and
    /// this way it is no one else's cost.
    pub async fn connect(workspace: &Path) -> Self {
        let (entries, warnings) = config::entries(workspace);
        Self::open(entries, warnings).await
    }

    /// The same over entries that are already read: the file is where a session
    /// gets them from, and a test that has its own entries — a stub server, and
    /// no file to write — has no reason to go through one.
    async fn open(entries: Vec<Entry>, warnings: Vec<String>) -> Self {
        let opened = futures_util::future::join_all(entries.iter().map(|entry| async move {
            match &entry.config {
                Err(why) => Err(why.clone()),
                Ok(config) => Connection::open(config).await,
            }
        }))
        .await;
        Self::assemble(entries, opened, warnings)
    }

    /// [`Hub::open`] for the tests of the dispatch out in `tools`, which need a
    /// hub with a server behind it and no configuration file to read.
    #[cfg(test)]
    pub(crate) async fn of_entries(entries: Vec<Entry>) -> Self {
        Self::open(entries, Vec::new()).await
    }

    /// Build the hub from entries and whatever came of opening them: what the
    /// names are, which of them are offered, and what has to be said.
    fn assemble(
        entries: Vec<Entry>,
        opened: Vec<Result<Connection, String>>,
        warnings: Vec<String>,
    ) -> Self {
        let mut hub = Self {
            servers: Vec::new(),
            tools: Vec::new(),
            routes: HashMap::new(),
            warnings,
        };
        for (entry, opened) in entries.into_iter().zip(opened) {
            let how = how_to_reach(&entry);
            let connection = match opened {
                Ok(connection) => connection,
                Err(why) => {
                    hub.servers.push(Server {
                        name: entry.name,
                        how,
                        from: entry.from,
                        state: State::Failed(why),
                    });
                    continue;
                }
            };
            let index = hub.servers.len();
            let mut offered = 0;
            for tool in &connection.tools {
                let name = tool_name(&entry.name, &tool.name);
                if hub.routes.contains_key(&name) {
                    hub.warnings.push(format!(
                        "{name}: two tools would carry this name, so the one from {} is not \
                         offered",
                        entry.name
                    ));
                    continue;
                }
                hub.routes.insert(
                    name.clone(),
                    Route {
                        server: index,
                        tool: tool.name.clone(),
                    },
                );
                hub.tools.push(ToolDef {
                    r#type: "function".into(),
                    function: FunctionDef {
                        name,
                        description: tool.description.clone(),
                        parameters: Some(tool.schema.clone()),
                    },
                });
                offered += 1;
            }
            for note in &connection.notes {
                hub.warnings.push(format!("{}: {note}", entry.name));
            }
            hub.servers.push(Server {
                name: entry.name,
                how,
                from: entry.from,
                state: State::Ready {
                    connection: Box::new(connection),
                    tools: offered,
                },
            });
        }
        hub
    }

    /// The tools the servers offer, in the order they are offered in: appended
    /// to the built-in tools, which is where a tool added later goes so that
    /// every prefix a session has already sent stays what it was.
    pub fn definitions(&self) -> &[ToolDef] {
        &self.tools
    }

    /// One tool call, answered with the text the model reads. Never fails: see
    /// the module's third decision.
    pub async fn call(&self, name: &str, args_json: &str) -> String {
        let Some(route) = self.routes.get(name) else {
            return format!(
                "error: no MCP tool named {name:?} is offered. The MCP tools this session has \
                 are: {}",
                self.offered_names()
            );
        };
        let server = &self.servers[route.server];
        let State::Ready { connection, .. } = &server.state else {
            // Unreachable: a route exists only for a server that came up. Said
            // rather than assumed away, because a panic here would end a turn.
            return format!("error: mcp server {} is not connected", server.name);
        };
        let arguments = match serde_json::from_str::<Value>(args_json) {
            Ok(Value::Object(fields)) => Value::Object(fields),
            Ok(_) => {
                return format!("error: the arguments of {name} have to be a JSON object");
            }
            Err(e) => {
                return format!("error: the arguments of {name} are not valid JSON: {e}");
            }
        };
        match connection.call(&route.tool, arguments).await {
            Ok(text) => text,
            Err(why) => format!(
                "error: mcp server {} could not run {}: {why}",
                server.name, route.tool
            ),
        }
    }

    /// The names of the tools it offers, as the model sees them, cut to the
    /// first few so that a failure stays a sentence.
    fn offered_names(&self) -> String {
        if self.tools.is_empty() {
            return "none came up (see /mcp for which servers did not)".to_string();
        }
        let names: Vec<&str> = self
            .tools
            .iter()
            .take(NAMES_SHOWN)
            .map(|tool| tool.function.name.as_str())
            .collect();
        let rest = self.tools.len() - names.len();
        let mut listed = names.join(", ");
        if rest > 0 {
            listed.push_str(&format!(" (and {rest} more)"));
        }
        listed
    }

    /// What the session says about MCP when it starts: one line for the servers
    /// that came up, one for each that did not, and the notes from the way in.
    pub fn notes(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let ready: Vec<&Server> = self
            .servers
            .iter()
            .filter(|server| matches!(server.state, State::Ready { .. }))
            .collect();
        if !ready.is_empty() {
            lines.push(format!(
                "mcp: {} · {} tools · /mcp names them",
                ready
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                self.tools.len()
            ));
        }
        for server in &self.servers {
            if let State::Failed(why) = &server.state {
                lines.push(format!("mcp: {} did not start: {why}", server.name));
            }
        }
        lines.extend(self.warnings.iter().map(|note| format!("mcp: {note}")));
        lines
    }

    /// What `/mcp` prints: every server, what it offers, and what is wrong.
    pub fn report(&self) -> Vec<String> {
        if self.servers.is_empty() && self.warnings.is_empty() {
            return vec![
                "no MCP servers: add them under \"mcpServers\" in ~/.caocli/settings.json or in \
                 this workspace's .mcp.json"
                    .to_string(),
            ];
        }
        let mut lines = Vec::new();
        for (index, server) in self.servers.iter().enumerate() {
            match &server.state {
                State::Ready { connection, tools } => lines.push(format!(
                    "{} · {} · {} tools · {} · protocol {} · from {}",
                    server.name,
                    server.how,
                    tools,
                    connection.identity,
                    connection.protocol,
                    server.from
                )),
                State::Failed(why) => lines.push(format!(
                    "{} · {} · did not start: {why} · from {}",
                    server.name, server.how, server.from
                )),
            }
            // The names the model sees, which are the ones a user has to know
            // to say "that one" about: `mcp__<server>__<tool>`, cut to the first
            // few rather than printed as a wall. Which names are this server's
            // is read off the routes rather than puzzled out of the prefix: a
            // name that collided belongs to the server that got it.
            let names: Vec<&str> = self
                .tools
                .iter()
                .filter(|tool| {
                    self.routes
                        .get(&tool.function.name)
                        .is_some_and(|route| route.server == index)
                })
                .map(|tool| tool.function.name.as_str())
                .collect();
            if !names.is_empty() {
                let shown: Vec<&str> = names.iter().take(NAMES_SHOWN).copied().collect();
                let rest = names.len() - shown.len();
                let mut line = format!("  {}", shown.join(", "));
                if rest > 0 {
                    line.push_str(&format!(" (and {rest} more)"));
                }
                lines.push(line);
            }
        }
        lines.extend(self.warnings.iter().map(|note| format!("warning: {note}")));
        lines
    }

    /// End every connection: what a session does on its way out, so that the
    /// programs it started do not outlive it.
    pub async fn shutdown(&self) {
        futures_util::future::join_all(self.servers.iter().filter_map(
            |server| match &server.state {
                State::Ready { connection, .. } => Some(connection.shutdown()),
                State::Failed(_) => None,
            },
        ))
        .await;
    }
}

/// What the entry says it is, for the report: the command line it runs, or the
/// url it calls. Quoted where the parts carry spaces, so that a reader can see
/// where one ends and the next begins.
fn how_to_reach(entry: &Entry) -> String {
    let Ok(config) = &entry.config else {
        return "nothing usable in the entry".to_string();
    };
    match (&config.command, &config.url) {
        (Some(command), _) => {
            let mut line = format!("stdio: {command}");
            for arg in &config.args {
                if arg.contains(' ') {
                    line.push_str(&format!(" {arg:?}"));
                } else {
                    line.push_str(&format!(" {arg}"));
                }
            }
            line
        }
        (None, Some(url)) => url.clone(),
        (None, None) => "nowhere".to_string(),
    }
}
