//! Live smoke test for tool calls on both Chat Completions and Responses.
//!
//! Run with:
//!
//! ```text
//! OPENAI_LIVE_KEY=sk-… OPENAI_LIVE_BASE=https://api.minimax.cn/v1 \
//!     OPENAI_LIVE_MODEL=MiniMax-M3 \
//!     cargo run -p caocli-openai --example live_tools
//! ```
//!
//! The example reads the key from the environment and never echoes it back,
//! and it asks the model a question that *should* trigger a tool call. The
//! report prints whether the endpoint answered with text or with a tool
//! call, and on the tool-call path what tool the model picked.

use std::env;
use std::time::{Duration, Instant};

use caocli_openai::{
    ChatCompletionMessageParam, ChatCompletionRequest, Client, Profile, ResponseCreateRequest,
    ResponseFunctionTool, ResponseInput, Tool,
};

/// The env var the bearer token is read from.
const KEY_ENV: &str = "OPENAI_LIVE_KEY";

/// The env var the base URL is read from.
const BASE_ENV: &str = "OPENAI_LIVE_BASE";

/// The env var the model name is read from.
const MODEL_ENV: &str = "OPENAI_LIVE_MODEL";

/// Redact a bearer token from a body string before printing.
fn redact(body: &str) -> String {
    let key = match env::var(KEY_ENV) {
        Ok(k) => k,
        Err(_) => return body.to_string(),
    };
    if key.is_empty() {
        return body.to_string();
    }
    body.replace(&key, "<redacted>")
}

/// A tool's surface: the call the model returned.
#[derive(Default)]
struct ToolHit {
    name: String,
    args: String,
}

/// What the endpoint answered with.
#[derive(Default)]
struct Report {
    name: &'static str,
    elapsed: Duration,
    text: Option<String>,
    tool: Option<ToolHit>,
    detail: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let key = match env::var(KEY_ENV) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("OPENAI_LIVE_KEY is not set; refusing to run.");
            std::process::exit(2);
        }
    };
    let base = env::var(BASE_ENV).unwrap_or_else(|_| "https://api.minimax.cn/v1".to_string());
    let model = env::var(MODEL_ENV).unwrap_or_else(|_| "MiniMax-M3".to_string());

    let profile = Profile::base(&base).with_bearer_token(&key);
    let client = match Client::new(profile) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to build client: {}", redact(&e.to_string()));
            std::process::exit(2);
        }
    };

    println!("# live tools");
    println!("base:  {}", base);
    println!("model: {}", model);
    println!("key:   <redacted, len={}>", key.len());
    println!();

    let def = caocli_openai::FunctionDefinition::new(
        "get_current_weather",
        serde_json::json!({
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City, e.g. San Francisco"},
                "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
            },
            "required": ["location"]
        }),
    )
    .with_description("Look up the current weather for a location");
    let tool = Tool::function(def);

    let resp_def = ResponseFunctionTool::new(
        "get_current_weather",
        serde_json::json!({
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City, e.g. San Francisco"},
                "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
            },
            "required": ["location"]
        }),
    )
    .with_description("Look up the current weather for a location");
    let resp_tool = caocli_openai::ResponseTool::Function(resp_def);

    let mut reports = Vec::new();
    reports.push(chat_completions_function(&client, &model, tool.clone()).await);
    reports.push(chat_completions_streaming_function(&client, &model, tool.clone()).await);
    reports.push(responses_function(&client, &model, resp_tool.clone()).await);
    reports.push(responses_streaming_function(&client, &model, resp_tool).await);

    println!();
    println!("# summary");
    let mut all_ok = true;
    for r in &reports {
        let outcome = if r.text.is_some() {
            "TEXT"
        } else if r.tool.is_some() {
            "TOOL"
        } else {
            "ERR "
        };
        let text_preview = r.text.as_deref().map(preview).unwrap_or_default();
        let tool_preview = r
            .tool
            .as_ref()
            .map(|t| format!("{}({})", t.name, preview(&t.args)))
            .unwrap_or_default();
        let line = if outcome == "ERR " {
            all_ok = false;
            format!(
                "{outcome}  {:>6.2}s  {:<40}  {}",
                r.elapsed.as_secs_f64(),
                r.name,
                redact(&r.detail)
            )
        } else {
            format!(
                "{outcome}  {:>6.2}s  {:<40}  text={text_preview} tool={tool_preview}",
                r.elapsed.as_secs_f64(),
                r.name,
            )
        };
        println!("{line}");
    }

    if !all_ok {
        std::process::exit(1);
    }
}

