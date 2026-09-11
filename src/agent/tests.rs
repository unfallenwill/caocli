//! The interpreter's tests: the turn loop, driven against a mock backend.
//!
//! What a turn does to the log is asserted on the session (the log *is* the
//! state); what it tells the front end is asserted through a real `Renderer`,
//! because a notification that never reaches one is a notification that does not
//! exist. The pieces that are pure — the request, the reply, the race — are
//! tested next to what they belong to.

use super::*;

use crate::agents_md;
use crate::session::SessionMeta;
use crate::types::Role;
use crate::ui::Renderer;
use crate::ui::doubles::{
    Answer, CancelAt, CancelNow, Dismissed, NoAnswer, NoCancel, NoQuestions, Picked,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The command-line flag becomes a policy in one place, and the policy says
/// which way round it points (a bare `bool` field would not).
#[test]
fn the_ask_flag_becomes_the_gate_policy() {
    assert_eq!(Approval::from_flag(true), Approval::Ask);
    assert_eq!(Approval::from_flag(false), Approval::Trusted);
}

fn tmpdir() -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "caocli-agent-mock-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn test_meta() -> SessionMeta {
    SessionMeta {
        provider: Some("deepseek".into()),
        model: "deepseek-v4-flash".into(),
        reasoning_effort: Some("high".into()),
        instructions: None,
    }
}

/// Build one SSE chunk line (including the blank-line separator).
fn sse(delta: serde_json::Value, finish: Option<&str>, usage: Option<serde_json::Value>) -> String {
    let mut obj = json!({
        "id": "mock-1",
        "model": "deepseek-v4-flash",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if let Some(u) = usage {
        obj["usage"] = u;
    }
    format!("data: {obj}\n\n")
}

async fn mount_chat(server: &MockServer, body: String, times: Option<u64>) {
    let mock = Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body));
    match times {
        Some(n) => mock.up_to_n_times(n).mount(server).await,
        None => mock.mount(server).await,
    }
}

fn test_agent(server: &MockServer, dir: &std::path::Path) -> Agent {
    let api = Client::new(
        "test-key".into(),
        format!("{}/chat/completions", server.uri()),
        provider::DEEPSEEK.wire,
    )
    .unwrap();
    let session = Session::create(dir, test_meta()).unwrap();
    Agent::new(api, session, provider::DEEPSEEK)
}

