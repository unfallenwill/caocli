use serde_json::json;
use std::time::Duration;

use crate::types::{FunctionDef, ToolDef};

pub const NAME: &str = "run_shell";
pub const TIMEOUT_SECS: u64 = 120;
pub const MAX_OUTPUT: usize = 10 * 1024;

pub fn definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: NAME.into(),
            description: Some(
                "在本地机器上用 bash 执行一条 shell 命令，返回 exit code、stdout、stderr。\
                 用于查看文件、运行程序、修改系统状态等。超时 120 秒会被强制终止。"
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的 bash 命令" }
                },
                "required": ["command"]
            })),
        },
    }
}

/// 执行工具。永不返回 Err：坏参数、命令失败、超时都以文本形式作为 tool
/// 结果回传给模型，由模型决定下一步（重试/换方式/告知用户）。
pub async fn execute(args_json: &str) -> String {
    let command = match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(v) => v.get("command").and_then(|c| c.as_str()).map(str::to_owned),
        Err(e) => return format!("error: 参数不是合法 JSON: {e}"),
    };
    let Some(command) = command else {
        return "error: 缺少必填参数 command (string)".into();
    };

    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let output = match child {
        Err(e) => return format!("exit_code: 127\n--- stderr ---\n无法启动 bash: {e}"),
        Ok(child) => {
            match tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), child.wait_with_output())
                .await
            {
                Err(_) => {
                    return format!(
                        "exit_code: 124\ntimeout: 命令超过 {TIMEOUT_SECS}s 已被终止，输出丢失。\
                     请改用更快的命令或将长任务放入后台并轮询输出文件。"
                    );
                }
                Ok(Err(e)) => return format!("exit_code: -1\nerror: {e}"),
                Ok(Ok(out)) => out,
            }
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let (stdout, stdout_cut) = truncate(&String::from_utf8_lossy(&output.stdout), MAX_OUTPUT);
    let (stderr, stderr_cut) = truncate(&String::from_utf8_lossy(&output.stderr), MAX_OUTPUT);

    let mut s = format!("exit_code: {exit_code}\n--- stdout ---\n{stdout}");
    if stdout_cut {
        s.push_str("\n[stdout 已截断]");
    }
    s.push_str("\n--- stderr ---\n");
    s.push_str(&stderr);
    if stderr_cut {
        s.push_str("\n[stderr 已截断]");
    }
    s
}

/// 按字节上限截断，回退到 UTF-8 字符边界，避免截断多字节字符。
fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_owned(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "中文".repeat(4096); // 每个 UTF-8 中文 3 字节，共 24576 字节
        let (out, cut) = truncate(&s, 10);
        assert!(cut);
        // 10 字节处不是字符边界，回退到 9（= 3 个完整字符）
        assert_eq!(out, "中文中");
        let (out2, cut2) = truncate("short", 10);
        assert!(!cut2);
        assert_eq!(out2, "short");
    }

    #[tokio::test]
    async fn execute_bad_json_returns_error_text() {
        let out = execute("not json").await;
        assert!(out.starts_with("error: 参数不是合法 JSON"));
    }

    #[tokio::test]
    async fn execute_missing_command_returns_error_text() {
        let out = execute(r#"{"cmd":"ls"}"#).await;
        assert!(out.starts_with("error: 缺少必填参数 command"));
    }

    #[tokio::test]
    async fn execute_runs_command() {
        let out = execute(r#"{"command":"echo hello; echo err >&2; exit 3"}"#).await;
        assert!(out.contains("exit_code: 3"));
        assert!(out.contains("hello"));
        assert!(out.contains("err"));
    }
}
