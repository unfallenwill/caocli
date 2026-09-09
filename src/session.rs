use anyhow::{Context, Result, bail};
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::types::{Message, Role};

// ============================================================================
// 会话存储：JSONL append-only。
// 第一行 header；每条消息一行 msg；meta 变更追加 meta 行（后行覆盖前行）。
// 文件永不重写；读取时跳过损坏行（进程崩溃写一半只影响尾部）。
// KVCache 依赖历史逐字节回放：Message 字段名与 API wire 格式一致，
// load 出来的 messages 直接原样进请求，无任何转换。
// 单写者假设：同一会话文件不允许两个进程同时追加（无文件锁）。
// 交错写入会产生无法自愈的非法历史（野 tool 结果），构建请求时会被
// is_request_valid tripwire 拦截（debug panic / release 由后端 400）。
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    id: String,
    created_at: i64,
    #[serde(flatten)]
    meta: SessionMeta,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Line {
    Header(Header),
    Msg {
        message: Message,
    },
    Meta {
        #[serde(flatten)]
        meta: SessionMeta,
    },
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    #[allow(dead_code)] // 会话创建时间（epoch 秒），未来用于列表展示与导出
    pub created_at: i64,
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    file: std::fs::File,
}

fn write_line<T: Serialize>(file: &mut std::fs::File, value: &T) -> Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    file.write_all(s.as_bytes())?;
    Ok(())
}

impl Session {
    pub fn create(dir: &Path, meta: SessionMeta) -> Result<Self> {
        let base = Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut id = base.clone();
        let mut n = 1u32;
        while dir.join(format!("{id}.jsonl")).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        let path = dir.join(format!("{id}.jsonl"));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("创建会话文件失败: {}", path.display()))?;
        let created_at = Utc::now().timestamp();
        write_line(
            &mut file,
            &Line::Header(Header {
                id: id.clone(),
                created_at,
                meta: meta.clone(),
            }),
        )?;
        Ok(Self {
            id,
            path,
            created_at,
            meta,
            messages: Vec::new(),
            file,
        })
    }

    /// 读取会话并打开追加句柄。损坏行跳过（仅尾部行可能因崩溃损坏）。
    pub fn load(path: &Path) -> Result<Self> {
        let data =
            std::fs::read(path).with_context(|| format!("读取会话文件失败: {}", path.display()))?;
        let mut id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut created_at = 0i64;
        let mut meta: Option<SessionMeta> = None;
        let mut messages = Vec::new();
        let mut bad_lines = 0usize;
        for raw in data.split(|&b| b == b'\n') {
            if raw.is_empty() {
                continue;
            }
            let s = String::from_utf8_lossy(raw);
            let s = s.trim_end_matches('\r');
            match serde_json::from_str::<Line>(s) {
                Ok(Line::Header(h)) => {
                    id = h.id;
                    created_at = h.created_at;
                    meta = Some(h.meta);
                }
                Ok(Line::Msg { message }) => messages.push(message),
                Ok(Line::Meta { meta: m }) => meta = Some(m),
                Err(_) => bad_lines += 1,
            }
        }
        let Some(meta) = meta else {
            bail!("会话文件缺少有效 header: {}", path.display());
        };
        if bad_lines > 0 {
            eprintln!("警告: {} 中有 {bad_lines} 行损坏已跳过", path.display());
        }
        // 崩溃自愈：为中断的工具调用合成占位结果，只改内存视图。
        // append-only：文件永不重写，下次 load 确定性地重新合成同样内容。
        let healed = crate::machine::heal(&mut messages);
        if healed > 0 {
            eprintln!(
                "警告: {} 有 {healed} 个中断的工具调用，已合成占位结果",
                path.display()
            );
        }
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("打开会话文件（追加模式）失败: {}", path.display()))?;
        Ok(Self {
            id,
            path: path.to_path_buf(),
            created_at,
            meta,
            messages,
            file,
        })
    }

    pub fn append_message(&mut self, m: &Message) -> Result<()> {
        write_line(&mut self.file, &Line::Msg { message: m.clone() })?;
        self.messages.push(m.clone());
        Ok(())
    }

    pub fn set_meta(&mut self, meta: SessionMeta) -> Result<()> {
        write_line(&mut self.file, &Line::Meta { meta: meta.clone() })?;
        self.meta = meta;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub modified: i64,
    pub message_count: usize,
    pub preview: String,
}

pub fn list(dir: &Path) -> Result<Vec<SessionInfo>> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("读取目录失败: {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        if let Ok(info) = summarize(&path) {
            out.push(info);
        }
    }
    out.sort_by_key(|i| std::cmp::Reverse(i.modified));
    Ok(out)
}

fn summarize(path: &Path) -> Result<SessionInfo> {
    let data = std::fs::read(path)?;
    let modified = std::fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut count = 0usize;
    let mut preview = String::from("(空会话)");
    for raw in data.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let Ok(line) = serde_json::from_str::<Line>(&String::from_utf8_lossy(raw)) else {
            continue; // list 模式静默跳过坏行
        };
        match line {
            Line::Header(h) => id = h.id,
            Line::Msg { message } => {
                if message.role == Role::User
                    && let Some(c) = &message.content
                {
                    preview = c.chars().take(40).collect();
                }
                count += 1;
            }
            Line::Meta { .. } => {}
        }
    }
    Ok(SessionInfo {
        id,
        path: path.to_path_buf(),
        modified,
        message_count: count,
        preview,
    })
}

