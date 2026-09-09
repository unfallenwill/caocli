use serde_json::json;
use std::path::Path;

use super::{MAX_FILE_BYTES, MAX_OUTPUT, parse_args, str_arg, truncate};
use crate::types::{FunctionDef, ToolDef};

pub const READ_NAME: &str = "Read";
pub const EDIT_NAME: &str = "Edit";
pub const WRITE_NAME: &str = "Write";

pub fn read_definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: READ_NAME.into(),
            description: Some(
                "读取本地文本文件的完整内容。修改文件前应先用本工具确认原文。\
                 超大文件会被截断，可用 run_shell 分段读取剩余部分。"
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "要读取的文件路径" }
                },
                "required": ["file_path"]
            })),
        },
    }
}

pub fn edit_definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: EDIT_NAME.into(),
            description: Some(
                "对已有文件做精确字符串替换（old_string -> new_string）。\
                 old_string 必须在文件中唯一出现，未找到或出现多次都会报错；\
                 报错时请用 Read 查看原文，补充上下文使其唯一后重试。"
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "要修改的文件路径" },
                    "old_string": { "type": "string", "description": "被替换的原文本，必须在文件中唯一出现" },
                    "new_string": { "type": "string", "description": "替换后的新文本；空字符串表示删除匹配内容" }
                },
                "required": ["file_path", "old_string", "new_string"]
            })),
        },
    }
}

pub fn write_definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: WRITE_NAME.into(),
            description: Some(
                "新建文件或整文件覆盖写入。父目录不存在会自动创建。\
                 已有文件会被完全覆盖；修改已有文件请优先用 Edit。"
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "要写入的文件路径" },
                    "content": { "type": "string", "description": "写入文件的完整内容" }
                },
                "required": ["file_path", "content"]
            })),
        },
    }
}

/// Read：返回文件全文；超 MAX_OUTPUT 截断并提示。
pub fn read(args_json: &str) -> String {
    let v = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let path = match str_arg(&v, "file_path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    match read_text(&path) {
        Err(e) => e,
        Ok(text) => {
            let (out, cut) = truncate(&text, MAX_OUTPUT);
            if cut {
                format!("{out}\n[已截断到 {MAX_OUTPUT} 字节；剩余内容请用 run_shell 分段读取]")
            } else {
                out
            }
        }
    }
}

/// Edit：唯一匹配才替换；写回采用 tmp+rename 原子替换。
pub fn edit(args_json: &str) -> String {
    let v = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let path = match str_arg(&v, "file_path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old = match str_arg(&v, "old_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let new = match str_arg(&v, "new_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    if old.is_empty() {
        return "error: old_string 不能为空".into();
    }
    let content = match read_text(&path) {
        Err(e) => return e,
        Ok(c) => c,
    };
    match content.matches(&old).count() {
        0 => return format!("error: old_string 在 {path} 中未找到。请先用 Read 确认原文"),
        n if n > 1 => {
            return format!(
                "error: old_string 在 {path} 中出现 {n} 次，不唯一。请补充上下文使其唯一后重试"
            );
        }
        _ => {}
    }
    let updated = content.replacen(&old, &new, 1);
    match atomic_write(Path::new(&path), &updated) {
        Ok(()) => format!("ok: 已替换 1 处，{path} 现有 {} 字节", updated.len()),
        Err(e) => format!("error: 写入 {path} 失败: {e}"),
    }
}

/// Write：新建或整文件覆盖；父目录自动创建。
pub fn write(args_json: &str) -> String {
    let v = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let path = match str_arg(&v, "file_path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match str_arg(&v, "content") {
        Ok(c) => c,
        Err(e) => return e,
    };
    if content.len() as u64 > MAX_FILE_BYTES {
        return format!(
            "error: content 大小 {} 字节，超过 {MAX_FILE_BYTES} 字节上限",
            content.len()
        );
    }
    let p = Path::new(&path);
    if let Some(parent) = p.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return format!("error: 创建目录 {} 失败: {e}", parent.display());
    }
    match atomic_write(p, &content) {
        Ok(()) => format!("ok: 已写入 {path}（{} 字节）", content.len()),
        Err(e) => format!("error: 写入 {path} 失败: {e}"),
    }
}

