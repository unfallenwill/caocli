use anyhow::Result;

use crate::api::Client;
use crate::session::Session;
use crate::tools;
use crate::types::{ChatRequest, Message, TurnAccumulator, Usage};
use crate::ui::Renderer;

/// 参与请求前缀（KVCache）。禁止注入时间、cwd、随机 id 等任何动态内容，
/// 否则每个请求的前缀都不同，缓存全 miss。
pub const SYSTEM_PROMPT: &str = "You are caocli, a terminal coding agent. You can run shell commands on the local machine via the run_shell tool. Prefer running commands to gather facts before answering. Keep answers concise.";

pub struct Agent {
    api: Client,
    pub session: Session,
    renderer: Renderer,
}

impl Agent {
    pub fn new(api: Client, session: Session) -> Self {
        Self {
            api,
            session,
            renderer: Renderer::new(),
        }
    }

    fn build_request(&self) -> ChatRequest {
        let mut messages = Vec::with_capacity(self.session.messages.len() + 1);
        messages.push(Message::system(SYSTEM_PROMPT));
        messages.extend(self.session.messages.iter().cloned());
        ChatRequest {
            model: self.session.meta.model.clone(),
            messages,
            tools: Some(vec![tools::definition()]),
            tool_choice: Some("auto".into()),
            stream: true,
            thinking: Some(self.session.meta.thinking.clone()),
            reasoning_effort: self.session.meta.reasoning_effort.clone(),
        }
    }

    /// 一轮对话：可能包含多个子请求（模型调用工具后继续，直到 finish_reason=stop）。
    pub async fn turn(&mut self, input: &str) -> Result<()> {
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
                        self.renderer.reasoning_delta(s);
                    }
                    if let Some(s) = &delta.content {
                        self.renderer.content_delta(s);
                    }
                    acc.feed(&delta);
                }
                if chunk.usage.is_some() {
                    usage = chunk.usage;
                }
            }
            self.renderer.finish_turn();

            let msg = acc.finish();
            self.session.append_message(&msg)?;
            if let Some(u) = usage {
                self.renderer.usage(&u);
            }

            let Some(calls) = msg.tool_calls.clone() else {
                break;
            };
            if calls.is_empty() {
                break;
            }
            for call in calls {
                self.renderer
                    .tool_start(&call.function.name, &call.function.arguments);
                let out = tools::execute(&call.function.arguments).await;
                self.renderer.tool_result(&out);
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

    #[test]
    fn system_prompt_is_stable_constant() {
        // 防止未来有人把动态内容塞进 system prompt 破坏 KVCache
        assert!(!SYSTEM_PROMPT.contains("now"));
        assert!(!SYSTEM_PROMPT.contains("cwd"));
    }

    #[test]
    fn build_request_prepends_system_and_keeps_history_order() {
        let dir = std::env::temp_dir().join(format!("caocli-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let meta = SessionMeta {
            model: "deepseek-v4-flash".into(),
            thinking: Thinking::enabled(),
            reasoning_effort: Some("high".into()),
        };
        let mut s = Session::create(&dir, meta).unwrap();
        s.append_message(&Message::user("q1")).unwrap();
        let agent = Agent {
            api: Client::new("k".into(), crate::config::BASE_URL.into()).unwrap(),
            session: s,
            renderer: Renderer::new(),
        };
        let req = agent.build_request();
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[0].content.as_deref(), Some(SYSTEM_PROMPT));
        assert_eq!(req.messages[1], Message::user("q1"));
        assert_eq!(req.tools.as_ref().unwrap()[0].function.name, "run_shell");
        assert_eq!(req.tool_choice.as_deref(), Some("auto"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
