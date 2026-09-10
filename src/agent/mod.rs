//! The interpreter: it asks the machine what the next beat is, executes it, and
//! writes the result back into the log.
//!
//! The machine decides and never executes (`machine::next_action`), so this is
//! the only place in the crate that turns a decision into IO. It owns the
//! session — the log is the state — and the two things a turn needs from
//! whoever is watching: an answer for the approval gate and a signal to stop.

use anyhow::Result;

use std::time::{Duration, Instant};

use crate::api::Client;
use crate::machine::{self, Action};
use crate::provider;
use crate::session::Session;
use crate::tools;
use crate::types::{ChatRequest, Message, TurnAccumulator, Usage};
use crate::ui::Ui;
use crate::ui::{Approve, Interrupt};

mod request;

pub use request::build_request;

pub struct Agent {
    api: Client,
    pub session: Session,
    /// The provider the client is bound to. The endpoint, the key and the
    /// answer ceiling all come from it, and the model is named by it: the
    /// status line reads `<provider>/<modelid>` from here.
    provider: provider::Provider,
    /// Ceiling on one answer, in tokens: the provider preset's value, sent as
    /// `max_tokens` on every request.
    max_tokens: u32,
    /// Approval gate: when on, Bash/Edit/Write ask the user before running
    /// (Read is always allowed).
    pub confirm_tools: bool,
    /// Per-turn tool step cap (product-level termination guarantee).
    pub max_tool_steps: usize,
    tool_steps: usize,
}

