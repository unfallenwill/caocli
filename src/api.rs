use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::pin::Pin;
use std::time::Duration;

use crate::types::ChatChunk;

// ============================================================================
// HTTP 客户端 + SSE 流式解析。
// parse_sse_line / take_line 是纯函数，可单测；SseStream::next_chunk 逐块返回。
// 流式约定：delta.reasoning_content 先于 delta.content；data: [DONE] 结束；
// usage 附在最后一个内容块上（不存在独立 usage 块）。
// ============================================================================

type ByteStream = Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    url: String,
}

impl Client {
    pub fn new(api_key: String, url: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .context("构建 HTTP 客户端失败")?;
        Ok(Self { http, api_key, url })
    }

    pub async fn stream_chat(&self, req: &crate::types::ChatRequest) -> Result<SseStream> {
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(req)
            .send()
            .await
            .context("请求发送失败（网络错误）")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("API 返回 HTTP {status}\n响应体: {body}");
        }
        Ok(SseStream { inner: Box::pin(resp.bytes_stream()), buf: Vec::new(), done: false })
    }
}

pub struct SseStream {
    inner: ByteStream,
    buf: Vec<u8>,
    done: bool,
}

impl SseStream {
    /// 返回 None 表示流结束（收到 [DONE] 或连接关闭）。
    pub async fn next_chunk(&mut self) -> Result<Option<ChatChunk>> {
        loop {
            if let Some(line) = take_line(&mut self.buf) {
                match parse_sse_line(&line)? {
                    SseLine::Chunk(c) => return Ok(Some(c)),
                    SseLine::Done => {
                        self.done = true;
                        return Ok(None);
                    }
                    SseLine::Ignored => continue,
                }
            }
            if self.done {
                return Ok(None);
            }
            match self.inner.next().await {
                Some(Ok(bytes)) => self.buf.extend_from_slice(&bytes),
                Some(Err(e)) => bail!("流读取错误: {e}"),
                None => self.done = true,
            }
        }
    }
}

/// 从缓冲区取出一行（含 \n 即返回），处理 \r\n。无完整行返回 None。
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.iter().position(|&b| b == b'\n')?;
    let line: Vec<u8> = buf.drain(..=pos).collect();
    let mut s = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
    if s.ends_with('\r') {
        s.pop();
    }
    Some(s)
}

pub enum SseLine {
    Chunk(ChatChunk),
    Done,
    Ignored,
}

/// 解析单行 SSE。只处理 "data:" 行；[DONE] 结束；其余（空行/注释/event: 字段）忽略。
fn parse_sse_line(line: &str) -> Result<SseLine> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(SseLine::Ignored);
    };
    let data = data.strip_prefix(' ').unwrap_or(data);
    if data.trim() == "[DONE]" {
        return Ok(SseLine::Done);
    }
    if data.trim().is_empty() {
        return Ok(SseLine::Ignored);
    }
    let chunk: ChatChunk = serde_json::from_str(data)
        .with_context(|| format!("解析 SSE chunk 失败: {data}"))?;
    Ok(SseLine::Chunk(chunk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatRequest, Message, Thinking};

    fn chunk_with_content(s: &str) -> String {
        format!(
            r#"{{"id":"1","choices":[{{"index":0,"delta":{{"content":"{s}"}},"finish_reason":null}}],"created":1,"model":"deepseek-v4-flash","object":"chat.completion.chunk"}}"#
        )
    }

    #[test]
    fn parse_data_line() {
        let line = chunk_with_content("hi");
        match parse_sse_line(&format!("data: {line}")).unwrap() {
            SseLine::Chunk(c) => {
                assert_eq!(c.choices[0].delta.as_ref().unwrap().content.as_deref(), Some("hi"));
            }
            _ => panic!("应为 Chunk"),
        }
    }

    #[test]
    fn parse_done_line() {
        assert!(matches!(parse_sse_line("data: [DONE]").unwrap(), SseLine::Done));
        assert!(matches!(parse_sse_line("data:[DONE]").unwrap(), SseLine::Done));
    }

    #[test]
    fn ignores_non_data_lines() {
        assert!(matches!(parse_sse_line("").unwrap(), SseLine::Ignored));
        assert!(matches!(parse_sse_line(": keep-alive").unwrap(), SseLine::Ignored));
        assert!(matches!(parse_sse_line("event: ping").unwrap(), SseLine::Ignored));
    }

    #[test]
    fn bad_json_is_error() {
        assert!(parse_sse_line("data: {broken").is_err());
    }

    #[test]
    fn take_line_handles_crlf_and_split_buffers() {
        let mut buf: Vec<u8> = b"data: {\"a\":1}\r\ndata: [DONE]\nresidual".to_vec();
        let l1 = take_line(&mut buf).unwrap();
        assert_eq!(l1, "data: {\"a\":1}");
        let l2 = take_line(&mut buf).unwrap();
        assert_eq!(l2, "data: [DONE]");
        assert!(take_line(&mut buf).is_none()); // residual 无换行，不取出
        assert_eq!(buf, b"residual");
    }

    #[tokio::test]
    async fn stream_consumes_multiple_chunks_across_buffer_boundaries() {
        // 模拟两个网络包：第一个包切断第二行的 JSON
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n",
            chunk_with_content("a"),
            chunk_with_content("b")
        );
        let bytes = body.as_bytes();
        let split_at = body.find("data: {").unwrap() + 12; // 第二行 JSON 中间切开
        let (p1, p2) = bytes.split_at(split_at);
        let stream = futures_util::stream::iter(vec![
            Ok(bytes::Bytes::from(p1.to_vec())),
            Ok(bytes::Bytes::from(p2.to_vec())),
        ]);
        let mut sse = SseStream { inner: Box::pin(stream), buf: Vec::new(), done: false };

        let c1 = sse.next_chunk().await.unwrap().unwrap();
        assert_eq!(c1.choices[0].delta.as_ref().unwrap().content.as_deref(), Some("a"));
        let c2 = sse.next_chunk().await.unwrap().unwrap();
        assert_eq!(c2.choices[0].delta.as_ref().unwrap().content.as_deref(), Some("b"));
        assert!(sse.next_chunk().await.unwrap().is_none());
        assert!(sse.next_chunk().await.unwrap().is_none());
    }

    #[test]
    fn request_body_shape() {
        let req = ChatRequest {
            model: "deepseek-v4-flash".into(),
            messages: vec![Message::system("sys"), Message::user("hi")],
            tools: None,
            tool_choice: None,
            stream: true,
            thinking: Some(Thinking::enabled()),
            reasoning_effort: Some("max".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "deepseek-v4-flash");
        assert_eq!(v["stream"], true);
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["reasoning_effort"], "max");
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    }
}
