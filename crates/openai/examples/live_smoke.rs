//! A live smoke test against a real endpoint.
//!
//! Run with:
//!
//! ```text
//! OPENAI_LIVE_KEY=sk-… OPENAI_LIVE_BASE=https://api.minimax.cn/v1 \
//!     OPENAI_LIVE_MODEL=MiniMax-M3 \
//!     cargo run -p caocli-openai --example live_smoke
//! ```
//!
//! The example reads the key from the environment and never echoes it back:
//! a 4xx body that names the key would be redacted before printing, because
//! a key in a log is a key leaked. This program is not part of `cargo test`;
//! it depends on a live endpoint and a credential the caller provides at
//! run time.
//!
//! What it exercises:
//!
//! 1. **Chat Completions, non-streaming** — one round-trip, prints the
//!    assistant text and the token usage the endpoint reports.
//! 2. **Chat Completions, streaming** — counts the chunks, prints the first
//!    piece of text the model writes.
//! 3. **Responses, non-streaming** — one round-trip, prints the output text
//!    and the status.
//! 4. **Responses, streaming** — counts the events, prints the first piece
//!    of text the model writes.
//!
//! Each section prints `OK` or `FAIL` with a one-line reason. A failure in
//! one section does not stop the others — the goal is to see which surfaces
//! the endpoint supports.

use std::env;
use std::time::{Duration, Instant};

use caocli_openai::{
    ChatCompletionMessageParam, ChatCompletionRequest, Client, Profile, ResponseCreateRequest,
    ResponseInput,
};

/// The env var the bearer token is read from.
const KEY_ENV: &str = "OPENAI_LIVE_KEY";

/// The env var the base URL is read from. The default is the endpoint the
/// user named; override for a different gateway.
const BASE_ENV: &str = "OPENAI_LIVE_BASE";

/// The env var the model name is read from. The default is the model caocli
/// itself uses for this endpoint.
const MODEL_ENV: &str = "OPENAI_LIVE_MODEL";

/// Redact a bearer token from a body string. A key in a log is a key leaked.
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

/// A section's outcome.
#[derive(Default)]
struct Report {
    name: &'static str,
    passed: bool,
    detail: String,
    elapsed: Duration,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let key = match env::var(KEY_ENV) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("OPENAI_LIVE_KEY is not set; refusing to run a live test without a key.");
            std::process::exit(2);
        }
    };
    let base = env::var(BASE_ENV).unwrap_or_else(|_| "https://api.minimax.cn/v1".to_string());
    let model = env::var(MODEL_ENV).unwrap_or_else(|_| "MiniMax-M3".to_string());

    // The profile points at the API root, so both Chat Completions and
    // Responses methods on the same client land on the right path. We never
    // print the key.
    let profile = Profile::base(&base).with_bearer_token(&key);
    let client = match Client::new(profile) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to build client: {}", redact(&e.to_string()));
            std::process::exit(2);
        }
    };

    println!("# live smoke");
    println!("base:  {}", base);
    println!("model: {}", model);
    println!("key:   <redacted, len={}>", key.len());
    println!();

    let mut reports = Vec::new();
    reports.push(chat_completions_non_streaming(&client, &model).await);
    reports.push(chat_completions_streaming(&client, &model).await);
    reports.push(responses_non_streaming(&client, &model).await);
    reports.push(responses_streaming(&client, &model).await);

    println!();
    println!("# summary");
    let mut all_ok = true;
    for r in &reports {
        let status = if r.passed { "OK  " } else { "FAIL" };
        println!(
            "{status}  {:>6.2}s  {:<40}  {}",
            r.elapsed.as_secs_f64(),
            r.name,
            r.detail
        );
        if !r.passed {
            all_ok = false;
        }
    }

    if !all_ok {
        std::process::exit(1);
    }
}

async fn chat_completions_non_streaming(client: &Client, model: &str) -> Report {
    let name = "chat completions / non-streaming";
    let started = Instant::now();
    let request = ChatCompletionRequest::new(
        model,
        vec![ChatCompletionMessageParam::user(
            "Reply with the single word 'pong' and nothing else.",
        )],
    )
    .with_max_tokens(64);
    let result = client.completion(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(completion) => {
            let text = completion
                .choices
                .first()
                .and_then(|c| c.message.content.as_deref())
                .unwrap_or("");
            let usage = completion
                .usage
                .as_ref()
                .map(|u| {
                    format!(
                        "prompt={} completion={} total={}",
                        u.prompt_tokens, u.completion_tokens, u.total_tokens
                    )
                })
                .unwrap_or_else(|| "usage: <missing>".into());
            let preview = preview_text(text);
            report.passed = !text.is_empty();
            report.detail = format!("{usage}; text={preview}");
        }
        Err(e) => {
            report.passed = false;
            report.detail = redact(&format!("{e:?}"));
        }
    }
    report
}

