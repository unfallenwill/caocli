//! The hub's internal state: every server, the tools they offer, and where a
//! call lands.
//!
//! `Hub` is the channel that talks to this from the outside; the actor task
//! holds the only `HubInner`. The split is on purpose — the protocol and the
//! routing are the protocol layer's, and `HubInner` is the only thing that
//! has to know both at once.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use caocli_core::ToolDef;

use super::client::Connection;
use super::config::{self, Entry};
use super::tool_name;

/// What a tool the model named is: which server offers it, and what it is
/// called there.
struct Route {
    server: String,
    tool: String,
}

/// The state of one server, as the hub sees it.
#[allow(clippy::large_enum_variant)] // The actor task owns this; boxing Connection would
                                    // add a heap indirection the only consumer never shares.
enum ServerState {
    /// It came up, and it offered this many tools.
    Ready {
        connection: Connection,
        tools: Vec<String>,
    },
    /// It did not, and this is what there is to say about it.
    Failed(String),
}

/// Everything `Hub` knows about one configured server — both the parts the
/// configuration file said (name, what it is, where it came from) and the
/// parts the connection attempt found out (the state it stands in).
struct ServerEntry {
    /// What the entry says it is — the command line, or the url — for the
    /// report. Readable rather than parseable: nothing decides anything by it.
    how: String,
    /// The file that named it, so that a server in a project's `.mcp.json` is
    /// told apart from one in the user's own settings.
    from: String,
    /// The parsed configuration, kept around so a future reconnect does not
    /// have to re-read the file. `Err` is the same reason the connection
    /// itself failed, kept here so the report can name it without having to
    /// look up the file again.
    #[allow(dead_code)] // Phase 4 reconnect reads this; today nothing does.
    config: Result<config::ServerConfig, String>,
    state: ServerState,
}

/// Every server a session talks to, and the tools they offer between them.
///
/// The actor task holds one of these and never shares it. The order the
/// tools are offered in is the order the servers are in the configuration —
/// sorted by name — and, within a server, the order it listed them. It is
/// part of every request the session sends, so it has to be the same on
/// every run of the same configuration.
pub(super) struct HubInner {
    servers: BTreeMap<String, ServerEntry>,
    /// The tools currently offered to the model, in the order the agent's
    /// request builder must send them. Two parallel data structures on
    /// purpose: `tool_order` keeps the position each tool sits at (the order
    /// matters for KV-cache prefix stability), `offered` keeps the name ->
    /// tool lookup (the lookup matters for `definitions()` and for
    /// enable/disable mutations that need to keep `tool_order` in step).
    tool_order: Vec<String>,
    offered: HashMap<String, ToolDef>,
    /// Public name -> which server offers it and what it is called there.
    routes: HashMap<String, Route>,
    /// What had to be said on the way: an entry that cannot be used, a
    /// variable that is not set, two tools that would carry one name.
    warnings: Vec<String>,
}

impl HubInner {
    /// Nobody to talk to: the hub a session has when there is no
    /// configuration, and the one every test that is not about MCP gets.
    pub(super) fn empty() -> Self {
        Self {
            servers: BTreeMap::new(),
            tool_order: Vec::new(),
            offered: HashMap::new(),
            routes: HashMap::new(),
            warnings: Vec::new(),
        }
    }

    /// Read the configuration and connect to everything it names.
    ///
    /// All of them at once: a session's first request waits for the tools of
    /// every server, and doing this one server at a time would make that wait
    /// the sum of theirs. A server that is slow to start is slow either way,
    /// and this way it is no one else's cost.
    pub(super) async fn from_workspace(workspace: &Path, user_settings: Option<&Value>) -> Self {
        let (entries, warnings) = config::entries(workspace, user_settings);
        Self::from_entries(entries, warnings).await
    }

    /// The same over entries that are already read: the file is where a
    /// session gets them from, and a test that has its own entries — a stub
    /// server, and no file to write — has no reason to go through one.
    pub(super) async fn from_entries(entries: Vec<Entry>, warnings: Vec<String>) -> Self {
        let opened = futures_util::future::join_all(entries.iter().map(|entry| async move {
            match &entry.config {
                Err(why) => Err(why.clone()),
                Ok(config) => Connection::open(config).await,
            }
        }))
        .await;
        Self::assemble(entries, opened, warnings)
    }

    /// Build the hub from entries and whatever came of opening them: what
    /// the names are, which of them are offered, and what has to be said.
    fn assemble(
        entries: Vec<Entry>,
        opened: Vec<Result<Connection, String>>,
        warnings: Vec<String>,
    ) -> Self {
        let mut inner = Self {
            servers: BTreeMap::new(),
            tool_order: Vec::new(),
            offered: HashMap::new(),
            routes: HashMap::new(),
            warnings,
        };
        for (entry, opened) in entries.into_iter().zip(opened) {
            let how = how_to_reach(&entry);
            let connection = match opened {
                Ok(connection) => connection,
                Err(why) => {
                    inner.servers.insert(
                        entry.name.clone(),
                        ServerEntry {
                            how,
                            from: entry.from,
                            config: entry.config,
                            state: ServerState::Failed(why),
                        },
                    );
                    continue;
                }
            };
            let mut offered_names = Vec::with_capacity(connection.tools.len());
            for tool in &connection.tools {
                let name = tool_name(&entry.name, &tool.name);
                if inner.routes.contains_key(&name) {
                    inner.warnings.push(format!(
                        "{name}: two tools would carry this name, so the one from {} is not \
                         offered",
                        entry.name
                    ));
                    continue;
                }
                inner.routes.insert(
                    name.clone(),
                    Route {
                        server: entry.name.clone(),
                        tool: tool.name.clone(),
                    },
                );
                inner.offered.insert(
                    name.clone(),
                    ToolDef {
                        r#type: "function".into(),
                        function: caocli_core::FunctionDef {
                            name: name.clone(),
                            description: tool.description.clone(),
                            parameters: Some(tool.schema.clone()),
                        },
                    },
                );
                inner.tool_order.push(name);
                offered_names.push(tool.name.clone());
            }
            for note in &connection.notes {
                inner.warnings.push(format!("{}: {note}", entry.name));
            }
            inner.servers.insert(
                entry.name.clone(),
                ServerEntry {
                    how,
                    from: entry.from,
                    config: entry.config,
                    state: ServerState::Ready {
                        connection,
                        tools: offered_names,
                    },
                },
            );
        }
        inner
    }