/// 读取 UTF-8 文本文件。所有失败都转成给模型看的错误文本。
fn read_text(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p).map_err(|e| format!("error: 无法访问 {path}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("error: {path} 是目录，不是文件"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "error: 文件 {path} 大小 {} 字节，超过 {MAX_FILE_BYTES} 字节上限；请用 run_shell 分段读取（如 sed -n '1,200p'）",
            meta.len()
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("error: 读取 {path} 失败: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("error: {path} 不是合法 UTF-8 文本"))
}

/// 同目录 tmp + rename，避免写到一半崩溃损坏原文件。
fn atomic_write(path: &Path, data: &str) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!(".{file_name}.caocli-tmp-{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-fs-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn args(path: &Path, extra: &str) -> String {
        format!(r#"{{"file_path":{:?}{extra}}}"#, path.to_string_lossy())
    }

    #[test]
    fn write_creates_file_and_parents() {
        let dir = tmpdir();
        let p = dir.join("a/b/c.txt");
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"hello\nworld"}}"#,
            p.to_string_lossy()
        ));
        assert!(out.starts_with("ok:"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\nworld");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_returns_content_and_errors_on_missing() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "内容").unwrap();
        assert_eq!(read(&args(&p, "")), "内容");
        let missing = read(&args(&dir.join("nope.txt"), ""));
        assert!(missing.contains("无法访问"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_truncates_large_file() {
        let dir = tmpdir();
        let p = dir.join("big.txt");
        std::fs::write(&p, "x".repeat(MAX_OUTPUT + 100)).unwrap();
        let out = read(&args(&p, ""));
        assert!(out.contains("[已截断到"), "应提示截断");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_replaces_unique_match() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "alpha beta gamma").unwrap();
        let out = edit(&args(&p, r#","old_string":"beta","new_string":"BETA""#));
        assert!(out.starts_with("ok:"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "alpha BETA gamma");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_errors_when_not_found() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "alpha").unwrap();
        let out = edit(&args(&p, r#","old_string":"zzz","new_string":"y""#));
        assert!(out.contains("未找到"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "alpha");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_errors_when_not_unique() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "aa bb aa").unwrap();
        let out = edit(&args(&p, r#","old_string":"aa","new_string":"cc""#));
        assert!(out.contains("出现 2 次"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "aa bb aa");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_rejects_empty_old_string() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "x").unwrap();
        let out = edit(&args(&p, r#","old_string":"","new_string":"y""#));
        assert!(out.contains("不能为空"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_deletes_when_new_string_empty() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "keep DELETE keep").unwrap();
        let out = edit(&args(&p, r#","old_string":"DELETE ","new_string":"""#));
        assert!(out.starts_with("ok:"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "keep keep");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_rejects_non_utf8() {
        let dir = tmpdir();
        let p = dir.join("bin");
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();
        let out = edit(&args(&p, r#","old_string":"a","new_string":"b""#));
        assert!(out.contains("不是合法 UTF-8"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_args_are_reported() {
        assert!(read("{}").contains("缺少必填参数 file_path"));
        assert!(edit(r#"{"file_path":"/x"}"#).contains("缺少必填参数 old_string"));
        assert!(write(r#"{"file_path":"/x"}"#).contains("缺少必填参数 content"));
    }

    #[test]
    fn bad_json_and_missing_args_across_tools() {
        for call in [
            read("not json"),
            edit("not json"),
            write("not json"),
            edit("{}"),                                     // 缺 file_path
            edit(r#"{"file_path":"/x","old_string":"a"}"#), // 缺 new_string
            write("{}"),                                    // 缺 file_path
        ] {
            assert!(call.starts_with("error:"), "{call}");
        }
    }

    #[test]
    fn read_rejects_directory_and_oversized_file() {
        let dir = tmpdir();
        // 目录：报"是目录"
        let out = read(&args(&dir, ""));
        assert!(out.contains("是目录"), "{out}");
        // 超过单文件上限
        let big = dir.join("huge.bin");
        std::fs::write(&big, vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        let out = read(&args(&big, ""));
        assert!(out.contains("超过"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_rejects_oversized_content() {
        let dir = tmpdir();
        let p = dir.join("too-big.txt");
        let content = "x".repeat(MAX_FILE_BYTES as usize + 1);
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"{content}"}}"#,
            p.to_string_lossy()
        ));
        assert!(out.contains("超过"), "{out}");
        assert!(!p.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_fails_when_parent_not_writable() {
        let dir = tmpdir();
        let ro = dir.join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
            let p = ro.join("sub/x.txt"); // 父目录只读，create_dir_all 失败
            let out = write(&format!(
                r#"{{"file_path":{:?},"content":"hi"}}"#,
                p.to_string_lossy()
            ));
            assert!(out.contains("创建目录"), "{out}");
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_and_write_report_atomic_write_failure() {
        let dir = tmpdir();
        let ro = dir.join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        let target = ro.join("f.txt");
        std::fs::write(&target, "内容").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
            // Edit：读成功、tmp 写入失败
            let out = edit(&args(&target, r#","old_string":"内容","new_string":"改""#));
            assert!(out.contains("写入"), "{out}");
            // Write：tmp 写入失败
            let out = write(&format!(
                r#"{{"file_path":{:?},"content":"overwrite"}}"#,
                target.to_string_lossy()
            ));
            assert!(out.contains("写入"), "{out}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "内容"); // 原文件未被破坏
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
