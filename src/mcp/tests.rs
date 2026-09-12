//! The hub's own tests: what the tools are called, where a call goes, and what
//! is said about a server that did not come up.
//!
//! The servers are as real as they get without a network: the stub scripts the
//! other tests use, started through the same path the session uses — entries
//! read as a configuration reads them, opened as `Hub::connect` opens them, and
//! assembled into a hub the same way.

use super::config::Entry;
use super::stub::Stub;
use super::*;

/// A hub over the entries, with every server opened the way the session opens
/// one. An entry that cannot be opened becomes a server that did not come up,
/// which is exactly what it is.
async fn hub_of(entries: Vec<Entry>) -> Hub {
    Hub::of_entries(entries).await
}

/// One entry, named as the test needs it called.
fn named(stub: &Stub, name: &str, vars: &[(&str, &str)]) -> Entry {
    let mut entry = stub.entry(vars);
    entry.name = name.to_string();
    entry
}

/// The tools a hub offers, by name.
fn names(hub: &Hub) -> Vec<String> {
    hub.definitions()
        .iter()
        .map(|tool| tool.function.name.clone())
        .collect()
}

#[test]
fn a_tool_is_named_by_the_server_it_comes_from() {
    assert_eq!(
        tool_name("filesystem", "read_file"),
        "mcp__filesystem__read_file"
    );
    assert!(is_tool("mcp__anything"));
    assert!(!is_tool("Bash"));
    assert!(!is_tool("Mcp__caseless"));
}

#[test]
fn a_name_a_backend_cannot_read_is_made_readable() {
    // What a backend takes: letters, digits, underscores, dashes. Anything else
    // — a space, a dot, a character from another alphabet — is one to a tool
    // name, and one that says where it came from rather than being dropped.
    assert_eq!(
        tool_name("my server", "read.file"),
        "mcp__my_server__read_file"
    );
    // A name from another alphabet becomes underscores — where the tool came
    // from, without pretending to spell it — and is a name all the same.
    let cjk = tool_name("文件", "读");
    assert!(cjk.starts_with("mcp__"), "{cjk}");
    assert!(
        cjk.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "{cjk}"
    );
    assert_eq!(
        tool_name("github", "create-pull-request"),
        "mcp__github__create-pull-request"
    );
}

#[test]
fn a_name_too_long_for_a_backend_keeps_its_front_and_a_hash_of_the_whole() {
    let tool = "a_very_long_tool_name_that_no_backend_would_take_because_it_is_simply_too_long";
    let name = tool_name("server", tool);
    assert!(name.len() <= MAX_NAME, "{name} is {} bytes", name.len());
    assert!(name.starts_with("mcp__server__a_very_long"), "{name}");
    assert!(name.contains('~'), "{name}");
    // The same input is the same name: a session log keeps the name, and a
    // later process has to work out the same one or fail to find the tool.
    assert_eq!(name, tool_name("server", tool));
    // And two names that agree for the whole of the front are still two names.
    let other = tool_name(
        "server",
        "a_very_long_tool_name_that_no_backend_would_take_because_it_is_simply_too_lonG",
    );
    assert_ne!(name, other);
}

#[tokio::test]
async fn an_empty_hub_offers_nothing_and_says_where_servers_come_from() {
    let hub = Hub::empty();
    assert!(hub.definitions().is_empty());
    assert!(hub.notes().is_empty());
    assert_eq!(hub.report().len(), 1);
    assert!(hub.report()[0].contains("mcpServers"), "{:?}", hub.report());
    let refused = hub.call("mcp__nobody__nothing", "{}").await;
    assert!(refused.starts_with("error: "), "{refused}");
    assert!(refused.contains("none came up"), "{refused}");
    hub.shutdown().await;
}

