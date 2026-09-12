//! Where the servers come from: two files the user writes, merged into one
//! table.
//!
//! `~/.caocli/settings.json` holds the servers that are the user's own — the
//! ones every workspace gets — and the workspace's `.mcp.json` holds the ones
//! that belong to a project (the file Claude Code reads, so a repository that
//! carries one is not asked to carry a second).
//!
//! The project file wins by name: it is the more specific of the two, and a
//! project that names a server the user also has means its own. The table is
//! sorted by name rather than kept in file order, so that the tool list built
//! from it — which is part of the request prefix — is the same on every run of
//! the same configuration.
//!
//! Everything a file can get wrong is a sentence rather than a failure: one
//! unusable server must not cost the user the ones that do work, and an entry
//! nobody can read is still an entry `/mcp` can report.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// One server as a configuration file writes it:
///
/// ```json
/// {"mcpServers": {"files": {"command": "npx", "args": ["-y", "mcp-files", "/tmp"]}}}
/// ```
///
/// `command` (with `args` and `env`) and `url` (with `headers`) are the two
/// ways to reach a server, and an entry carries one of them. `type` says which
/// one out loud — it is optional and the fields already say it — and `timeout`
/// is how long this server may take to start and answer, in seconds, for the
/// ones that are slow to come up.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// One server the configuration asks for: the name it answers to, the file that
/// named it, and what it takes to reach it — or the reason it cannot be used.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// The file the entry was read from, as `/mcp` reports it.
    pub from: String,
    pub config: Result<ServerConfig, String>,
}

/// The servers to run, in the order their tools are offered, and what had to be
/// said about the entries on the way (a variable that is not set, a file that
/// cannot be read).
pub fn entries(workspace: &Path) -> (Vec<Entry>, Vec<String>) {
    let mut named: BTreeMap<String, (String, Value)> = BTreeMap::new();
    let mut warnings = Vec::new();

    match crate::config::setting("mcpServers") {
        Ok(Some(value)) => collect(value, "settings.json", &mut named, &mut warnings),
        Ok(None) => {}
        // The file this came from is also where the API keys live, so a
        // problem reading it is reported like any other and the run goes on:
        // without a key the first request says so, and without servers this
        // session is the one it would have been before MCP existed.
        Err(e) => warnings.push(format!("mcpServers: {e:#}")),
    }

    let path = workspace.join(".mcp.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) => match value.get("mcpServers").cloned() {
                Some(table) => collect(table, ".mcp.json", &mut named, &mut warnings),
                // The file is the project's own copy of the format, so a table
                // under another name is a file that will never be read: said
                // rather than passed over, because the servers in it are ones
                // nobody would see again.
                None => warnings.push(format!(
                    "{}: has no mcpServers table, so nothing in it is read",
                    path.display()
                )),
            },
            Err(e) => warnings.push(format!("{}: cannot be read: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warnings.push(format!("{}: cannot be read: {e}", path.display())),
    }

    let entries = named
        .into_iter()
        .map(|(name, (from, raw))| {
            let config = read(&name, &raw, &mut warnings);
            Entry { name, from, config }
        })
        .collect();
    (entries, warnings)
}

/// Take one file's `mcpServers` table into the table being built.
fn collect(
    value: Value,
    from: &str,
    named: &mut BTreeMap<String, (String, Value)>,
    warnings: &mut Vec<String>,
) {
    let Value::Object(table) = value else {
        warnings.push(format!("{from}: mcpServers is not an object"));
        return;
    };
    for (name, raw) in table {
        if name.trim().is_empty() {
            warnings.push(format!("{from}: a server with no name is skipped"));
            continue;
        }
        // A name defined in both files is the project's: it is the more
        // specific of the two, and the one the person who runs it can see.
        named.insert(name, (from.to_string(), raw));
    }
}

/// One entry, read: the fields with their variables resolved, and the sentence
/// that says why it cannot be used when it cannot be.
fn read(name: &str, raw: &Value, warnings: &mut Vec<String>) -> Result<ServerConfig, String> {
    let mut config: ServerConfig =
        serde_json::from_value(raw.clone()).map_err(|e| format!("cannot be read: {e}"))?;
    let mut missing: Vec<String> = Vec::new();
    config.command = config.command.map(|text| expand(&text, &mut missing));
    config.args = config
        .args
        .iter()
        .map(|arg| expand(arg, &mut missing))
        .collect();
    config.env = config
        .env
        .iter()
        .map(|(key, value)| (expand(key, &mut missing), expand(value, &mut missing)))
        .collect();
    config.url = config.url.map(|text| expand(&text, &mut missing));
    config.headers = config
        .headers
        .iter()
        .map(|(key, value)| (expand(key, &mut missing), expand(value, &mut missing)))
        .collect();
    missing.dedup();
    for var in missing {
        warnings.push(format!(
            "{name}: ${{{var}}} is not set, so the entry keeps the text as written"
        ));
    }

    let stdio = config.command.is_some();
    let http = config.url.is_some();
    match (stdio, http, config.kind.as_deref()) {
        (true, true, _) => {
            return Err("names both a command and a url; it is one or the other".into());
        }
        (false, false, _) => {
            return Err(
                "names neither a command nor a url, so nothing says how to reach it".into(),
            );
        }
        (true, false, Some(kind)) if kind != "stdio" => {
            return Err(format!("is started by a command but its type is {kind:?}"));
        }
        (false, true, Some(kind)) if !matches!(kind, "http" | "sse") => {
            return Err(format!("names a url but its type is {kind:?}"));
        }
        (false, true, _) => {
            let url = config.url.as_deref().unwrap_or_default();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!("url {url:?} is neither http nor https"));
            }
        }
        _ => {}
    }
    if config.timeout == Some(0) {
        return Err("timeout must be at least 1 second".into());
    }
    Ok(config)
}

