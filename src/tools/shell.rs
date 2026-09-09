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
                "在本地机器上执行一条 bash 命令，返回 exit_code、stdout、stderr（三者分开返回）。\
                 每次调用都是全新的 shell：工作目录和环境变量不会保留，\
                 需要特定目录时请在同一条命令里写 cd /abs/path && ...，并优先用绝对路径。\
                 适用：运行程序、构建、测试、git、目录操作、批量文本处理。\
                 不适用：读文本文件用 Read，改已有文件用 Edit，新建或整文件重写用 Write；\
                 不要用 cat/sed -i/tee 代替它们。\
                 stdout/stderr 各超过 10240 字节会被截断并标记 [已截断]，\
                 请用 head/tail/grep/wc 主动收窄输出。\
                 不要执行交互式或常驻命令（vim、top、裸 read 等），会阻塞至 120 秒超时被杀且输出丢失。"
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