#[tokio::test]
async fn a_call_goes_to_the_server_that_offers_the_tool() {
    let first = Stub::new();
    let second = Stub::new();
    let hub = hub_of(vec![
        named(&first, "alpha", &[("STUB_TOOLS", "echo")]),
        named(&second, "beta", &[("STUB_TOOLS", "fail")]),
    ])
    .await;
    assert_eq!(
        names(&hub),
        vec!["mcp__alpha__echo", "mcp__beta__fail"],
        "in configuration order, which is sorted by name"
    );
    let echoed = hub.call("mcp__alpha__echo", r#"{"text":"hi"}"#).await;
    assert_eq!(echoed, "called with {text:hi}");
    let failed = hub.call("mcp__beta__fail", "{}").await;
    assert_eq!(failed, "error: the stub could not do it");
    // Each call reached the server it named and no other.
    assert!(first.received().iter().any(|line| line.contains("hi")));
    assert!(!second.received().iter().any(|line| line.contains("hi")));
    assert!(
        second
            .received()
            .iter()
            .any(|line| line.contains("tools/call"))
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_server_that_did_not_come_up_costs_nothing_else() {
    let stub = Stub::new();
    let mut broken = Stub::new().entry(&[]);
    broken.name = "missing".into();
    let config = broken.config.as_mut().unwrap();
    config.command = Some("caocli-no-such-binary".into());
    let hub = hub_of(vec![
        named(&stub, "alpha", &[("STUB_TOOLS", "echo")]),
        broken,
    ])
    .await;
    assert_eq!(names(&hub), vec!["mcp__alpha__echo"]);
    let notes = hub.notes();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes[0].contains("alpha"), "{notes:?}");
    assert!(notes[1].contains("missing"), "{notes:?}");
    assert!(notes[1].contains("failed to start"), "{notes:?}");
    let report = hub.report();
    assert!(report[0].contains("protocol"), "{report:?}");
    assert!(report[1].contains("alpha"), "{report:?}");
    assert!(report[2].contains("did not start"), "{report:?}");
    hub.shutdown().await;
}

#[tokio::test]
async fn two_servers_whose_names_become_one_are_told_apart_by_what_is_said() {
    let first = Stub::new();
    let second = Stub::new();
    // "a b" and "a_b" are two names to a configuration and one to a backend, so
    // the tools of the second server would carry the names of the first's.
    let hub = hub_of(vec![
        named(&first, "a b", &[("STUB_TOOLS", "echo")]),
        named(&second, "a_b", &[("STUB_TOOLS", "echo")]),
    ])
    .await;
    assert_eq!(names(&hub), vec!["mcp__a_b__echo"]);
    assert_eq!(hub.warnings.len(), 1, "{:?}", hub.warnings);
    assert!(hub.warnings[0].contains("two tools"), "{:?}", hub.warnings);
    // The one that is offered is the first server's: it reached that server and
    // not the other.
    hub.call("mcp__a_b__echo", r#"{"text":"one"}"#).await;
    assert!(first.received().iter().any(|line| line.contains("one")));
    assert!(!second.received().iter().any(|line| line.contains("one")));
    hub.shutdown().await;
}

#[tokio::test]
async fn a_call_to_a_dead_server_is_a_failure_and_not_a_hang() {
    let stub = Stub::new();
    let hub = hub_of(vec![named(&stub, "alpha", &[("STUB_TOOLS", "die")])]).await;
    let refused = hub.call("mcp__alpha__die", "{}").await;
    assert!(refused.starts_with("error: "), "{refused}");
    assert!(refused.contains("could not run die"), "{refused}");
    assert!(refused.contains("closed its output"), "{refused}");
    // The server is still listed, and now says why it cannot be used.
    let again = hub.call("mcp__alpha__die", "{}").await;
    assert!(again.contains("closed its output"), "{again}");
}

#[tokio::test]
async fn a_name_nobody_offers_is_answered_with_what_is_offered() {
    let stub = Stub::new();
    let hub = hub_of(vec![named(
        &stub,
        "alpha",
        &[("STUB_TOOLS", "a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p")],
    )])
    .await;
    let refused = hub.call("mcp__alpha__nowhere", "{}").await;
    assert!(refused.starts_with("error: "), "{refused}");
    assert!(refused.contains("mcp__alpha__a"), "{refused}");
    // A long list is cut: a failure is a sentence, and the whole tool list is
    // in the request the model already has.
    assert!(refused.contains("and 4 more"), "{refused}");
    // A name from a server that is not there at all is answered the same way.
    let nowhere = hub.call("mcp__nowhere__nothing", "{}").await;
    assert!(nowhere.contains("no MCP tool named"), "{nowhere}");
    hub.shutdown().await;
}

#[tokio::test]
async fn arguments_that_are_not_an_object_are_answered_rather_than_sent() {
    let stub = Stub::new();
    let hub = hub_of(vec![named(&stub, "alpha", &[("STUB_TOOLS", "echo")])]).await;
    let not_json = hub.call("mcp__alpha__echo", "{oops").await;
    assert!(not_json.contains("not valid JSON"), "{not_json}");
    let not_an_object = hub.call("mcp__alpha__echo", "[1,2]").await;
    assert!(
        not_an_object.contains("have to be a JSON object"),
        "{not_an_object}"
    );
    // Neither of them was sent anywhere.
    assert!(
        !stub
            .received()
            .iter()
            .any(|line| line.contains("tools/call"))
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn the_report_names_the_tools_the_model_sees() {
    let stub = Stub::new();
    let hub = hub_of(vec![named(&stub, "alpha", &[("STUB_TOOLS", "echo,fail")])]).await;
    let report = hub.report();
    assert_eq!(report.len(), 2, "{report:?}");
    assert!(report[1].contains("mcp__alpha__echo"), "{report:?}");
    assert!(report[1].contains("mcp__alpha__fail"), "{report:?}");
    // The names of another server's tools are not under this one's heading.
    let other = Stub::new();
    let hub = hub_of(vec![
        named(&stub, "alpha", &[("STUB_TOOLS", "echo")]),
        named(&other, "beta", &[("STUB_TOOLS", "fail")]),
    ])
    .await;
    let report = hub.report();
    assert!(report[1].contains("mcp__alpha__echo"), "{report:?}");
    assert!(!report[1].contains("mcp__beta__fail"), "{report:?}");
    assert!(report[3].contains("mcp__beta__fail"), "{report:?}");
    hub.shutdown().await;
}

#[tokio::test]
async fn shutting_the_hub_down_ends_every_server() {
    let first = Stub::new();
    let second = Stub::new();
    let hub = hub_of(vec![
        named(&first, "alpha", &[]),
        named(&second, "beta", &[]),
    ])
    .await;
    hub.shutdown().await;
    for stub in [&first, &second] {
        assert_eq!(stub.received().last().map(String::as_str), Some("eof"));
    }
}

#[tokio::test]
async fn the_notes_say_what_is_wrong_with_an_entry_nobody_could_use() {
    // A configuration warning is carried through: it is the same "what happened
    // on the way in" the report is about.
    let hub = Hub::assemble(Vec::new(), Vec::new(), vec!["${TOKEN} is not set".into()]);
    assert_eq!(hub.notes().len(), 1, "{:?}", hub.notes());
    assert!(hub.notes()[0].contains("TOKEN"), "{:?}", hub.notes());
    assert!(hub.report()[0].contains("warning"), "{:?}", hub.report());
}

/// The one test here that reads configuration files, and so the one that needs
/// a HOME of its own: the guard is held across the awaits on purpose, because
/// HOME has to stay put for as long as the hub is reading it.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn the_hub_connects_what_the_files_name() {
    use crate::config::{env_lock, scratch_home};
    let _guard = env_lock();
    let home = scratch_home();
    let stub = Stub::new();
    let workspace = std::env::temp_dir().join(format!("caocli-mcp-hub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join(".mcp.json"),
        serde_json::to_string(&serde_json::json!({
            "mcpServers": {"alpha": {
                "command": "bash",
                "args": [stub.script().to_string_lossy()],
                "env": {"STUB_TOOLS": "echo,big"},
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let hub = Hub::connect(&workspace).await;
    assert_eq!(names(&hub), vec!["mcp__alpha__echo", "mcp__alpha__big"]);
    assert_eq!(
        hub.call("mcp__alpha__echo", r#"{"text":"from the file"}"#)
            .await,
        "called with {text:from the file}"
    );
    hub.shutdown().await;
    std::fs::remove_dir_all(&home).unwrap();
    std::fs::remove_dir_all(&workspace).unwrap();
}
