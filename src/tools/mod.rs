mod fs;
mod shell;

use crate::types::ToolDef;

/// 工具输出回传上限（字节）。
pub const MAX_OUTPUT: usize = 10 * 1024;
/// 单文件读写上限（字节），防止把超大文件读进内存或上下文。
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// 全部工具定义。顺序固定：顺序变化会改变请求前缀，导致 KVCache 全量 miss。
pub fn definitions() -> Vec<ToolDef> {
    vec![
        shell::definition(),
        fs::read_definition(),
        fs::edit_definition(),
        fs::write_definition(),
    ]
}

/// 按名字分派执行。永不返回 Err：一切错误（未知工具、坏参数、IO 失败）
/// 都以文本形式作为 tool 结果回传给模型，由模型决定下一步。
pub async fn execute(name: &str, args_json: &str) -> String {
    match name {
        shell::NAME => shell::execute(args_json).await,
        fs::READ_NAME => fs::read(args_json),
        fs::EDIT_NAME => fs::edit(args_json),
        fs::WRITE_NAME => fs::write(args_json),
        other => format!(
            "error: 未知工具 {other:?}。可用工具: Bash, {}, {}, {}",
            fs::READ_NAME,
            fs::EDIT_NAME,
            fs::WRITE_NAME
        ),
    }
}

/// 解析工具参数 JSON。错误文本直接作为 tool 结果返回。
fn parse_args(args_json: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(args_json).map_err(|e| format!("error: 参数不是合法 JSON: {e}"))
}

/// 取必填字符串参数。
fn str_arg(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("error: 缺少必填参数 {key} (string)"))
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

    #[test]
    fn definitions_are_stable_and_named() {
        let names: Vec<String> = definitions().into_iter().map(|d| d.function.name).collect();
        assert_eq!(names, vec!["Bash", "Read", "Edit", "Write"]);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_text() {
        let out = execute("Delete", "{}").await;
        assert!(out.contains("未知工具"));
        assert!(out.contains("Bash"));
    }

    #[tokio::test]
    async fn dispatch_reaches_shell() {
        let out = execute("Bash", r#"{"command":"echo dispatched"}"#).await;
        assert!(out.contains("dispatched"));
    }
}