#[test]
fn effort_label_reads_the_stored_tier_or_the_providers_default() {
    let dir = tmpdir();
    let s = Session::create(&dir, test_meta()).unwrap();
    let agent = Agent::new(
        Client::new(
            "k".into(),
            provider::DEEPSEEK.url.into(),
            provider::DEEPSEEK.wire,
        )
        .unwrap(),
        s,
        provider::DEEPSEEK,
    );
    assert_eq!(agent.effort_label(), "high");

    // A session that stored none is at the provider's default, which is the
    // tier the request is built with.
    let mut s = Session::create(&dir, test_meta()).unwrap();
    s.meta.reasoning_effort = None;
    let agent = Agent::new(
        Client::new(
            "k".into(),
            provider::ZAI_CODING_CN.url.into(),
            provider::ZAI_CODING_CN.wire,
        )
        .unwrap(),
        s,
        provider::ZAI_CODING_CN,
    );
    assert_eq!(agent.effort_label(), provider::ZAI_CODING_CN.default_effort);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// End to end: tool_calls (with sharded arguments) → really execute Bash →
/// second round passes reasoning_content back → final answer. This pins the
/// DeepSeek hard constraint "with tools, reasoning_content must be passed
/// back" and byte-for-byte history replay.
#[tokio::test]
async fn mock_full_tool_loop_replays_reasoning_content() {
    let server = MockServer::start().await;
    let turn1 = [
        sse(json!({"role":"assistant","reasoning_content":"I need to run a command."}), None, None),
        sse(json!({"tool_calls":[{"index":0,"id":"call_mock_1","type":"function","function":{"name":"Bash","arguments":"{\"comm"}}]}), None, None),
        sse(json!({"tool_calls":[{"index":0,"function":{"arguments":"and\":\"echo caocli-mock-marker\"}"}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), Some(json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_cache_hit_tokens":0,"prompt_cache_miss_tokens":10}))),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"Done executing."}), None, None),
        sse(json!({"content":""}), Some("stop"), Some(json!({"prompt_tokens":20,"completion_tokens":8,"total_tokens":28,"prompt_cache_hit_tokens":12,"prompt_cache_miss_tokens":8}))),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "use a tool to leave a marker",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    // usage must reach the renderer held by the caller, otherwise the status
    // bar's cache stats never update
    let cache = ui.stats();
    assert_eq!(
        (cache.hit, cache.miss),
        (12, 18),
        "hit/miss from both sub-requests should accumulate into the same renderer"
    );

    // session history: user / assistant(thinking + tool_calls) / tool(result) / assistant
    assert_eq!(agent.session.messages.len(), 4);
    let assistant1 = &agent.session.messages[1];
    assert_eq!(
        assistant1.reasoning_content.as_deref(),
        Some("I need to run a command.")
    );
    let calls = assistant1.tool_calls.as_ref().unwrap();
    assert_eq!(calls[0].id, "call_mock_1");
    assert_eq!(calls[0].function.name, "Bash");
    assert_eq!(
        calls[0].function.arguments,
        r#"{"command":"echo caocli-mock-marker"}"#
    );
    let tool_msg = &agent.session.messages[2];
    assert_eq!(tool_msg.role, Role::Tool);
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_mock_1"));
    // Bash really ran
    assert!(tool_msg.text().unwrap().contains("caocli-mock-marker"));
    assert_eq!(
        agent.session.messages[3].text().as_deref(),
        Some("Done executing.")
    );

    // second round's request body: the core hard constraint — reasoning_content
    // is passed back verbatim
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4); // system + user + assistant + tool
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["reasoning_content"], "I need to run a command.");
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_mock_1");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_mock_1");
    // request parameter shape
    assert_eq!(body["model"], "deepseek-v4-flash");
    assert_eq!(body["max_tokens"], 384_000);
    assert_eq!(body["stream"], true);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["tools"][0]["function"]["name"], "Bash");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A workspace with a subdirectory whose AGENTS.md a file-tool call can
/// discover: the workspace root, the subdirectory, and a file in it to
/// operate on.
fn nested_workspace() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let root = tmpdir();
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("AGENTS.md"), "the subdirectory's own rules").unwrap();
    let file = sub.join("notes.txt");
    std::fs::write(&file, "hello\n").unwrap();
    (root, sub, file)
}

/// First touch of a directory with instructions of its own: the instructions
/// are appended where a user message is legal — after the result window has
/// closed — and the second sub-request carries them, so the model reads them
/// in the same turn that touched the directory.
#[tokio::test]
async fn mock_first_touch_of_a_directory_appends_its_instructions() {
    let server = MockServer::start().await;
    let (root, sub, file) = nested_workspace();
    let read_args = format!(r#"{{"file_path":"{}"}}"#, file.display());
    let turn1 = [
        sse(json!({"tool_calls":[
            {"index":0,"id":"call_r1","type":"function","function":{"name":"Read","arguments": read_args }}
        ]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"Read it."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            &format!("read {}", file.display()),
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();
    // history: user / assistant(call) / tool(result) / user(inject) / assistant
    assert_eq!(
        agent.session.messages.len(),
        5,
        "the injection sits between the result and the answer"
    );
    let injected = &agent.session.messages[3];
    assert_eq!(injected.role, Role::User);
    let text = injected.text().unwrap();
    assert!(
        text.starts_with(agents_md::INJECT_LEAD),
        "the injection is in its own shape: {text}"
    );
    assert!(
        text.contains(&sub.display().to_string()),
        "it names the directory it came from: {text}"
    );
    assert!(
        text.contains("the subdirectory's own rules"),
        "it carries the content: {text}"
    );

    // the second sub-request carried the injection after the tool result
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5); // system + user + assistant + tool + inject
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[4]["role"], "user");
    assert!(
        msgs[4]["content"]
            .as_str()
            .unwrap()
            .starts_with(agents_md::INJECT_LEAD),
        "the request carries what the log carries"
    );
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

/// A directory is injected once per session, not once per call: the second
/// turn to touch it finds the log already carrying the injection.
#[tokio::test]
async fn mock_a_directory_is_injected_once_per_session() {
    let server = MockServer::start().await;
    let (root, _sub, file) = nested_workspace();
    let read_args = format!(r#"{{"file_path":"{}"}}"#, file.display());
    let read_call = |id: &str| {
        json!({"tool_calls":[
            {"index":0,"id":id,"type":"function","function":{"name":"Read","arguments": read_args.as_str() }}
        ]})
    };
    let turn1 = [
        sse(read_call("call_a1"), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"first answer"}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1.clone(), Some(1)).await;
    mount_chat(&server, turn2.clone(), Some(1)).await;
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    for prompt in ["read it once", "read it again"] {
        agent
            .turn(
                prompt,
                &mut ui,
                &mut NoCancel,
                &mut Answer::denies(),
                &mut NoQuestions,
            )
            .await
            .unwrap();
    }

    // 4 messages per plain turn, plus the one injection the first turn made
    assert_eq!(agent.session.messages.len(), 9);
    let injections = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.text()
                .is_some_and(|t| t.starts_with(agents_md::INJECT_LEAD))
        })
        .count();
    assert_eq!(injections, 1, "the second touch appended nothing");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

/// A window of several declared calls keeps its results on uninterrupted: the
/// injection is appended after the last result, where a user message is
/// legal, never between the results of one window (a shape the backend
/// refuses, and the request tripwire panics on).
#[tokio::test]
async fn mock_multi_call_window_injects_after_the_last_result() {
    let server = MockServer::start().await;
    let (root, sub, file) = nested_workspace();
    let read_args = format!(r#"{{"file_path":"{}"}}"#, file.display());
    let turn1 = [
        sse(json!({"tool_calls":[
            {"index":0,"id":"call_w1","type":"function","function":{"name":"Read","arguments": read_args }},
            {"index":1,"id":"call_w2","type":"function","function":{"name":"Read","arguments": read_args }}
        ]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"Both read."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "read it twice",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    // user / assistant(2 calls) / tool / tool / user(inject) / assistant — and
    // not user / tool / user(inject) / tool, which would be an invalid history
    assert_eq!(agent.session.messages.len(), 6);
    assert_eq!(agent.session.messages[2].role, Role::Tool);
    assert_eq!(agent.session.messages[3].role, Role::Tool);
    assert_eq!(agent.session.messages[4].role, Role::User);
    assert!(
        agent.session.messages[4]
            .text()
            .unwrap()
            .starts_with(agents_md::INJECT_LEAD)
    );
    assert_eq!(
        agents_md::sent_dirs(&agent.session.messages),
        [sub].into_iter().collect(),
        "one directory, sent once"
    );
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

/// A Bash call names no file in any argument the interpreter can read, so it
/// discovers nothing: instructions are a file-tool discovery here.
#[tokio::test]
async fn mock_a_bash_call_discovers_no_instructions() {
    let server = MockServer::start().await;
    let (root, _sub, file) = nested_workspace();
    let cat = format!(r#"{{"command":"cat {}"}}"#, file.display());
    let turn1 = [
        sse(json!({"tool_calls":[
            {"index":0,"id":"call_b1","type":"function","function":{"name":"Bash","arguments": cat }}
        ]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"Cat it."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "cat the file",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    // user / assistant(call) / tool(result) / assistant — and no injection
    assert_eq!(agent.session.messages.len(), 4);
    assert!(!agent.session.messages.iter().any(|m| {
        m.text()
            .is_some_and(|t| t.starts_with(agents_md::INJECT_LEAD))
    }));
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

/// End to end on the Anthropic wire: a MiniMax-M3 turn with a signed
/// thinking block and a sharded tool call runs the same loop, and the second
/// sub-request replays the thinking block verbatim — signature included — as
/// a tool_use/tool_result block conversation.
#[tokio::test]
async fn mock_anthropic_loop_replays_thinking_blocks_verbatim() {
    let server = MockServer::start().await;
    let ev = |name: &str, payload: serde_json::Value| format!("event: {name}\ndata: {payload}\n\n");
    let turn1 = [
        ev("message_start", json!({"type":"message_start","message":{"id":"m1","model":"MiniMax-M3","usage":{"input_tokens":100,"cache_read_input_tokens":80,"cache_creation_input_tokens":20,"output_tokens":1}}})),
        ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to run "}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"a command."}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-mock-1"}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ev("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_mock_1","name":"Bash","input":{}}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"echo caocli-mock-marker\"}"}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":1})),
        ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}})),
        ev("message_stop", json!({"type":"message_stop"})),
    ]
    .concat();
    let turn2 = [
        ev("message_start", json!({"type":"message_start","message":{"id":"m2","model":"MiniMax-M3","usage":{"input_tokens":50,"cache_read_input_tokens":30,"cache_creation_input_tokens":0,"output_tokens":1}}})),
        ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done executing."}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}})),
        ev("message_stop", json!({"type":"message_stop"})),
    ]
    .concat();
    let server = &server;
    let mount = |body: String, times: Option<u64>| async move {
        let mock = Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body));
        match times {
            Some(n) => mock.up_to_n_times(n).mount(server).await,
            None => mock.mount(server).await,
        }
    };
    mount(turn1, Some(1)).await;
    mount(turn2, None).await;

    let dir = tmpdir();
    let api = Client::new(
        "test-key".into(),
        format!("{}/anthropic/v1/messages", server.uri()),
        provider::MINIMAX.wire,
    )
    .unwrap();
    let session = Session::create(
        &dir,
        SessionMeta {
            provider: Some("minimax".into()),
            model: "MiniMax-M3".into(),
            reasoning_effort: Some("on".into()),
        },
    )
    .unwrap();
    let mut agent = Agent::new(api, session, provider::MINIMAX);
    let mut ui = Renderer::new();
    agent
        .turn(
            "use a tool to leave a marker",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();
    // cache stats from both sub-requests accumulate: turn 1 hit 80 / miss
    // 120, turn 2 hit 30 / miss 50.
    let cache = ui.stats();
    assert_eq!((cache.hit, cache.miss), (110, 170));

    // session history: user / assistant(thinking + tool_calls) / tool / assistant
    assert_eq!(agent.session.messages.len(), 4);
    let assistant1 = &agent.session.messages[1];
    assert_eq!(assistant1.text().as_deref(), Some(""));
    assert_eq!(
        assistant1.thinking,
        Some(vec![crate::types::ThinkingBlock {
            thinking: "I need to run a command.".into(),
            signature: "sig-mock-1".into(),
        }])
    );
    let calls = assistant1.tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), 1, "block indexes 0/1 are not tool ordinals");
    assert_eq!(calls[0].id, "call_mock_1");
    assert_eq!(
        calls[0].function.arguments,
        r#"{"command":"echo caocli-mock-marker"}"#
    );
    // Bash really ran
    assert!(
        agent.session.messages[2]
            .text()
            .unwrap()
            .contains("caocli-mock-marker")
    );

    // second round's request body: the Anthropic shape, thinking replayed
    // verbatim, the result a tool_result block in the user turn.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(body["model"], "MiniMax-M3");
    assert_eq!(body["max_tokens"], 131_072);
    assert_eq!(body["stream"], true);
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(body["tool_choice"], json!({"type": "auto"}));
    assert_eq!(body["tools"][0]["name"], "Bash");
    assert!(
        body["system"]
            .as_str()
            .unwrap()
            .starts_with("You are caocli")
    );
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3); // user / assistant(blocks) / user(tool_result)
    let blocks = msgs[1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["signature"], "sig-mock-1");
    assert_eq!(blocks[0]["thinking"], "I need to run a command.");
    assert_eq!(blocks[1]["type"], "tool_use");
    assert_eq!(blocks[1]["id"], "call_mock_1");
    assert_eq!(blocks[1]["input"]["command"], "echo caocli-mock-marker");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_mock_1");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// End to end: two tool calls declared at once. The interpreter must finish
/// executing both, in declaration order, before sending the second
/// sub-request — sending after only one would leave the history missing a
/// tool result (API 400).
#[tokio::test]
async fn mock_multi_call_loop_executes_all_before_next_request() {
    let server = MockServer::start().await;
    let turn1 = [
        sse(json!({"tool_calls":[
            {"index":0,"id":"call_m1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo one\"}"}},
            {"index":1,"id":"call_m2","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo two\"}"}}
        ]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"Both executed."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "run two commands",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    let msgs = &agent.session.messages;
    assert_eq!(
        msgs.len(),
        5,
        "user / assistant(2 calls) / tool×2 / assistant"
    );
    assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_m1"));
    assert_eq!(msgs[3].tool_call_id.as_deref(), Some("call_m2"));
    assert!(msgs[2].text().as_deref().unwrap().contains("one"));
    assert!(msgs[3].text().as_deref().unwrap().contains("two"));

    // In the second sub-request's history both results must already be present
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let results: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["tool_call_id"].as_str())
        .collect();
    assert_eq!(
        results,
        vec!["call_m1", "call_m2"],
        "the second request must carry every tool result"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Ctrl-C arrives during the first stream: the assistant is not committed, so
/// the history stops at the user message.
/// Cancellation = a live interruption in its lightest form: no marker is
/// needed because the history was already valid.
#[tokio::test]
async fn cancel_during_first_stream_keeps_history_at_user() {
    let server = MockServer::start().await; // no mock mounted: pump stays pending
    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    // the first cancellation point (the pump select) fires immediately
    let mut cancel = CancelNow;
    agent
        .turn(
            "original instruction",
            &mut ui,
            &mut cancel,
            &mut Answer::allows(),
            &mut NoQuestions,
        )
        .await
        .unwrap();
    assert_eq!(agent.session.messages.len(), 1);
    assert_eq!(agent.session.messages[0].role, Role::User);
    assert!(machine::is_request_valid(&agent.session.messages));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Ctrl-C arrives while a tool is executing: the interrupted call and every
/// call after it that never ran are persisted with the cancellation marker,
/// closing the window and keeping the history valid — the next turn does not
/// resurrect zombie calls.
#[tokio::test]
async fn cancel_during_tool_marks_remaining_calls_cancelled() {
    let server = MockServer::start().await;
    let turn1 = [
        sse(json!({"tool_calls":[
            {"index":0,"id":"call_c1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"sleep 30\"}"}},
            {"index":1,"id":"call_c2","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo two\"}"}}
        ]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    // the stream is left to finish; the second wait point is the tool call
    let mut cancel = CancelAt::on(&[2]);
    agent
        .turn(
            "run two slow commands",
            &mut ui,
            &mut cancel,
            &mut Answer::allows(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    let msgs = &agent.session.messages;
    assert_eq!(
        msgs.len(),
        4,
        "user / assistant(2 calls) / cancellation marker ×2"
    );
    assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_c1"));
    assert_eq!(msgs[3].tool_call_id.as_deref(), Some("call_c2"));
    assert_eq!(
        msgs[2].text().as_deref(),
        Some(machine::Marker::Cancelled.text()),
        "a call interrupted while executing is marked cancelled too"
    );
    assert_eq!(
        msgs[3].text().as_deref(),
        Some(machine::Marker::Cancelled.text())
    );
    // the window is closed: the history is valid, so the next turn's decision
    // is to send a request rather than resurrect zombie calls
    assert!(machine::is_request_valid(msgs));
    assert_eq!(machine::next_action(msgs), Some(machine::Action::CallModel));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A result that reads like the cancellation marker is still a result: a file
/// whose text is that marker must not end the turn. Cancellation is decided by
/// which arm of the race won, not by comparing what a tool returned.
#[tokio::test]
async fn a_tool_result_is_not_mistaken_for_a_cancellation() {
    let dir = tmpdir();
    let marker_file = dir.join("marker.txt");
    let marker = machine::Marker::Cancelled.text();
    // The marker text as the file's one line, newline-terminated so that Read's
    // row numbering is the only thing in front of it: the result reads like the
    // marker and is still a result.
    std::fs::write(&marker_file, format!("{marker}\n")).unwrap();

    let server = MockServer::start().await;
    let read_args = json!({"file_path": marker_file}).to_string();
    let turn1 = [
        sse(
            json!({"tool_calls": [{"index": 0, "id": "call_read", "type": "function",
                   "function": {"name": "Read", "arguments": read_args}}]}),
            None,
            None,
        ),
        sse(json!({"content": ""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(
            json!({"content": "the file holds the marker text"}),
            None,
            None,
        ),
        sse(json!({"content": ""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "read that file",
            &mut ui,
            &mut NoCancel,
            &mut Answer::allows(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    let msgs = &agent.session.messages;
    assert_eq!(
        msgs[2].text().as_deref(),
        Some(format!("1\t{marker}").as_str()),
        "the tool really returned the marker text"
    );
    assert_eq!(
        msgs.len(),
        4,
        "the turn went back to the model instead of ending as cancelled: {msgs:?}"
    );
    assert_eq!(
        msgs[3].text().as_deref(),
        Some("the file holds the marker text")
    );
    assert!(machine::is_request_valid(msgs));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The step budget is the turn's, not the process's: a session that has spent
/// it once may spend it again on the next question. A cumulative counter would
/// stop the second turn before its first call.
#[tokio::test]
async fn the_step_budget_is_spent_again_by_the_next_turn() {
    let server = MockServer::start().await;
    let call = |word: &str| {
        json!({"tool_calls": [{"index": 0, "id": format!("call_{word}"), "type": "function",
               "function": {"name": "Bash", "arguments": format!("{{\"command\":\"echo {word}\"}}")}}]})
    };
    let tool_turn = |word: &str| {
        [
            sse(call(word), None, None),
            sse(json!({"content": ""}), Some("tool_calls"), None),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat()
    };
    let answer = |text: &str| {
        [
            sse(json!({"content": text}), None, None),
            sse(json!({"content": ""}), Some("stop"), None),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat()
    };
    // Each turn costs two sub-requests: the call, then the answer.
    mount_chat(&server, tool_turn("budget-one"), Some(1)).await;
    mount_chat(&server, answer("first done"), Some(1)).await;
    mount_chat(&server, tool_turn("budget-two"), Some(1)).await;
    mount_chat(&server, answer("second done"), None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    agent.max_tool_steps = 1;
    let mut ui = Renderer::new();
    for question in ["run one command", "run another"] {
        agent
            .turn(
                question,
                &mut ui,
                &mut NoCancel,
                &mut Answer::allows(),
                &mut NoQuestions,
            )
            .await
            .unwrap();
    }

    let msgs = &agent.session.messages;
    let results: Vec<String> = msgs
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.text())
        .collect();
    assert_eq!(results.len(), 2, "each turn ran exactly one call");
    assert!(
        results.iter().all(|r| !r.contains("step limit")),
        "the second turn must get a budget of its own: {results:?}"
    );
    assert!(results[0].contains("budget-one") && results[1].contains("budget-two"));
    assert_eq!(
        msgs.last().and_then(|m| m.text()).as_deref(),
        Some("second done"),
        "the second turn ran to the model's answer"
    );
    assert!(machine::is_request_valid(msgs));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Approval denied: the denial marker closes that call, the window is valid, and
/// the next beat keeps going.
#[tokio::test]
async fn approval_denied_commits_denial_marker() {
    let server = MockServer::start().await;
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_d1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo blocked\"}"}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    let turn2 = [
        sse(json!({"content":"Skipped that command."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn2, None).await;
    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    agent.approval = Approval::Ask;
    let mut ui = Renderer::new();
    agent
        .turn(
            "run the forbidden command",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();
    let msgs = &agent.session.messages;
    assert_eq!(
        msgs.len(),
        4,
        "user / assistant / denial marker / closing assistant"
    );
    assert_eq!(
        msgs[2].text().as_deref(),
        Some(machine::Marker::Denied.text())
    );
    assert!(machine::is_request_valid(msgs));
    // the window is closed and the denial went back to the model: the closing
    // assistant proves the model digested the denial
    assert_eq!(msgs[3].role, Role::Assistant);
    assert!(msgs[3].text().unwrap_or_default().contains("Skipped"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Ctrl-C during the approval wait: unanswered calls are persisted with the
/// cancellation marker (distinct from the denial marker).
#[tokio::test]
async fn cancel_during_approval_wait_commits_cancelled() {
    let server = MockServer::start().await;
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_w1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo hi\"}"}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    agent.approval = Approval::Ask;
    let mut ui = Renderer::new();
    // the stream is left to finish; the second wait point is the tool call
    let mut cancel = CancelAt::on(&[2]);
    agent
        .turn(
            "run it",
            &mut ui,
            &mut cancel,
            &mut NoAnswer,
            &mut NoQuestions,
        )
        .await
        .unwrap();
    let msgs = &agent.session.messages;
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        msgs[2].text().as_deref(),
        Some(machine::Marker::Cancelled.text())
    );
    assert!(machine::is_request_valid(msgs));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Step cap: once the cap is reached no new tool runs, and the remaining calls
/// are closed with the cap marker.
#[tokio::test]
async fn step_limit_aborts_with_deterministic_markers() {
    let server = MockServer::start().await;
    let calls = ["one", "two", "three"]
        .iter()
        .enumerate()
        .map(|(i, word)| {
            json!({"index": i, "id": format!("call_s{}", i + 1), "type": "function",
                   "function": {"name": "Bash", "arguments": format!("{{\"command\":\"echo {word}\"}}")}})
        })
        .collect::<Vec<_>>();
    let turn1 = [
        sse(json!({"tool_calls": calls}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    agent.max_tool_steps = 2;
    let mut ui = Renderer::new();
    agent
        .turn(
            "run three commands",
            &mut ui,
            &mut NoCancel,
            &mut Answer::allows(),
            &mut NoQuestions,
        )
        .await
        .unwrap();
    let msgs = &agent.session.messages;
    assert_eq!(msgs.len(), 5, "user / assistant(3 calls) / tool×3");
    assert_eq!(
        msgs[4].text().as_deref(),
        Some(machine::Marker::StepLimit.text()),
        "the third call gets the marker instead of executing because the cap is reached"
    );
    assert!(machine::is_request_valid(msgs));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// End to end: Write tool dispatch — the model calls Write, the file really is
/// created, and the second request carries all four tool definitions.
#[tokio::test]
async fn mock_write_tool_creates_file() {
    let server = MockServer::start().await;
    let file_dir = tmpdir();
    let target = file_dir.join("written.txt");
    let args =
        json!({"file_path": target.to_string_lossy(), "content": "written by mock"}).to_string();
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_w1","type":"function","function":{"name":"Write","arguments":args}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"File created."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "write a file",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "written by mock");
    let tool_msg = &agent.session.messages[2];
    assert!(tool_msg.text().as_deref().unwrap().starts_with("ok:"));

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    let names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "Bash",
            "Read",
            "Edit",
            "Write",
            "AskUserQuestion",
            "TodoWrite"
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&file_dir).unwrap();
}

/// A front end that keeps what it was told, in the order it was told it.
///
/// A running command's output is the one notification with no other way to see it:
/// it is not a value the machine gets back, and it is deliberately not part of the
/// log -- the log keeps the result -- so what proves it arrived is the front end it
/// arrived at. `Watching` implements the notification channel alone: the turn asks
/// for nothing back.
#[derive(Default)]
struct Hearing(Vec<String>);

impl crate::ui::Ui for Hearing {
    fn reasoning_delta(&mut self, _s: &str) {}
    fn content_delta(&mut self, _s: &str) {}
    fn finish_turn(&mut self) {}
    fn tool_start(&mut self, name: &str, _args: &str) {
        self.0.push(format!("call:{name}"));
    }
    fn tool_output(&mut self, chunk: &str) {
        self.0.push(format!("out:{}", chunk.trim_end()));
    }
    fn tool_result(&mut self, result: &str) {
        self.0
            .push(format!("result:{}", result.lines().next().unwrap_or("")));
    }
    fn instructions(&mut self, dir: &str) {
        self.0.push(format!("instructions:{dir}"));
    }
    fn usage(&mut self, _usage: &crate::types::Usage, _stream: std::time::Duration) {}
    fn interrupted(&mut self) {}
    fn approval_requested(&mut self, _name: &str, _args: &str) {}
}

/// End to end: what a command prints while it runs reaches the front end before the
/// result does, and the log keeps the result rather than the stream.
#[tokio::test]
async fn mock_a_running_commands_output_reaches_the_front_end() {
    let server = MockServer::start().await;
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_live_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo caocli-live-marker\"}"}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"Saw it."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Hearing::default();
    agent
        .turn(
            "run it",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();

    assert_eq!(
        ui.0,
        vec!["call:Bash", "out:caocli-live-marker", "result:exit_code: 0"],
        "the stream arrives while the call is still running"
    );
    // What the log holds is the result, which carries the same marker: the stream is
    // a view of the call and never a second record of it.
    let tool_msg = &agent.session.messages[2];
    let text = tool_msg.text().unwrap();
    assert!(text.starts_with("exit_code: 0"), "{text}");
    assert!(text.contains("caocli-live-marker"), "{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An HTTP error must carry the status code and the response body, and must
/// not damage the already-persisted session.
#[tokio::test]
async fn mock_http_error_includes_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"reasoning_content must be passed back","type":"invalid_request_error"}}"#,
        ))
        .mount(&server)
        .await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    let err = agent
        .turn(
            "hi",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(
        s.contains("400"),
        "the error should include the status code: {s}"
    );
    assert!(
        s.contains("reasoning_content must be passed back"),
        "the error should include the response body: {s}"
    );
    // the user message is persisted, the assistant one is not
    assert_eq!(agent.session.messages.len(), 1);
    assert_eq!(agent.session.messages[0].role, Role::User);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A broken SSE chunk errors out with context.
#[tokio::test]
async fn mock_broken_sse_chunk_fails_with_context() {
    let server = MockServer::start().await;
    mount_chat(&server, "data: {\"broken\":\n\n".to_string(), None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    let err = agent
        .turn(
            "hi",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("failed to parse SSE chunk"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The question tool, end to end: the model asks, the front end answers, and what
/// the user chose is what the call's result says -- in the log the next request is
/// built from, and on the screen.
#[tokio::test]
async fn mock_question_tool_records_the_users_answer() {
    let server = MockServer::start().await;
    let args = json!({"questions": [
        {"id": "auth", "header": "Auth", "question": "Which auth?",
         "options": [{"label": "JWT (Recommended)"}, {"label": "Session cookie"}]},
        {"id": "store", "question": "Where?", "multi_select": true,
         "options": [{"label": "Postgres"}, {"label": "SQLite"}]}
    ]})
    .to_string();
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_q1","type":"function","function":{"name":"AskUserQuestion","arguments":args}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let turn2 = [
        sse(json!({"content":"Both settled."}), None, None),
        sse(json!({"content":""}), Some("stop"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, Some(1)).await;
    mount_chat(&server, turn2, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    let mut answers = Picked::sets(&[&["JWT (Recommended)"], &["Postgres", "SQLite"]]);
    agent
        .turn(
            "make it work",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut answers,
        )
        .await
        .unwrap();

    let msgs = &agent.session.messages;
    assert_eq!(
        msgs[2].text().as_deref(),
        Some("auth: JWT (Recommended)\nstore: Postgres, SQLite"),
        "the answer is the call's result, id by id"
    );
    assert!(machine::is_request_valid(msgs));
    // The answer is what the next request carries, byte for byte.
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    // The request carries the system prompt first, so the tool result is the
    // fourth message in it.
    assert_eq!(
        body["messages"][3]["content"],
        json!("auth: JWT (Recommended)\nstore: Postgres, SQLite")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A question the user dismissed is answered with a marker, not with the option
/// the cursor happened to be on: the model can ask again, or go on without it.
#[tokio::test]
async fn mock_a_dismissed_question_is_a_marker() {
    let server = MockServer::start().await;
    let args = json!({"questions": [{"id": "auth", "question": "Which auth?"}]}).to_string();
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_q1","type":"function","function":{"name":"AskUserQuestion","arguments":args}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "make it work",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut Dismissed,
        )
        .await
        .unwrap();
    assert_eq!(
        agent.session.messages[2].text().as_deref(),
        Some(machine::Marker::Unanswered.text())
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Arguments the tool cannot read are text for the model to correct itself from --
/// and nobody is asked anything: a question that cannot be read is one nobody can
/// answer.
#[tokio::test]
async fn mock_unreadable_question_arguments_never_reach_the_user() {
    let server = MockServer::start().await;
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_q1","type":"function","function":{"name":"AskUserQuestion","arguments":"{\"questions\":\"auth\"}"}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    agent
        .turn(
            "make it work",
            &mut ui,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();
    let result = agent.session.messages[2].text().unwrap().to_owned();
    assert!(
        result.starts_with("error: missing required argument questions (array)"),
        "was {result:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A cancel with a question open: the call is closed with the cancellation marker
/// and the turn ends, so the history is still a valid request prefix.
#[tokio::test]
async fn mock_cancelling_while_a_question_is_open_closes_it() {
    let server = MockServer::start().await;
    let args = json!({"questions": [{"id": "auth", "question": "Which auth?"}]}).to_string();
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_q1","type":"function","function":{"name":"AskUserQuestion","arguments":args}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    let mut ui = Renderer::new();
    // The turn's first wait is the sub-request; the second is the question.
    let mut cancel = CancelAt::on(&[2]);
    agent
        .turn(
            "make it work",
            &mut ui,
            &mut cancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
        .await
        .unwrap();
    assert_eq!(
        agent.session.messages[2].text().as_deref(),
        Some(machine::Marker::Cancelled.text())
    );
    assert!(machine::is_request_valid(&agent.session.messages));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Asking is not something the approval gate is asked about: with `--ask` on, a
/// question still reaches the user, because asking permission to ask would be
/// asking twice about one thing.
#[tokio::test]
async fn the_gate_never_stands_in_front_of_a_question() {
    let server = MockServer::start().await;
    let args = json!({"questions": [{"id": "auth", "question": "Which auth?"}]}).to_string();
    let turn1 = [
        sse(json!({"tool_calls":[{"index":0,"id":"call_q1","type":"function","function":{"name":"AskUserQuestion","arguments":args}}]}), None, None),
        sse(json!({"content":""}), Some("tool_calls"), None),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    mount_chat(&server, turn1, None).await;

    let dir = tmpdir();
    let mut agent = test_agent(&server, &dir);
    agent.approval = Approval::Ask;
    let mut ui = Renderer::new();
    agent
        .turn(
            "make it work",
            &mut ui,
            &mut NoCancel,
            // A gate that would deny anything it was asked about.
            &mut Answer::denies(),
            &mut Picked::labels(&["JWT"]),
        )
        .await
        .unwrap();
    assert_eq!(
        agent.session.messages[2].text().as_deref(),
        Some("auth: JWT"),
        "the answer is the user's, not a denial"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
