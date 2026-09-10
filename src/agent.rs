use anyhow::Result;

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use crate::api::Client;
use crate::machine::{self, Action};
use crate::provider;
use crate::session::Session;
use crate::tools;
use crate::types::{ChatRequest, Message, Thinking, ToolCall, TurnAccumulator, Usage};
use crate::ui::Ui;

/// Participates in the request prefix (KVCache). Injecting time, cwd, a random
/// id or any other dynamic content is forbidden, or every request would have a
/// different prefix and the cache would miss entirely.
pub const SYSTEM_PROMPT: &str = "You are caocli, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist.";

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

/// Out-of-band cancellation (Ctrl-C) source: one long-lived listener is held for
/// the whole turn and lends out a droppable wait future on demand. Waiting is
/// cancel-safe: dropping the future does not lose the signal (the state lives in
/// the listener), and an unconsumed signal makes the next `wait()` ready
/// immediately.
/// The output lifetime is bound to `&mut self` — the `Fn` family cannot express
/// "the return value borrows the receiver".
pub trait Interrupt {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

/// The real SIGINT listener. It subscribes to tokio's watch at construction time
/// (no poll needed), so a signal arriving at any moment from the start of the
/// turn to its end cannot be lost to a "no listener" gap.
pub struct Sigint(
    #[cfg(unix)] tokio::signal::unix::Signal,
    #[cfg(not(unix))] tokio::signal::windows::CtrlC,
);

impl Sigint {
    /// Subscribe to SIGINT. The caller constructs one per turn, before the first
    /// await point, so no signal can arrive before the subscription exists.
    pub fn new() -> Result<Self> {
        #[cfg(unix)]
        let listener = Self(tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        )?);
        #[cfg(not(unix))]
        let listener = Self(tokio::signal::windows::ctrl_c()?);
        Ok(listener)
    }
}

impl Interrupt for Sigint {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async {
            self.0.recv().await;
        })
    }
}

/// The approval gate's answer source: the Input channel that pairs with the
/// Notice a `Ui` sends when it asks. A Notice never returns data, so the answer
/// comes back through a channel of its own -- this is that channel.
///
/// Like `Interrupt` it is a trait rather than a closure so that a front end
/// owning the terminal can take the answer from its own event loop, and so the
/// output lifetime can be bound to `&mut self`.
pub trait Approve {
    fn ask(&mut self, call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>>;
}

/// One line of stdin per question, denial by default: the plain front end's
/// answer source.
pub struct StdinApproval;

impl Approve for StdinApproval {
    fn ask(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>> {
        Box::pin(async {
            // The blocking read is wrapped in spawn_blocking: the global stdin
            // buffer is shared across calls, so surplus type-ahead is not lost.
            // (Cost: on cancellation a blocked thread lingers and swallows the
            // first line typed afterwards -- a known trade-off.)
            tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                let read = std::io::stdin().read_line(&mut line);
                let line = line.trim();
                read.map(|n| n > 0).unwrap_or(false)
                    && (line.eq_ignore_ascii_case("y") || line.starts_with('y'))
            })
            .await
            .unwrap_or(false)
        })
    }
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
        let mut messages = Vec::with_capacity(self.session.messages.len() + 1);
        messages.push(Message::system(SYSTEM_PROMPT));
        messages.extend(self.session.messages.iter().cloned());
        // Specification tripwire (debug builds only): the history being sent must
        // satisfy the executable specification. A violation is a shape the
        // backend answers with a 400 — catch it during development rather than in
        // production.
        debug_assert!(
            machine::is_request_valid(&messages),
            "request history violates the tool_calls window specification: {messages:?}"
        );
        ChatRequest {
            model: self.session.meta.model.clone(),
            max_tokens: self.max_tokens,
            messages,
            tools: Some(tools::definitions()),
            tool_choice: Some("auto".into()),
            stream: true,
            // The thinking switch and the effort fallback are the provider's:
            // a preset that omits one or defaults differently says so in the
            // table, and nothing here needs to know which.
            thinking: self.provider.send_thinking.then(Thinking::enabled),
            reasoning_effort: Some(
                self.session
                    .meta
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| self.provider.default_effort.to_string()),
            ),
        }
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
}
