use anyhow::Result;

use crate::api::Client;
use crate::session::Session;
use crate::tools;
use crate::types::{ChatRequest, Message, TurnAccumulator, Usage};
use crate::ui::Renderer;

/// 参与请求前缀（KVCache）。禁止注入时间、cwd、随机 id 等任何动态内容，
/// 否则每个请求的前缀都不同，缓存全 miss。
pub const SYSTEM_PROMPT: &str = "You are caocli, a terminal coding agent. You can run shell commands on the local machine via the Bash tool. Prefer running commands to gather facts before answering. Keep answers concise. Tool routing: use Read to read a file, Edit to modify an existing file, Write to create or fully rewrite a file, and Bash for everything else (running programs, builds, tests, git, directories, bulk text processing). Prefer absolute paths: each Bash call starts a fresh shell, so cd does not persist.";

pub struct Agent {
    api: Client,
    pub session: Session,
}

impl Agent {
    pub fn new(api: Client, session: Session) -> Self {
        Self { api, session }
    }

    fn build_request(&self) -> ChatRequest {
        let mut messages = Vec::with_capacity(self.session.messages.len() + 1);
        messages.push(Message::system(SYSTEM_PROMPT));
        messages.extend(self.session.messages.iter().cloned());
        ChatRequest {
            model: self.session.meta.model.clone(),
            messages,
            tools: Some(tools::definitions()),
            tool_choice: Some("auto".into()),
            stream: true,
            thinking: Some(self.session.meta.thinking.clone()),
            reasoning_effort: self.session.meta.reasoning_effort.clone(),
        }
    }

    /// 一轮对话：可能包含多个子请求（模型调用工具后继续，直到 finish_reason=stop）。
    /// 渲染器由调用方持有并传入：状态栏与流式输出必须走同一个 `Renderer`，
    /// 否则 `usage()` 记录到的缓存统计不会反映到已建栏的实例上。
    pub async fn turn(&mut self, input: &str, ui: &mut Renderer) -> Result<()> {
        self.session.append_message(&Message::user(input))?;
        loop {
            let req = self.build_request();
            let mut stream = self.api.stream_chat(&req).await?;
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

            let msg = acc.finish();
            self.session.append_message(&msg)?;
            if let Some(u) = usage {
                ui.usage(&u);
            }

            let Some(calls) = msg.tool_calls.clone() else {
                break;
            };
            if calls.is_empty() {
                break;
            }
            for call in calls {
                ui.tool_start(&call.function.name, &call.function.arguments);
                let out = tools::execute(&call.function.name, &call.function.arguments).await;
                ui.tool_result(&out);
                self.session.append_message(&Message::tool(&call.id, out))?;
            }
            // 工具结果已入历史，继续子请求让模型消化
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionMeta;
    use crate::types::{Role, Thinking};
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
            model: "deepseek-v4-flash".into(),
            thinking: Thinking::enabled(),
            reasoning_effort: Some("high".into()),
        }
    }