impl Agent {
    pub fn new(api: Client, session: Session, provider: provider::Provider) -> Self {
        Self {
            api,
            session,
            provider,
            max_tokens: provider.max_tokens,
            confirm_tools: false,
            max_tool_steps: machine::MAX_TOOL_STEPS,
            tool_steps: 0,
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

    /// Point the machine at a provider: a client for its endpoint and key, and
    /// the ceiling its preset declares. The session's meta is the caller's to
    /// write -- it is a change to the log, and the interpreter writes the log.
    pub fn bind(&mut self, provider: provider::Provider, api: Client) {
        self.provider = provider;
        self.max_tokens = provider.max_tokens;
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

    /// One conversational turn: may contain several sub-requests (the model keeps
    /// going after calling tools, until finish_reason=stop).
    /// The renderer is held by the caller and passed in: the status bar and the
    /// streaming output must go through the same `Ui` implementation, otherwise
    /// the cache stats recorded by `usage()` never reach the instance that
    /// already drew the bar.
    ///
    /// Exactly one interrupt listener is subscribed per turn, and the caller
    /// subscribes it before calling this: tokio's signal notifications ride on a
    /// watch, so if a listener were created inside each `select`, a SIGINT
    /// arriving in the gap between two `select`s would be broadcast away before
    /// the new listener subscribed and would be lost forever (measured in the
    /// pty smoke test, a millisecond-scale window). The caller owning the
    /// subscription also keeps the interpreter independent of how a cancel
    /// arrives: `Sigint` is only one possible source.
    ///
    /// `interrupt` is the source of an out-of-band Command (Ctrl-C): every await
    /// point races against it (biased: if the effect finishes first its result is
    /// kept).
    /// When it fires, the current effect is dropped — the stream disconnects and
    /// the Bash child process is killed by kill_on_drop; the calls that were
    /// declared but not answered are persisted with CANCELLED_RESULT to close the
    /// window, the history stays is_request_valid, and the next turn continues
    /// from a valid prefix.
    /// Cancellation is handled in the interpreter layer and never enters
    /// `next_action` (it is out-of-band and every state is reachable).
    ///
    /// `approve` answers the approval gate. Both are supplied by the caller
    /// rather than built here because both depend on which front end is running:
    /// a front end that owns the terminal in raw mode leaves no SIGINT to listen
    /// for, so it answers both channels from its own event loop.
    pub async fn turn(
        &mut self,
        input: &str,
        ui: &mut dyn Ui,
        interrupt: &mut dyn Interrupt,
        approve: &mut dyn Approve,
    ) -> Result<()> {
        self.session.append_message(&Message::user(input))?;
        let mut cancelled = false;
        // Interpreter loop: each beat asks the machine for a decision (an Action
        // folded out of the log) and writes the result back into the log, until
        // Done. The loop itself carries no state — the log is the only history.
        loop {
            match machine::next_action(&self.session.messages) {
                Some(Action::CallModel) => {
                    let req = self.build_request();
                    // The future is dropped when the select expression ends, which
                    // ends the borrow of self
                    let done = tokio::select! {
                        biased;
                        r = self.pump(&req, ui) => Some(r?),
                        _ = interrupt.wait() => None,
                    };
                    match done {
                        Some((msg, usage, stream_time)) => {
                            self.session.append_message(&msg)?;
                            if let Some(u) = usage {
                                ui.usage(&u, stream_time);
                            }
                        }
                        None => cancelled = true,
                    }
                }
                Some(Action::ExecTool(call)) => {
                    ui.tool_start(&call.function.name, &call.function.arguments);
                    // Step cap: every ExecTool action counts as a step (including
                    // denied ones); exceeding it ends the turn with a
                    // deterministic marker, so a model that goes haywire in a loop
                    // cannot run forever.
                    if self.tool_steps >= self.max_tool_steps {
                        ui.tool_result(machine::STEP_LIMIT_RESULT);
                        self.session.append_message(&Message::tool(
                            &call.id,
                            machine::STEP_LIMIT_RESULT.to_string(),
                        ))?;
                        self.close_open_calls(ui, machine::STEP_LIMIT_RESULT)?;
                        break;
                    }
                    self.tool_steps += 1;
                    // Approval gate: Bash/Edit/Write ask first; a denial closes
                    // that call with DENIED_RESULT
                    let mut denied = false;
                    if self.confirm_tools && call.function.name != tools::READ_NAME {
                        ui.approval_requested(&call.function.name, &call.function.arguments);
                        let approved = tokio::select! {
                            biased;
                            ok = approve.ask(&call) => ok,
                            _ = interrupt.wait() => { cancelled = true; false }
                        };
                        if cancelled {
                            break;
                        }
                        if !approved {
                            denied = true;
                            ui.tool_result(machine::DENIED_RESULT);
                            self.session.append_message(&Message::tool(
                                &call.id,
                                machine::DENIED_RESULT.to_string(),
                            ))?;
                        }
                    }
                    if !denied {
                        let out = tokio::select! {
                            biased;
                            out = tools::execute(&call.function.name, &call.function.arguments) => out,
                            _ = interrupt.wait() => machine::CANCELLED_RESULT.to_string(),
                        };
                        let cancelled_call = out == machine::CANCELLED_RESULT;
                        ui.tool_result(&out);
                        self.session.append_message(&Message::tool(&call.id, out))?;
                        if cancelled_call {
                            cancelled = true;
                        }
                    }
                }
                Some(Action::Done) | None => break,
            }
            if cancelled {
                break;
            }
        }
        if cancelled {
            self.close_open_calls(ui, machine::CANCELLED_RESULT)?;
            ui.interrupted();
        }
        Ok(())
    }

    /// Interruption/step-limit cleanup: persist the given marker for calls that
    /// were declared but not answered, closing the window.
    /// Persisted rather than kept in the in-memory view only — the process is
    /// still alive, so the file has to record it faithfully.
    fn close_open_calls(&mut self, ui: &mut dyn Ui, marker: &str) -> Result<()> {
        for id in machine::open_call_ids(&self.session.messages) {
            self.session.append_message(&Message::tool(&id, marker))?;
            ui.tool_result(marker);
        }
        Ok(())
    }

    /// Run one sub-request: consume the SSE stream (notifying the UI delta by
    /// delta) and aggregate a complete assistant message.
    /// Errors propagate upward; at that point the assistant message has not been
    /// persisted, so the session stays at a valid prefix.
    async fn pump(
        &self,
        req: &ChatRequest,
        ui: &mut dyn Ui,
    ) -> Result<(Message, Option<Usage>, Duration)> {
        let mut stream = self.api.stream_chat(req).await?;
        // The wall time the stream took, first chunk to last: what a
        // tokens-per-second figure divides the reported completion tokens by.
        // Connection setup is not counted -- the speed of a stream is the speed
        // of the tokens, not of the handshake.
        let started = Instant::now();
        let mut acc = TurnAccumulator::default();
        let mut usage: Option<Usage> = None;
        while let Some(chunk) = stream.next_chunk().await? {
            for choice in chunk.choices {
                let Some(delta) = choice.delta else { continue };
                if let Some(s) = &delta.reasoning_content {
                    ui.reasoning_delta(s);
                }
                if let Some(s) = &delta.content {
                    ui.content_delta(s);
                }
                acc.feed(&delta);
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        ui.finish_turn();
        Ok((acc.finish(), usage, started.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    /// Test double: fires immediately at the first wait point (simulating a
    /// signal that already arrived).
    struct Immediate;
    impl Interrupt for Immediate {
        fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async {})
        }
    }

    /// Test double: pops cancellation points in order; once exhausted it never
    /// fires again.
    struct Steps(std::collections::VecDeque<Pin<Box<dyn Future<Output = ()>>>>);
    impl Interrupt for Steps {
        fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
            self.0
                .pop_front()
                .unwrap_or_else(|| Box::pin(std::future::pending()))
        }
    }

    /// Test double: never cancels. For tests that are not about cancellation.
    struct Silent;
    impl Interrupt for Silent {
        fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(std::future::pending())
        }
    }

    /// Test double: answers every approval the same way.
    struct Answer(bool);
    impl Approve for Answer {
        fn ask(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>> {
            let verdict = self.0;
            Box::pin(async move { verdict })
        }
    }

    /// Test double: never answers, which leaves the approval gate open so that a
    /// cancellation has something to race against.
    struct NoAnswer;
    impl Approve for NoAnswer {
        fn ask(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>> {
            Box::pin(std::future::pending())
        }
    }

    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    use crate::types::ToolCall;
    // The prompt is asserted on here, but it lives with the request it belongs
    // to; the pure request tests move next to it in the same file.
    use super::request::SYSTEM_PROMPT;
    use crate::session::SessionMeta;
    use crate::types::Role;
    use crate::ui::Renderer;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
        }
    }

    /// Build one SSE chunk line (including the blank-line separator).
    fn sse(
        delta: serde_json::Value,
        finish: Option<&str>,
        usage: Option<serde_json::Value>,
    ) -> String {
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
        )
        .unwrap();
        let session = Session::create(dir, test_meta()).unwrap();
        Agent::new(api, session, provider::DEEPSEEK)
    }

    #[test]
    fn system_prompt_is_stable_constant() {
        // Guard against someone injecting dynamic content into the system prompt
        // and breaking KVCache in the future
        assert!(!SYSTEM_PROMPT.contains("now"));
        assert!(!SYSTEM_PROMPT.contains("cwd"));
    }

    #[test]
    fn build_request_prepends_system_and_keeps_history_order() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("q1")).unwrap();
        let agent = Agent::new(
            Client::new("k".into(), provider::DEEPSEEK.url.into()).unwrap(),
            s,
            provider::DEEPSEEK,
        );
        let req = agent.build_request();
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[0].content.as_deref(), Some(SYSTEM_PROMPT));
        assert_eq!(req.messages[1], Message::user("q1"));
        assert_eq!(req.tools.as_ref().unwrap()[0].function.name, "Bash");
        assert_eq!(req.tool_choice.as_deref(), Some("auto"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_request_defaults_missing_effort_to_max() {
        // Even when an older session's meta has no effort, it must be pinned to
        // max rather than left to each backend's default.
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.meta.reasoning_effort = None;
        s.append_message(&Message::user("q1")).unwrap();
        let agent = Agent::new(
            Client::new("k".into(), provider::DEEPSEEK.url.into()).unwrap(),
            s,
            provider::DEEPSEEK,
        );
        let req = agent.build_request();
        assert_eq!(req.reasoning_effort.as_deref(), Some("max"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn effort_label_reads_the_stored_tier_or_the_providers_default() {
        let dir = tmpdir();
        let s = Session::create(&dir, test_meta()).unwrap();
        let agent = Agent::new(
            Client::new("k".into(), provider::DEEPSEEK.url.into()).unwrap(),
            s,
            provider::DEEPSEEK,
        );
        assert_eq!(agent.effort_label(), "high");

        // A session that stored none is at the provider's default, which is the
        // tier the request is built with.
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.meta.reasoning_effort = None;
        let agent = Agent::new(
            Client::new("k".into(), provider::ZAI_CODING_CN.url.into()).unwrap(),
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
                &mut Silent,
                &mut Answer(false),
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
        assert!(
            tool_msg
                .content
                .as_deref()
                .unwrap()
                .contains("caocli-mock-marker")
        );
        assert_eq!(
            agent.session.messages[3].content.as_deref(),
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
            .turn("run two commands", &mut ui, &mut Silent, &mut Answer(false))
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
        assert!(msgs[2].content.as_deref().unwrap().contains("one"));
        assert!(msgs[3].content.as_deref().unwrap().contains("two"));

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
        let mut interrupt = Immediate;
        agent
            .turn(
                "original instruction",
                &mut ui,
                &mut interrupt,
                &mut Answer(true),
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
        // cancellation point queue: never fires at pump (let the stream finish);
        // fires immediately at the first tool
        let mut queue: std::collections::VecDeque<Pin<Box<dyn Future<Output = ()>>>> =
            Default::default();
        queue.push_back(Box::pin(std::future::pending::<()>()));
        queue.push_back(Box::pin(async {}));
        let mut interrupt = Steps(queue);
        agent
            .turn(
                "run two slow commands",
                &mut ui,
                &mut interrupt,
                &mut Answer(true),
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
            msgs[2].content.as_deref(),
            Some(machine::CANCELLED_RESULT),
            "a call interrupted while executing is marked cancelled too"
        );
        assert_eq!(msgs[3].content.as_deref(), Some(machine::CANCELLED_RESULT));
        // the window is closed: the history is valid, so the next turn's decision
        // is to send a request rather than resurrect zombie calls
        assert!(machine::is_request_valid(msgs));
        assert_eq!(machine::next_action(msgs), Some(machine::Action::CallModel));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Regression pin for the lost-signal window: a signal arriving between two
    /// waits must be visible to the next wait immediately. The real SIGINT relies
    /// on the same tokio watch semantics — one listener per turn, subscribed at
    /// construction; if it degenerated into a listener created per select, the gap
    /// between two selects would swallow a signal arriving exactly then (measured
    /// in the pty smoke test).
    #[tokio::test]
    async fn signal_between_waits_is_not_lost() {
        // A double using the same watch semantics to mimic the real listener's
        // state machine
        struct Gate(tokio::sync::watch::Receiver<bool>);
        impl Interrupt for Gate {
            fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
                let rx = &mut self.0;
                Box::pin(async move {
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                })
            }
        }
        let (tx, rx) = tokio::sync::watch::channel(false);
        let mut gate = Gate(rx);

        // first wait: not fired, polled once and dropped (matching the end of the
        // pump select)
        let w = gate.wait();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), w)
                .await
                .is_err(),
            "it should not be ready before firing"
        );

        // the signal arrives between the two selects
        tx.send(true).unwrap();

        // the next wait must be ready immediately — this is the lost window that
        // was fixed
        let w = gate.wait();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), w)
                .await
                .is_ok(),
            "the signal must not be lost"
        );
    }

    /// Approval denied: DENIED_RESULT closes that call, the window is valid, and
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
        agent.confirm_tools = true;
        let mut ui = Renderer::new();
        agent
            .turn(
                "run the forbidden command",
                &mut ui,
                &mut Steps(Default::default()),
                &mut Answer(false),
            )
            .await
            .unwrap();
        let msgs = &agent.session.messages;
        assert_eq!(
            msgs.len(),
            4,
            "user / assistant / denial marker / closing assistant"
        );
        assert_eq!(msgs[2].content.as_deref(), Some(machine::DENIED_RESULT));
        assert!(machine::is_request_valid(msgs));
        // the window is closed and the denial went back to the model: the closing
        // assistant proves the model digested the denial
        assert_eq!(msgs[3].role, Role::Assistant);
        assert!(
            msgs[3]
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("Skipped")
        );
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
        agent.confirm_tools = true;
        let mut ui = Renderer::new();
        // cancellation point queue: no fire at pump; immediate fire at the approval wait
        let mut queue: std::collections::VecDeque<Pin<Box<dyn Future<Output = ()>>>> =
            Default::default();
        queue.push_back(Box::pin(std::future::pending::<()>()));
        queue.push_back(Box::pin(async {}));
        let mut interrupt = Steps(queue);
        agent
            .turn("run it", &mut ui, &mut interrupt, &mut NoAnswer)
            .await
            .unwrap();
        let msgs = &agent.session.messages;
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].content.as_deref(), Some(machine::CANCELLED_RESULT));
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
                &mut Steps(Default::default()),
                &mut Answer(true),
            )
            .await
            .unwrap();
        let msgs = &agent.session.messages;
        assert_eq!(msgs.len(), 5, "user / assistant(3 calls) / tool×3");
        assert_eq!(
            msgs[4].content.as_deref(),
            Some(machine::STEP_LIMIT_RESULT),
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
        let args = json!({"file_path": target.to_string_lossy(), "content": "written by mock"})
            .to_string();
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
            .turn("write a file", &mut ui, &mut Silent, &mut Answer(false))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "written by mock");
        let tool_msg = &agent.session.messages[2];
        assert!(tool_msg.content.as_deref().unwrap().starts_with("ok:"));

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Bash", "Read", "Edit", "Write"]);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&file_dir).unwrap();
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
            .turn("hi", &mut ui, &mut Silent, &mut Answer(false))
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
            .turn("hi", &mut ui, &mut Silent, &mut Answer(false))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse SSE chunk"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    /// A history with a closed tool-call window: system, a call, its result, and
    /// a next user line — every message shape the request carries.
    fn freeze_history() -> Vec<Message> {
        vec![
            Message::user("freeze"),
            Message {
                role: Role::Assistant,
                content: Some("".into()),
                reasoning_content: Some("think".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_f1".into(),
                    r#type: "function".into(),
                    function: crate::types::ToolCallFunction {
                        name: "Bash".into(),
                        arguments: r#"{"command":"true"}"#.into(),
                    },
                }]),
                tool_call_id: None,
            },
            Message::tool("call_f1", "exit_code: 0"),
            Message::user("again"),
        ]
    }

    /// The request prefix, frozen. The backend's KVCache matches on bytes: any
    /// change to the system prompt, the tool definitions, the field names or a
    /// provider profile changes every request this program sends and voids the
    /// cache of every session that ran before it. Some of those changes are
    /// right and some are accidents — this test turns each one into a decision,
    /// by failing until the literal below is updated with it.
    #[test]
    fn the_request_prefix_is_frozen() {
        let history = freeze_history();
        let deepseek = SessionMeta {
            provider: Some("deepseek".into()),
            model: "deepseek-flash".into(),
            reasoning_effort: Some("high".into()),
        };
        assert_eq!(
            serde_json::to_string(&build_request(&provider::DEEPSEEK, &deepseek, &history))
                .unwrap(),
            r#"{"model":"deepseek-flash","max_tokens":384000,"messages":[{"role":"system","content":"You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist."},{"role":"user","content":"freeze"},{"role":"assistant","content":"","reasoning_content":"think","tool_calls":[{"id":"call_f1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"true\"}"}}]},{"role":"tool","content":"exit_code: 0","tool_call_id":"call_f1"},{"role":"user","content":"again"}],"tools":[{"type":"function","function":{"name":"Bash","description":"Run one bash command on the local machine; returns exit_code, stdout and stderr (returned separately). Every call is a fresh shell: the working directory and environment variables do not persist, so to target a directory write cd /abs/path && ... inside the same command, and prefer absolute paths. Use for: running programs, builds, tests, git, directory operations, bulk text processing. Not for: reading a text file (use Read), modifying an existing file (use Edit), creating or fully rewriting a file (use Write); do not substitute cat/sed -i/tee for them. stdout and stderr are each truncated at 10240 bytes and marked [truncated]; narrow the output yourself with head/tail/grep/wc. Do not run interactive or long-lived commands (vim, top, a bare read, etc.): they block until the 120s timeout kills them and the output is lost.","parameters":{"properties":{"command":{"description":"the bash command to run","type":"string"}},"required":["command"],"type":"object"}}},{"type":"function","function":{"name":"Read","description":"Read the full contents of a text file. Confirm the original text with this tool before modifying a file. UTF-8 text only: a directory raises an error, while a binary file is decoded into garbage without raising one. Output over 10240 bytes is truncated on a byte boundary and reported, and a file over 10MB raises an error; in either case read it in pieces with Bash instead, e.g. sed -n '100,200p'.","parameters":{"properties":{"file_path":{"description":"path of the file to read","type":"string"}},"required":["file_path"],"type":"object"}}},{"type":"function","function":{"name":"Edit","description":"Make an exact string replacement in an existing file (old_string -> new_string). Cannot create a new file; use Write to create one or to rewrite a whole file. old_string must match the file content character for character, including indentation, tab-versus-space differences and line endings — one character off is reported as not found, so use Read to check the original when the indentation is uncertain. old_string must occur exactly once in the file (zero or multiple occurrences is an error); one call replaces one occurrence, so make several calls for several edits. old_string must not be empty; an empty new_string deletes the matched text.","parameters":{"properties":{"file_path":{"description":"path of the file to modify","type":"string"},"new_string":{"description":"the replacement text; an empty string deletes the matched text","type":"string"},"old_string":{"description":"the original text to replace; must occur exactly once in the file","type":"string"}},"required":["file_path","old_string","new_string"],"type":"object"}}},{"type":"function","function":{"name":"Write","description":"Create a file, or overwrite a whole file; missing parent directories are created automatically. An existing file is overwritten completely and unrecoverably, so use Read to check it first; use this only to create a file or rewrite one wholesale, and use Edit for partial changes to an existing file. A content larger than 10MB is rejected.","parameters":{"properties":{"content":{"description":"the full contents to write","type":"string"},"file_path":{"description":"path of the file to write","type":"string"}},"required":["file_path","content"],"type":"object"}}}],"tool_choice":"auto","stream":true,"thinking":{"type":"enabled"},"reasoning_effort":"high"}"#
        );
        // The other preset: its own answer ceiling, and the default effort when
        // the meta carries none (a stored value rides along in `deepseek` above).
        let zai = SessionMeta {
            provider: Some("zai-coding-cn".into()),
            model: "glm-5.3".into(),
            reasoning_effort: None,
        };
        assert_eq!(
            serde_json::to_string(&build_request(&provider::ZAI_CODING_CN, &zai, &history))
                .unwrap(),
            r#"{"model":"glm-5.3","max_tokens":128000,"messages":[{"role":"system","content":"You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist."},{"role":"user","content":"freeze"},{"role":"assistant","content":"","reasoning_content":"think","tool_calls":[{"id":"call_f1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"true\"}"}}]},{"role":"tool","content":"exit_code: 0","tool_call_id":"call_f1"},{"role":"user","content":"again"}],"tools":[{"type":"function","function":{"name":"Bash","description":"Run one bash command on the local machine; returns exit_code, stdout and stderr (returned separately). Every call is a fresh shell: the working directory and environment variables do not persist, so to target a directory write cd /abs/path && ... inside the same command, and prefer absolute paths. Use for: running programs, builds, tests, git, directory operations, bulk text processing. Not for: reading a text file (use Read), modifying an existing file (use Edit), creating or fully rewriting a file (use Write); do not substitute cat/sed -i/tee for them. stdout and stderr are each truncated at 10240 bytes and marked [truncated]; narrow the output yourself with head/tail/grep/wc. Do not run interactive or long-lived commands (vim, top, a bare read, etc.): they block until the 120s timeout kills them and the output is lost.","parameters":{"properties":{"command":{"description":"the bash command to run","type":"string"}},"required":["command"],"type":"object"}}},{"type":"function","function":{"name":"Read","description":"Read the full contents of a text file. Confirm the original text with this tool before modifying a file. UTF-8 text only: a directory raises an error, while a binary file is decoded into garbage without raising one. Output over 10240 bytes is truncated on a byte boundary and reported, and a file over 10MB raises an error; in either case read it in pieces with Bash instead, e.g. sed -n '100,200p'.","parameters":{"properties":{"file_path":{"description":"path of the file to read","type":"string"}},"required":["file_path"],"type":"object"}}},{"type":"function","function":{"name":"Edit","description":"Make an exact string replacement in an existing file (old_string -> new_string). Cannot create a new file; use Write to create one or to rewrite a whole file. old_string must match the file content character for character, including indentation, tab-versus-space differences and line endings — one character off is reported as not found, so use Read to check the original when the indentation is uncertain. old_string must occur exactly once in the file (zero or multiple occurrences is an error); one call replaces one occurrence, so make several calls for several edits. old_string must not be empty; an empty new_string deletes the matched text.","parameters":{"properties":{"file_path":{"description":"path of the file to modify","type":"string"},"new_string":{"description":"the replacement text; an empty string deletes the matched text","type":"string"},"old_string":{"description":"the original text to replace; must occur exactly once in the file","type":"string"}},"required":["file_path","old_string","new_string"],"type":"object"}}},{"type":"function","function":{"name":"Write","description":"Create a file, or overwrite a whole file; missing parent directories are created automatically. An existing file is overwritten completely and unrecoverably, so use Read to check it first; use this only to create a file or rewrite one wholesale, and use Edit for partial changes to an existing file. A content larger than 10MB is rejected.","parameters":{"properties":{"content":{"description":"the full contents to write","type":"string"},"file_path":{"description":"path of the file to write","type":"string"}},"required":["file_path","content"],"type":"object"}}}],"tool_choice":"auto","stream":true,"thinking":{"type":"enabled"},"reasoning_effort":"max"}"#
        );
    }

    /// The profile as wire: a provider that does not take the thinking switch
    /// sends no `thinking` field at all, and its own default effort applies when
    /// the meta stores none.
    #[test]
    fn a_provider_that_omits_thinking_sends_no_field() {
        let mut p = provider::ZAI_CODING_CN;
        p.send_thinking = false;
        let meta = SessionMeta {
            provider: Some("zai-coding-cn".into()),
            model: "glm-5.3".into(),
            reasoning_effort: None,
        };
        let json = serde_json::to_string(&build_request(&p, &meta, &[])).unwrap();
        assert!(!json.contains("\"thinking\""), "{json}");
        assert!(json.contains("\"reasoning_effort\":\"max\""), "{json}");
    }
}