/// A prompt that should trigger the tool: asking about the weather at a known
/// location is the canonical "please call a function" example.
fn weather_prompt() -> &'static str {
    "What is the current weather in San Francisco? Use the get_current_weather tool."
}

async fn chat_completions_function(client: &Client, model: &str, tool: Tool) -> Report {
    let name = "chat completions / function tool / non-stream";
    let started = Instant::now();
    let request = ChatCompletionRequest::new(
        model,
        vec![ChatCompletionMessageParam::user(weather_prompt())],
    )
    .with_max_tokens(512)
    .with_tool(tool.clone());
    let result = client.completion(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(completion) => {
            let choice = completion.choices.first();
            let text = choice
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();
            let tool_hit = choice.and_then(|c| {
                c.message.tool_calls.as_ref().and_then(|calls| {
                    calls.first().map(|call| match call {
                        caocli_openai::MessageToolCall::Function { id: _, function } => ToolHit {
                            name: function.name.clone(),
                            args: function.arguments.clone(),
                        },
                    })
                })
            });
            if let Some(hit) = tool_hit {
                report.tool = Some(hit);
            } else if !text.is_empty() {
                report.text = Some(text);
            } else {
                report.detail = "empty answer".into();
            }
        }
        Err(e) => report.detail = redact(&format!("{e:?}")),
    }
    report
}