async fn chat_completions_streaming(client: &Client, model: &str) -> Report {
    let name = "chat completions / streaming";
    let started = Instant::now();
    let request = ChatCompletionRequest::streaming(
        model,
        vec![ChatCompletionMessageParam::user(
            "Reply with the single word 'pong' and nothing else.",
        )],
    )
    .with_max_tokens(64);
    let result = client.stream_completion(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(mut stream) => {
            let mut chunks = 0usize;
            let mut text = String::new();
            let mut finish_reason = None;
            while let Some(chunk) = match stream.next_chunk().await {
                Ok(c) => c,
                Err(e) => {
                    report.passed = false;
                    report.detail = redact(&format!("{e:?}"));
                    return report;
                }
            } {
                chunks += 1;
                for choice in &chunk.choices {
                    if let Some(s) = &choice.delta.content {
                        text.push_str(s);
                    }
                    if choice.finish_reason.is_some() && finish_reason.is_none() {
                        finish_reason = choice.finish_reason.clone();
                    }
                }
            }
            report.passed = !text.is_empty();
            report.detail = format!(
                "chunks={chunks} text={} finish_reason={}",
                preview_text(&text),
                finish_reason
                    .as_ref()
                    .map(|r| r.as_str().to_string())
                    .unwrap_or_else(|| "<none>".into())
            );
        }
        Err(e) => {
            report.passed = false;
            report.detail = redact(&format!("{e:?}"));
        }
    }
    report
}

async fn responses_non_streaming(client: &Client, model: &str) -> Report {
    let name = "responses / non-streaming";
    let started = Instant::now();
    let request = ResponseCreateRequest::new(
        model,
        ResponseInput::text("Reply with the single word 'pong' and nothing else."),
    )
    .with_max_output_tokens(64);
    let result = client.responses(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(response) => {
            let text = response
                .output
                .iter()
                .flat_map(|item| match item {
                    caocli_openai::ResponseOutputItem::Message(msg) => msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            caocli_openai::OutputMessageContent::OutputText { text, .. } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>()
                .join("");
            let status = response
                .status
                .as_ref()
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "<none>".into());
            report.passed = !text.is_empty();
            report.detail = format!("status={status} text={}", preview_text(&text));
        }
        Err(e) => {
            report.passed = false;
            report.detail = redact(&format!("{e:?}"));
        }
    }
    report
}

async fn responses_streaming(client: &Client, model: &str) -> Report {
    let name = "responses / streaming";
    let started = Instant::now();
    let request = ResponseCreateRequest::streaming(
        model,
        ResponseInput::text("Reply with the single word 'pong' and nothing else."),
    )
    .with_max_output_tokens(64);
    let result = client.stream_responses(&request).await;
    let elapsed = started.elapsed();
    let mut report = Report {
        name,
        elapsed,
        ..Default::default()
    };
    match result {
        Ok(mut stream) => {
            let mut events = 0usize;
            let mut text = String::new();
            let mut status = None;
            while let Some(event) = match stream.next_event().await {
                Ok(e) => e,
                Err(e) => {
                    report.passed = false;
                    report.detail = redact(&format!("{e:?}"));
                    return report;
                }
            } {
                events += 1;
                if let caocli_openai::ResponseStreamEvent::OutputTextDelta { delta, .. } = &event {
                    text.push_str(delta);
                }
                if let caocli_openai::ResponseStreamEvent::Completed { response, .. } = &event {
                    status = response
                        .status
                        .as_ref()
                        .map(|s| s.as_str().to_string())
                        .or_else(|| Some("completed".into()));
                }
            }
            report.passed = !text.is_empty();
            report.detail = format!(
                "events={events} text={} status={}",
                preview_text(&text),
                status.unwrap_or_else(|| "<none>".into())
            );
        }
        Err(e) => {
            report.passed = false;
            report.detail = redact(&format!("{e:?}"));
        }
    }
    report
}

fn preview_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= 60 {
        format!("{trimmed:?}")
    } else {
        let head: String = trimmed.chars().take(57).collect();
        format!("{head:?}…")
    }
}