    /// 构造一个 SSE chunk 行（含空行分隔）。
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
        Agent::new(api, session)
    }

    #[test]
    fn system_prompt_is_stable_constant() {
        // 防止未来有人把动态内容塞进 system prompt 破坏 KVCache
        assert!(!SYSTEM_PROMPT.contains("now"));
        assert!(!SYSTEM_PROMPT.contains("cwd"));
    }

    #[test]
    fn build_request_prepends_system_and_keeps_history_order() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("q1")).unwrap();
        let agent = Agent {
            api: Client::new("k".into(), crate::config::BASE_URL.into()).unwrap(),
            session: s,
        };
        let req = agent.build_request();
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[0].content.as_deref(), Some(SYSTEM_PROMPT));
        assert_eq!(req.messages[1], Message::user("q1"));
        assert_eq!(req.tools.as_ref().unwrap()[0].function.name, "Bash");
        assert_eq!(req.tool_choice.as_deref(), Some("auto"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 端到端：tool_calls（arguments 分片）→ 真实执行 Bash → 第二轮带
    /// reasoning_content 回传 → 最终回答。钉住 DeepSeek "带 tools 必须回传
    /// reasoning_content" 硬约束与历史逐字节回放。
    #[tokio::test]
    async fn mock_full_tool_loop_replays_reasoning_content() {
        let server = MockServer::start().await;
        let turn1 = [
            sse(json!({"role":"assistant","reasoning_content":"我需要执行命令。"}), None, None),
            sse(json!({"tool_calls":[{"index":0,"id":"call_mock_1","type":"function","function":{"name":"Bash","arguments":"{\"comm"}}]}), None, None),
            sse(json!({"tool_calls":[{"index":0,"function":{"arguments":"and\":\"echo caocli-mock-marker\"}"}}]}), None, None),
            sse(json!({"content":""}), Some("tool_calls"), Some(json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_cache_hit_tokens":0,"prompt_cache_miss_tokens":10}))),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let turn2 = [
            sse(json!({"content":"已执行完毕。"}), None, None),
            sse(json!({"content":""}), Some("stop"), Some(json!({"prompt_tokens":20,"completion_tokens":8,"total_tokens":28,"prompt_cache_hit_tokens":12,"prompt_cache_miss_tokens":8}))),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        mount_chat(&server, turn1, Some(1)).await;
        mount_chat(&server, turn2, None).await;

        let dir = tmpdir();
        let mut agent = test_agent(&server, &dir);
        let mut ui = Renderer::new();
        agent.turn("用工具打个标记", &mut ui).await.unwrap();

        // usage 必须喂给调用方持有的渲染器，否则状态栏缓存统计永远不更新
        assert_eq!(
            ui.stats(),
            crate::ui::CacheStats { hit: 12, miss: 18 },
            "两个子请求的 hit/miss 应累计到同一渲染器"
        );

        // 会话历史：user / assistant(思维链+tool_calls) / tool(执行结果) / assistant
        assert_eq!(agent.session.messages.len(), 4);
        let assistant1 = &agent.session.messages[1];
        assert_eq!(
            assistant1.reasoning_content.as_deref(),
            Some("我需要执行命令。")
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
        // Bash 真的执行了
        assert!(
            tool_msg
                .content
                .as_deref()
                .unwrap()
                .contains("caocli-mock-marker")
        );
        assert_eq!(
            agent.session.messages[3].content.as_deref(),
            Some("已执行完毕。")
        );

        // 第二轮请求体：核心硬约束 —— reasoning_content 原样回传
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2);
        let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4); // system + user + assistant + tool
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["reasoning_content"], "我需要执行命令。");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_mock_1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_mock_1");
        // 请求参数形状
        assert_eq!(body["model"], "deepseek-v4-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["tools"][0]["function"]["name"], "Bash");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 端到端：Write 工具分派 —— 模型调用 Write，文件真的被创建，且
    /// 第二轮请求携带全部四个工具定义。
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
            sse(json!({"content":"文件已创建。"}), None, None),
            sse(json!({"content":""}), Some("stop"), None),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        mount_chat(&server, turn1, Some(1)).await;
        mount_chat(&server, turn2, None).await;

        let dir = tmpdir();
        let mut agent = test_agent(&server, &dir);
        let mut ui = Renderer::new();
        agent.turn("写个文件", &mut ui).await.unwrap();

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

    /// HTTP 错误必须带状态码和响应体，且不破坏已落盘的会话。
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
        let err = agent.turn("hi", &mut ui).await.unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("400"), "错误信息应含状态码: {s}");
        assert!(
            s.contains("reasoning_content must be passed back"),
            "错误信息应含响应体: {s}"
        );
        // user 消息已落盘，assistant 未落盘
        assert_eq!(agent.session.messages.len(), 1);
        assert_eq!(agent.session.messages[0].role, Role::User);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 坏 SSE chunk 报错并带上下文。
    #[tokio::test]
    async fn mock_broken_sse_chunk_fails_with_context() {
        let server = MockServer::start().await;
        mount_chat(&server, "data: {\"broken\":\n\n".to_string(), None).await;

        let dir = tmpdir();
        let mut agent = test_agent(&server, &dir);
        let mut ui = Renderer::new();
        let err = agent.turn("hi", &mut ui).await.unwrap_err();
        assert!(format!("{err:#}").contains("解析 SSE chunk 失败"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