async fn chat_completions_streaming_function(client: &Client, model: &str, tool: Tool) -> Report {
    let name = "chat completions / function tool / stream";
    let started = Instant::now();
    let request = ChatCompletionRequest::streaming(
        model,
        vec![ChatCompletionMessageParam::user(weather_prompt())],
    )
    .with_max_tokens(512)
    .with_tool(tool.clone());
    let result = client.stream_completion(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(mut stream) => {
            let mut text = String::new();
            // Tool calls arrive sharded across chunks. Accumulate per `index`:
            // each chunk may carry an id, name, or a piece of the arguments
            // string. The reference client's accumulator does this; so does
            // ours, locally.
            let mut calls: std::collections::BTreeMap<u32, (String, String, String)> =
                std::collections::BTreeMap::new();
            while let Some(chunk) = match stream.next_chunk().await {
                Ok(c) => c,
                Err(e) => {
                    report.detail = redact(&format!("{e:?}"));
                    return report;
                }
            } {
                for choice in &chunk.choices {
                    if let Some(s) = &choice.delta.content {
                        text.push_str(s);
                    }
                    if let Some(deltas) = &choice.delta.tool_calls {
                        for d in deltas {
                            let entry = calls
                                .entry(d.index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(id) = &d.id {
                                entry.0 = id.clone();
                            }
                            if let Some(f) = &d.function {
                                if let Some(name) = &f.name {
                                    entry.1 = name.clone();
                                }
                                if let Some(args) = &f.arguments {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                    }
                }
            }
            if let Some((_, name, args)) = calls.into_values().next() {
                if !name.is_empty() {
                    report.tool = Some(ToolHit { name, args });
                } else if !text.is_empty() {
                    report.text = Some(text);
                } else {
                    report.detail = "empty stream".into();
                }
            } else if !text.is_empty() {
                report.text = Some(text);
            } else {
                report.detail = "empty stream".into();
            }
        }
        Err(e) => report.detail = redact(&format!("{e:?}")),
    }
    report
}

async fn responses_function(
    client: &Client,
    model: &str,
    tool: caocli_openai::ResponseTool,
) -> Report {
    let name = "responses / function tool / non-stream";
    let started = Instant::now();
    let request = ResponseCreateRequest::new(model, ResponseInput::text(weather_prompt()))
        .with_max_output_tokens(512)
        .with_tool(tool.clone());
    let result = client.responses(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(response) => {
            let mut text = String::new();
            let mut tool_hit: Option<ToolHit> = None;
            for item in &response.output {
                match item {
                    caocli_openai::ResponseOutputItem::Message(msg) => {
                        for part in &msg.content {
                            if let caocli_openai::OutputMessageContent::OutputText {
                                text: t, ..
                            } = part
                            {
                                text.push_str(t);
                            }
                        }
                    }
                    caocli_openai::ResponseOutputItem::FunctionToolCall(call) => {
                        tool_hit = Some(ToolHit {
                            name: call.name.clone(),
                            args: call.arguments.clone(),
                        });
                    }
                    _ => {}
                }
            }
            if let Some(hit) = tool_hit {
                report.tool = Some(hit);
            } else if !text.is_empty() {
                report.text = Some(text);
            } else {
                report.detail = "empty answer".into();
            }
        }
        Err(e) => report.detail = redact(&format!("{e:?}")),
    }
    report
}

async fn responses_streaming_function(
    client: &Client,
    model: &str,
    tool: caocli_openai::ResponseTool,
) -> Report {
    let name = "responses / function tool / stream";
    let started = Instant::now();
    let request = ResponseCreateRequest::streaming(model, ResponseInput::text(weather_prompt()))
        .with_max_output_tokens(512)
        .with_tool(tool);
    let result = client.stream_responses(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(mut stream) => {
            let mut text = String::new();
            // Tool-call arguments arrive as a series of function_call_arguments.delta
            // events keyed by item id. Aggregate per id, then take the first one
            // the model emitted.
            let mut calls: std::collections::BTreeMap<String, (String, String)> =
                std::collections::BTreeMap::new();
            let mut error_event: Option<String> = None;
            let mut event_count: usize = 0;
            while let Some(event) = match stream.next_event().await {
                Ok(e) => e,
                Err(e) => {
                    report.detail = redact(&format!("{e:?}"));
                    return report;
                }
            } {
                event_count += 1;
                match event {
                    caocli_openai::ResponseStreamEvent::OutputTextDelta { delta, .. } => {
                        text.push_str(&delta);
                    }
                    caocli_openai::ResponseStreamEvent::FunctionCallArgumentsDelta {
                        item_id,
                        delta,
                        ..
                    } => {
                        let entry = calls.entry(item_id).or_default();
                        entry.1.push_str(&delta);
                    }
                    caocli_openai::ResponseStreamEvent::OutputItemDone {
                        item: caocli_openai::ResponseOutputItem::FunctionToolCall(call),
                        ..
                    } => {
                        // The done event carries the full final tool call; if
                        // we already started collecting deltas for it, the
                        // name is set; otherwise the model emitted no
                        // arguments delta and we use the done values.
                        let key = call.id.clone().unwrap_or_default();
                        let entry = calls
                            .entry(key)
                            .or_insert_with(|| (call.name.clone(), String::new()));
                        if entry.0.is_empty() {
                            entry.0 = call.name.clone();
                        }
                        if entry.1.is_empty() {
                            entry.1 = call.arguments.clone();
                        }
                    }
                    caocli_openai::ResponseStreamEvent::Error { message, .. } => {
                        error_event = Some(message);
                    }
                    _ => {}
                }
            }
            if let Some(msg) = error_event {
                report.detail = format!("error event: {msg}");
                return report;
            }
            if let Some((name, args)) = calls.into_values().next() {
                if !name.is_empty() {
                    report.tool = Some(ToolHit { name, args });
                } else if !text.is_empty() {
                    report.text = Some(text);
                } else {
                    report.detail = format!("{event_count} events, no tool or text");
                }
            } else if !text.is_empty() {
                report.text = Some(text);
            } else {
                report.detail = format!("{event_count} events, no tool or text");
            }
        }
        Err(e) => report.detail = redact(&format!("{e:?}")),
    }
    report
}

fn preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= 60 {
        format!("{trimmed:?}")
    } else {
        let head: String = trimmed.chars().take(57).collect();
        format!("{head:?}…")
    }
}
