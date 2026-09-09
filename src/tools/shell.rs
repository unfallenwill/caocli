use serde_json::json;
use std::time::Duration;

use super::truncate;
use crate::types::{FunctionDef, ToolDef};

pub const NAME: &str = "Bash";
pub const TIMEOUT_SECS: u64 = 120;

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

/// 执行 shell 命令。永不返回 Err：坏参数、命令失败、超时都以文本形式
/// 作为 tool 结果回传给模型。
pub async fn execute(args_json: &str) -> String {
    let command = match super::parse_args(args_json) {
        Ok(v) => v.get("command").and_then(|c| c.as_str()).map(str::to_owned),
        Err(e) => return e,
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
    let (stdout, stdout_cut) =
        truncate(&String::from_utf8_lossy(&output.stdout), super::MAX_OUTPUT);
    let (stderr, stderr_cut) =
        truncate(&String::from_utf8_lossy(&output.stderr), super::MAX_OUTPUT);

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn execute_truncates_stdout_and_stderr() {
        let out = execute(
            r#"{"command":"head -c 30000 /dev/zero | tr '\\0' 'a'; head -c 30000 /dev/zero | tr '\\0' 'b' >&2"}"#,
        )
        .await;
        assert!(out.contains("[stdout 已截断]"), "{out}");
        assert!(out.contains("[stderr 已截断]"), "{out}");
        assert!(
            out.len() < crate::tools::MAX_OUTPUT * 2 + 512,
            "输出应被截断"
        );
    }
}