/// `${VAR}` in a value, and `${VAR:-default}` for the ones that are allowed to
/// be unset, the way the project file format writes them.
///
/// A reference to a variable that is not set and has no default is left exactly
/// as it was written — the text is what the user typed, and a value silently
/// turned into nothing is a server that starts and cannot work. Its name is
/// collected so the run can say which one to set. `$${...}` has no escape: the
/// format is JSON, and a value that is not a reference is not touched.
fn expand(text: &str, missing: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // No closing brace: not a reference, just text that opens with the
            // two characters a reference opens with.
            out.push_str(&rest[start..]);
            return out;
        };
        let body = &after[..end];
        match body.split_once(":-") {
            Some((name, default)) => match lookup(name) {
                Some(value) => out.push_str(&value),
                None => out.push_str(default),
            },
            None => match lookup(body) {
                Some(value) => out.push_str(&value),
                None => {
                    // A name that is empty is not a name, and a warning about
                    // it would be a warning about nothing.
                    if !body.is_empty() {
                        missing.push(body.to_string());
                    }
                    out.push_str(&rest[start..start + 2 + end + 1]);
                }
            },
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// One variable, as a reference names it. A name that is empty or carries
/// something other than the characters a variable name is made of is not a
/// reference to anything, so it reads as unset.
fn lookup(name: &str) -> Option<String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{env_lock, scratch_home};
    use serde_json::json;

    /// One entry's config, as the tests that are not about the files read it.
    fn parsed(raw: Value) -> Result<ServerConfig, String> {
        let mut warnings = Vec::new();
        read("test", &raw, &mut warnings)
    }

    /// The environment the expansion tests read: the lock is held for as long
    /// as the guard lives, because the process has one environment and a test
    /// that moved it under another test's feet would be a flake.
    fn expansion_env() -> std::sync::MutexGuard<'static, ()> {
        let guard = env_lock();
        unsafe {
            std::env::set_var("MCP_TOKEN", "token");
            std::env::remove_var("MCP_NOPE");
        }
        guard
    }

    /// A directory of the test's own, so that two tests writing a `.mcp.json`
    /// at once do not write each other's.
    fn workspace() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "caocli-mcp-cfg-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_command_entry_is_a_stdio_server() {
        let config = parsed(json!({
            "command": "npx",
            "args": ["-y", "mcp-files"],
            "env": {"TOKEN": "t"}
        }))
        .unwrap();
        assert_eq!(config.command.as_deref(), Some("npx"));
        assert_eq!(config.args, vec!["-y", "mcp-files"]);
        assert_eq!(config.env.get("TOKEN").map(String::as_str), Some("t"));
        assert!(config.url.is_none());
    }

    #[test]
    fn a_url_entry_is_an_http_server() {
        let config = parsed(json!({
            "url": "https://example.test/mcp",
            "headers": {"Authorization": "Bearer t"}
        }))
        .unwrap();
        assert_eq!(config.url.as_deref(), Some("https://example.test/mcp"));
        assert_eq!(
            config.headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
    }

    #[test]
    fn an_entry_says_how_to_reach_the_server_exactly_once() {
        let both = parsed(json!({"command": "npx", "url": "https://x.test/mcp"})).unwrap_err();
        assert!(both.contains("both a command and a url"), "{both}");
        let neither = parsed(json!({})).unwrap_err();
        assert!(neither.contains("neither a command nor a url"), "{neither}");
    }

    #[test]
    fn the_type_must_agree_with_the_fields() {
        assert!(parsed(json!({"command": "npx", "type": "stdio"})).is_ok());
        assert!(parsed(json!({"url": "https://x.test", "type": "http"})).is_ok());
        // A url may call itself `sse`: the old transport's name for the same
        // thing, and one a file copied from another client can carry.
        assert!(parsed(json!({"url": "https://x.test", "type": "sse"})).is_ok());
        let wrong = parsed(json!({"command": "npx", "type": "http"})).unwrap_err();
        assert!(wrong.contains("type is \"http\""), "{wrong}");
        let other = parsed(json!({"url": "https://x.test", "type": "websocket"})).unwrap_err();
        assert!(other.contains("type is \"websocket\""), "{other}");
        let unknown = parsed(json!({"command": "npx", "type": "streamable"})).unwrap_err();
        assert!(unknown.contains("streamable"), "{unknown}");
    }

    #[test]
    fn a_url_that_is_not_http_is_refused() {
        let err = parsed(json!({"url": "ftp://x.test/mcp"})).unwrap_err();
        assert!(err.contains("neither http nor https"), "{err}");
        let err = parsed(json!({"url": "example.test/mcp"})).unwrap_err();
        assert!(err.contains("neither http nor https"), "{err}");
    }

    #[test]
    fn a_timeout_of_no_time_is_refused() {
        assert!(parsed(json!({"command": "npx", "timeout": 0})).is_err());
        assert_eq!(
            parsed(json!({"command": "npx", "timeout": 120}))
                .unwrap()
                .timeout,
            Some(120)
        );
    }

    #[test]
    fn an_entry_that_is_not_an_object_is_reported() {
        let err = parsed(json!("npx -y mcp-files")).unwrap_err();
        assert!(err.contains("cannot be read"), "{err}");
    }

    #[test]
    fn a_variable_reference_is_resolved() {
        let _g = expansion_env();
        let mut missing = Vec::new();
        assert_eq!(expand("Bearer ${MCP_TOKEN}", &mut missing), "Bearer token");
        assert!(missing.is_empty());
        assert_eq!(
            expand("no reference here", &mut missing),
            "no reference here"
        );
        assert_eq!(
            expand("${MCP_TOKEN}${MCP_TOKEN}", &mut missing),
            "tokentoken"
        );
    }

    #[test]
    fn a_default_is_used_when_there_is_no_variable() {
        let _g = expansion_env();
        let mut missing = Vec::new();
        assert_eq!(expand("${MCP_NOPE:-fallback}", &mut missing), "fallback");
        // An empty default is a default: the reference resolves to nothing
        // without being reported as unset.
        assert_eq!(expand("[${MCP_NOPE:-}]", &mut missing), "[]");
        assert!(missing.is_empty(), "{missing:?}");
        // And a variable that *is* set wins over the default.
        assert_eq!(expand("${MCP_TOKEN:-fallback}", &mut missing), "token");
    }

    #[test]
    fn a_variable_that_is_not_set_is_left_as_written_and_named() {
        let _g = expansion_env();
        let mut missing = Vec::new();
        assert_eq!(expand("${MCP_NOPE}", &mut missing), "${MCP_NOPE}");
        assert_eq!(missing, vec!["MCP_NOPE".to_string()]);
        // A name that is not a name is not a reference: it is text, and an
        // empty one is not even worth naming.
        missing.clear();
        assert_eq!(expand("${}", &mut missing), "${}");
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(expand("${a b}", &mut missing), "${a b}");
        assert_eq!(missing, vec!["a b".to_string()]);
        // An opening without a closing is text too, and what follows it is not
        // searched for another one.
        missing.clear();
        assert_eq!(
            expand("${MCP_NOPE and more", &mut missing),
            "${MCP_NOPE and more"
        );
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[test]
    fn a_reference_in_a_command_or_a_header_is_resolved_too() {
        let _g = expansion_env();
        let mut warnings = Vec::new();
        let started = read(
            "test",
            &json!({
                "command": "${MCP_TOKEN}",
                "args": ["--token=${MCP_TOKEN}"],
                "env": {"${MCP_TOKEN}": "${MCP_TOKEN}"}
            }),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(started.command.as_deref(), Some("token"));
        assert_eq!(started.args, vec!["--token=token"]);
        // The env table is keyed by the resolved names: two references to the
        // same variable are one variable.
        assert_eq!(started.env.get("token").map(String::as_str), Some("token"));
        assert!(warnings.is_empty(), "{warnings:?}");

        let called = read(
            "test",
            &json!({
                "url": "https://example.test/${MCP_NOPE}",
                "headers": {"X-Key": "${MCP_TOKEN:-none}"}
            }),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(
            called.url.as_deref(),
            Some("https://example.test/${MCP_NOPE}")
        );
        assert_eq!(
            called.headers.get("X-Key").map(String::as_str),
            Some("token")
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("MCP_NOPE"), "{warnings:?}");
    }

    /// The two files, with a HOME of the test's own. The lock is held for the
    /// whole test: the environment and the working directory are the process's.
    fn files(user: Option<Value>, project: Option<Value>) -> (Vec<Entry>, Vec<String>) {
        let home = scratch_home();
        if let Some(value) = &user {
            std::fs::write(
                crate::config::settings_file().unwrap(),
                serde_json::to_string(value).unwrap(),
            )
            .unwrap();
        }
        let workspace = workspace();
        if let Some(value) = &project {
            std::fs::write(
                workspace.join(".mcp.json"),
                serde_json::to_string(value).unwrap(),
            )
            .unwrap();
        }
        let read = entries(&workspace);
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&workspace).unwrap();
        read
    }

    #[test]
    fn the_two_files_are_merged_with_the_project_winning() {
        let _g = env_lock();
        let (found, warnings) = files(
            Some(json!({"mcpServers": {
                "shared": {"command": "user-binary"},
                "mine": {"command": "npx", "args": ["-y", "mcp-files"]}
            }})),
            Some(json!({"mcpServers": {
                "shared": {"command": "project-binary"},
                "theirs": {"url": "https://example.test/mcp"}
            }})),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        // Sorted by name, so the same configuration offers the same tool list
        // in the same order on every run.
        assert_eq!(names, vec!["mine", "shared", "theirs"]);
        let shared = found.iter().find(|e| e.name == "shared").unwrap();
        assert_eq!(shared.from, ".mcp.json");
        assert_eq!(
            shared.config.as_ref().unwrap().command.as_deref(),
            Some("project-binary")
        );
        assert_eq!(found[0].from, "settings.json");
        assert_eq!(found[2].from, ".mcp.json");
    }

    #[test]
    fn no_files_is_no_servers() {
        let _g = env_lock();
        let (found, warnings) = files(None, None);
        assert!(found.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_a_warning_rather_than_an_error() {
        let _g = env_lock();
        let home = scratch_home();
        std::fs::write(crate::config::settings_file().unwrap(), "{ not json").unwrap();
        let workspace = workspace();
        std::fs::write(workspace.join(".mcp.json"), "{ also not json").unwrap();
        let (found, warnings) = entries(&workspace);
        assert!(found.is_empty());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("settings.json")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains(".mcp.json")),
            "{warnings:?}"
        );
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&workspace).unwrap();
    }

    #[test]
    fn a_table_that_is_not_a_table_is_a_warning() {
        let _g = env_lock();
        let (found, warnings) = files(Some(json!({"mcpServers": ["npx"]})), None);
        assert!(found.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("not an object"), "{warnings:?}");
    }

    #[test]
    fn an_entry_that_cannot_be_used_is_kept_as_the_reason() {
        let _g = env_lock();
        let (found, _) = files(
            Some(json!({"mcpServers": {
                "broken": {"command": "npx", "url": "https://x.test"},
                "nameless": {}
            }})),
            None,
        );
        assert_eq!(found.len(), 2);
        for entry in found {
            let why = entry.config.as_ref().unwrap_err();
            assert!(why.contains("command"), "{why}");
        }
    }

    #[test]
    fn an_entry_with_no_name_is_skipped() {
        let _g = env_lock();
        let (found, warnings) = files(Some(json!({"mcpServers": {" ": {"command": "npx"}}})), None);
        assert!(found.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no name"), "{warnings:?}");
    }
}