    /// One tool call, answered with the text the model reads. Never fails:
    /// a name nobody offers, a server that died, a refusal from the far end —
    /// all of them come back as the result text.
    pub(super) async fn call(&self, name: &str, args_json: &str) -> String {
        let Some(route) = self.routes.get(name) else {
            return format!(
                "error: no MCP tool named {name:?} is offered. The MCP tools this session has \
                 are: {}",
                self.offered_names()
            );
        };
        let Some(server) = self.servers.get(&route.server) else {
            // Unreachable: a route exists only for a server that came up. Said
            // rather than assumed away, because a panic here would end a turn.
            return format!("error: mcp server {} is not connected", route.server);
        };
        let ServerState::Ready { connection, .. } = &server.state else {
            return format!("error: mcp server {} is not connected", route.server);
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
                route.server, route.tool
            ),
        }
    }

    /// The tools currently offered, in the order a request should send them
    /// in. Returned as an `Arc` so the caller — the agent's request builder —
    /// can hold onto it without re-asking the actor every turn.
    pub(super) fn definitions(&self) -> Arc<Vec<ToolDef>> {
        Arc::new(
            self.tool_order
                .iter()
                .filter_map(|name| self.offered.get(name))
                .cloned()
                .collect(),
        )
    }

    /// What the session says about MCP when it starts: one line for the
    /// servers that came up, one for each that did not, and the notes from
    /// the way in.
    pub(super) fn notes(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let ready: Vec<&str> = self
            .servers
            .iter()
            .filter_map(|(name, entry)| match entry.state {
                ServerState::Ready { .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        if !ready.is_empty() {
            lines.push(format!(
                "mcp: {} · {} tools · /mcp names them",
                ready.join(", "),
                self.tool_order.len()
            ));
        }
        for (name, entry) in &self.servers {
            if let ServerState::Failed(why) = &entry.state {
                lines.push(format!("mcp: {name} did not start: {why}"));
            }
        }
        lines.extend(self.warnings.iter().map(|note| format!("mcp: {note}")));
        lines
    }

    /// What `/mcp` prints: every server, what it offers, and what is wrong.
    pub(super) fn report(&self) -> Vec<String> {
        const NAMES_SHOWN: usize = 12;
        if self.servers.is_empty() && self.warnings.is_empty() {
            return vec![
                "no MCP servers: add them under \"mcpServers\" in ~/.caocli/settings.json or in \
                 this workspace's .mcp.json"
                    .to_string(),
            ];
        }
        let mut lines = Vec::new();
        for (name, entry) in &self.servers {
            match &entry.state {
                ServerState::Ready { connection, tools } => lines.push(format!(
                    "{name} · {} · {} tools · {} · protocol {} · from {}",
                    entry.how,
                    tools.len(),
                    connection.identity,
                    connection.protocol,
                    entry.from
                )),
                ServerState::Failed(why) => lines.push(format!(
                    "{name} · {} · did not start: {why} · from {}",
                    entry.how, entry.from
                )),
            }
            // The names the model sees, which are the ones a user has to know
            // to say "that one" about: `mcp__<server>__<tool>`, cut to the
            // first few rather than printed as a wall. Which names are this
            // server's is read off the routes rather than puzzled out of the
            // prefix: a name that collided belongs to the server that got it.
            let names: Vec<&str> = self
                .routes
                .iter()
                .filter(|(_, route)| route.server == *name)
                .map(|(public, _)| public.as_str())
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
    pub(super) async fn shutdown_all(&mut self) {
        let mut connections: Vec<Connection> = Vec::new();
        for entry in self.servers.values_mut() {
            if let ServerState::Ready { connection, .. } =
                std::mem::replace(&mut entry.state, ServerState::Failed("shut down".into()))
            {
                connections.push(connection);
            }
        }
        futures_util::future::join_all(connections.iter().map(|c| c.shutdown())).await;
    }

    /// The names it offers, as the model sees them, cut to the first few so
    /// that a failure stays a sentence.
    fn offered_names(&self) -> String {
        const NAMES_SHOWN: usize = 12;
        if self.tool_order.is_empty() {
            return "none came up (see /mcp for which servers did not)".to_string();
        }
        let names: Vec<&str> = self
            .tool_order
            .iter()
            .take(NAMES_SHOWN)
            .map(String::as_str)
            .collect();
        let rest = self.tool_order.len() - names.len();
        let mut listed = names.join(", ");
        if rest > 0 {
            listed.push_str(&format!(" (and {rest} more)"));
        }
        listed
    }
}

/// What the entry says it is, for the report: the command line it runs, or
/// the url it calls. Quoted where the parts carry spaces, so that a reader
/// can see where one ends and the next begins.
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

// `Entry` and `ServerConfig` come from `super::config`. Tests reach them as
// `super::config::Entry`; production callers don't need them.