pub fn latest(dir: PathBuf) -> Result<Option<PathBuf>> {
    Ok(list(&dir)?.into_iter().next().map(|i| i.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_meta() -> SessionMeta {
        SessionMeta {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: Some("high".into()),
        }
    }

    #[test]
    fn create_append_load_roundtrip() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("第一问")).unwrap();
        s.append_message(&Message {
            role: Role::Assistant,
            content: Some("答".into()),
            reasoning_content: Some("推理过程".into()),
            tool_calls: Some(vec![crate::types::ToolCall {
                id: "call_1".into(),
                r#type: "function".into(),
                function: crate::types::ToolCallFunction {
                    name: "Bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                },
            }]),
            tool_call_id: None,
        })
        .unwrap();
        s.append_message(&Message::tool("call_1", "file.txt"))
            .unwrap();

        let loaded = Session::load(&s.path).unwrap();
        assert_eq!(loaded.id, s.id);
        assert_eq!(loaded.meta, test_meta());
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(
            loaded.messages[1].reasoning_content.as_deref(),
            Some("推理过程")
        );
        assert_eq!(loaded.messages[2].tool_call_id.as_deref(), Some("call_1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_tail_line_is_skipped() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("ok")).unwrap();
        // 模拟崩溃写一半
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        f.write_all(br#"{"t":"msg","message":{"role":"user","cont"#)
            .unwrap();
        drop(f);

        let loaded = Session::load(&s.path).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 崩溃现场：call_2 的结果因中断丢失。load 后内存视图被修复成合法历史，
    /// 但文件保持原样（append-only），修复是确定性的读取时视图。
    #[test]
    fn load_heals_orphan_tool_calls_in_memory_only() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("q")).unwrap();
        s.append_message(&crate::types::Message {
            role: Role::Assistant,
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: Some(vec![
                crate::types::ToolCall {
                    id: "call_1".into(),
                    r#type: "function".into(),
                    function: crate::types::ToolCallFunction {
                        name: "Bash".into(),
                        arguments: "{}".into(),
                    },
                },
                crate::types::ToolCall {
                    id: "call_2".into(),
                    r#type: "function".into(),
                    function: crate::types::ToolCallFunction {
                        name: "Read".into(),
                        arguments: "{}".into(),
                    },
                },
            ]),
            tool_call_id: None,
        })
        .unwrap();
        s.append_message(&Message::tool("call_1", "ok")).unwrap();
        s.append_message(&Message::user("下一问")).unwrap();

        let loaded = Session::load(&s.path).unwrap();
        let msgs = &loaded.messages;
        assert_eq!(msgs.len(), 5, "补插一条 call_2 的占位结果");
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(
            msgs[3].content.as_deref(),
            Some(crate::machine::INTERRUPTED_RESULT)
        );
        // 文件没有被重写：仍是 header + 4 行消息
        let raw = std::fs::read_to_string(&s.path).unwrap();
        assert_eq!(raw.lines().count(), 5);
        // 修复后的历史对决策函数合法：可以直接发请求
        assert_eq!(
            crate::machine::next_action(msgs),
            Some(crate::machine::Action::CallModel)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn meta_line_overrides_header() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.set_meta(SessionMeta {
            model: "deepseek-v4-pro".into(),
            reasoning_effort: Some("max".into()),
        })
        .unwrap();
        let loaded = Session::load(&s.path).unwrap();
        assert_eq!(loaded.meta.model, "deepseek-v4-pro");
        assert_eq!(loaded.meta.reasoning_effort.as_deref(), Some("max"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_orders_by_mtime_and_previews_last_user() {
        let dir = tmpdir();
        let mut s1 = Session::create(&dir, test_meta()).unwrap();
        s1.append_message(&Message::user("第一个会话的问题"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mut s2 = Session::create(&dir, test_meta()).unwrap();
        s2.append_message(&Message::user("第二个会话的问题"))
            .unwrap();

        let infos = list(&dir).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].id, s2.id); // mtime 最新在前
        assert_eq!(infos[0].preview, "第二个会话的问题");
        assert_eq!(infos[0].message_count, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_errors_when_header_missing() {
        let dir = tmpdir();
        let path = dir.join("orphan.jsonl");
        std::fs::write(
            &path,
            br#"{"t":"msg","message":{"role":"user","content":"hi"}}\n"#,
        )
        .unwrap();
        let err = match Session::load(&path) {
            Ok(_) => panic!("缺少 header 应当报错"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("缺少有效 header"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_skips_non_jsonl_and_unreadable() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("真实会话")).unwrap();
        // 非 jsonl：list 忽略
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        // 名为 .jsonl 的目录：summarize 读失败，list 静默跳过
        std::fs::create_dir(dir.join("broken.jsonl")).unwrap();

        let infos = list(&dir).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, s.id);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_tolerates_corrupt_lines_and_meta() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("长".repeat(60))).unwrap();
        s.set_meta(SessionMeta {
            model: "deepseek-v4-pro".into(),
            reasoning_effort: None,
        })
        .unwrap();
        // 追加一条崩溃写一半的坏行
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        f.write_all(br#"{"t":"msg","message":{"role":"use"#)
            .unwrap();
        drop(f);

        let infos = list(&dir).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].preview.chars().count(), 40); // 坏行/meta 不计消息数，preview 截断
        assert_eq!(infos[0].message_count, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn latest_returns_most_recent_or_none() {
        let dir = tmpdir();
        assert!(latest(dir.clone()).unwrap().is_none()); // 空目录
        let mut s1 = Session::create(&dir, test_meta()).unwrap();
        s1.append_message(&Message::user("旧")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mut s2 = Session::create(&dir, test_meta()).unwrap();
        s2.append_message(&Message::user("新")).unwrap();
        assert_eq!(latest(dir.clone()).unwrap().unwrap(), s2.path);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
