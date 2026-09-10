use anyhow::{Context, Result, bail};
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::types::{Message, Role};

#[cfg(unix)]
use std::os::fd::AsRawFd;

// ============================================================================
// Session storage: append-only JSONL.
// The first line is the header; each message is one msg line; a meta change
// appends a meta line (later lines override earlier ones).
// The file is never rewritten; corrupt lines are skipped when reading (a crash
// mid-write can only damage the tail).
// KVCache depends on byte-for-byte history replay: Message field names match the
// API wire format, so the messages returned by load go straight into the request
// with no conversion at all.
// Single-writer assumption: two processes must never append to the same session
// file at once (enforced with an exclusive flock, see lock_exclusive).
// Interleaved writes would produce invalid history that cannot be healed (stray
// tool results); constructing a request trips the is_request_valid tripwire
// (debug panic / release gets a 400 from the backend).
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Provider the model belongs to: the id from the preset table, which says
    /// the endpoint and the key to send it to. `None` in a session written
    /// before this was recorded — such a session runs on whichever provider the
    /// run selected, which is what every session did then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id, sent to the backend as it stands.
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
    #[allow(dead_code)]
    // The file this log is appended to. Nothing on the screen reads it any more:
    // the banner names the session by its id, which is the whole of what the path
    // said, and `--list` prints the path from the summary it builds itself. It is
    // kept because a session is its file -- what was written is what it is -- and
    // the tests that check what was written read it here.
    pub path: PathBuf,
    #[allow(dead_code)]
    // session creation time (epoch seconds), for future list display and export
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

/// Single-writer enforcement: take an exclusive flock on the session file
/// (non-blocking, held until the process exits).
/// Two processes appending to the same session at once would interleave into
/// invalid history that cannot be healed (stray tool results), so this rejects
/// it right at the entrance.
#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File, path: &Path) -> Result<()> {
    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(());
        }
        // A signal delivered mid-call is not a lock held somewhere else, and the
        // call is asking the same question either way.
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        bail!(
            "session {} is already in use by another process (single-writer constraint): {err}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &std::fs::File, _path: &Path) -> Result<()> {
    Ok(())
}

/// Open a session file that was just closed, waiting for the lock to go with
/// the handle.
///
/// `flock` locks belong to the open file description, and `fork` copies file
/// descriptions: a child process that has not reached `exec` yet is still holding
/// the lock its parent dropped, so for a moment the file looks busy with nothing
/// wrong with it. Any test that spawns a child can put another test in that
/// window (measured: a few in every hundred attempts against a thread spawning
/// `true` in a loop), and a test that reopens a session right away would fail on
/// luck alone.
///
/// A real single-writer conflict is permanent, which is why production never
/// waits: it reports.
#[cfg(test)]
pub(crate) fn load_when_released(path: &Path) -> Session {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match Session::load(path) {
            Ok(open) => return open,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => panic!("still locked after waiting {e:#}"),
        }
    }
}

/// The same wait, for a caller that has to go through a path it does not own
/// (a command that loads the session itself) and only needs the lock gone.
#[cfg(test)]
pub(crate) fn wait_until_released(path: &Path) {
    drop(load_when_released(path));
}

#[cfg(not(unix))]
#[cfg(test)]
pub(crate) fn load_when_released(path: &Path) -> Session {
    Session::load(path).unwrap()
}

#[cfg(not(unix))]
#[cfg(test)]
pub(crate) fn wait_until_released(path: &Path) {
    drop(load_when_released(path));
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
            .with_context(|| format!("failed to create session file: {}", path.display()))?;
        let created_at = Utc::now().timestamp();
        write_line(
            &mut file,
            &Line::Header(Header {
                id: id.clone(),
                created_at,
                meta: meta.clone(),
            }),
        )?;
        lock_exclusive(&file, &path)?;
        Ok(Self {
            id,
            path,
            created_at,
            meta,
            messages: Vec::new(),
            file,
        })
    }

    /// Load a session and open the append handle. Corrupt lines are skipped
    /// (only the tail line can be damaged, by a crash).
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read session file: {}", path.display()))?;
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
            bail!("session file has no valid header: {}", path.display());
        };
        if bad_lines > 0 {
            eprintln!(
                "warning: skipped {bad_lines} corrupt line(s) in {}",
                path.display()
            );
        }
        // Crash healing: synthesize placeholder results for interrupted tool
        // calls, in the in-memory view only.
        // append-only: the file is never rewritten, and the next load
        // deterministically synthesizes the same content again.
        let healed = crate::machine::heal(&mut messages);
        if healed > 0 {
            eprintln!(
                "warning: synthesized placeholder results for {healed} interrupted tool call(s) in {}",
                path.display()
            );
        }
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| {
                format!(
                    "failed to open session file in append mode: {}",
                    path.display()
                )
            })?;
        lock_exclusive(&file, path)?;
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
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory: {}", dir.display()))?;
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
    let mut preview = String::from("(empty session)");
    for raw in data.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let Ok(line) = serde_json::from_str::<Line>(&String::from_utf8_lossy(raw)) else {
            continue; // list mode silently skips corrupt lines
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
            provider: Some("deepseek".into()),
            model: "deepseek-v4-flash".into(),
            reasoning_effort: Some("high".into()),
        }
    }

    #[test]
    fn create_append_load_roundtrip() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("first question")).unwrap();
        s.append_message(&Message {
            role: Role::Assistant,
            content: Some("answer".into()),
            reasoning_content: Some("reasoning trace".into()),
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

        let path = s.path.clone();
        let id = s.id.clone();
        drop(s); // the flock is held by the live Session, so release it before reloading
        let loaded = load_when_released(&path);
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(
            loaded.messages[1].reasoning_content.as_deref(),
            Some("reasoning trace")
        );
        assert_eq!(loaded.messages[2].tool_call_id.as_deref(), Some("call_1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_header_names_the_provider_the_model_belongs_to() {
        let dir = tmpdir();
        let s = Session::create(&dir, test_meta()).unwrap();
        let line = std::fs::read_to_string(&s.path).unwrap();
        assert!(
            line.contains(r#""provider":"deepseek""#),
            "the header should carry it: {line}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_session_written_before_providers_were_recorded_still_loads() {
        // The field is optional because every session written before it existed
        // has no provider in it, and refusing to load one would strand the
        // sessions of whoever upgraded.
        let dir = tmpdir();
        let path = dir.join("20250101-120000.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"t":"header","id":"20250101-120000","created_at":1,"model":"deepseek-v4-pro","reasoning_effort":"high"}"#,
                "\n"
            ),
        )
        .unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.meta.provider, None);
        assert_eq!(loaded.meta.model, "deepseek-v4-pro");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_tail_line_is_skipped() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("ok")).unwrap();
        // simulate a crash mid-write
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        f.write_all(br#"{"t":"msg","message":{"role":"user","cont"#)
            .unwrap();
        drop(f);

        let path = s.path.clone();
        drop(s);
        let loaded = load_when_released(&path);
        assert_eq!(loaded.messages.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Crash site: call_2's result was lost to an interruption. After load the
    /// in-memory view is repaired into valid history, but the file stays exactly
    /// as it was (append-only) — the repair is a deterministic read-time view.
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
        s.append_message(&Message::user("next question")).unwrap();

        let path = s.path.clone();
        drop(s);
        let loaded = load_when_released(&path);
        let msgs = &loaded.messages;
        assert_eq!(
            msgs.len(),
            5,
            "one placeholder result is inserted for call_2"
        );
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(
            msgs[3].content.as_deref(),
            Some(crate::machine::INTERRUPTED_RESULT)
        );
        // the file was not rewritten: still header + 4 message lines
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 5);
        // the healed history is valid for the decision function: a request can
        // be sent right away
        assert_eq!(
            crate::machine::next_action(msgs),
            Some(crate::machine::Action::CallModel)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Single-writer enforcement: while the locking Session is alive, a second
    /// handle's load must be rejected.
    #[test]
    fn load_rejects_while_another_process_holds_the_lock() {
        let dir = tmpdir();
        let s = Session::create(&dir, test_meta()).unwrap();
        // Session does not implement Debug, so pull the error out with match
        let err = match Session::load(&s.path) {
            Err(e) => e,
            Ok(_) => panic!("load must fail while the lock is held"),
        };
        assert!(
            format!("{err:#}").contains("single-writer"),
            "the error should mention the single-writer constraint: {err:#}"
        );
        let path = s.path.clone();
        drop(s); // release the lock (it is also released automatically at process exit)
        // Getting it back at all is the assertion: the lock went with the
        // handle.
        wait_until_released(&path);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn meta_line_overrides_header() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.set_meta(SessionMeta {
            provider: Some("zai-coding-cn".into()),
            model: "deepseek-v4-pro".into(),
            reasoning_effort: Some("max".into()),
        })
        .unwrap();
        let path = s.path.clone();
        drop(s);
        let loaded = load_when_released(&path);
        assert_eq!(loaded.meta.model, "deepseek-v4-pro");
        assert_eq!(loaded.meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(loaded.meta.reasoning_effort.as_deref(), Some("max"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_orders_by_mtime_and_previews_last_user() {
        let dir = tmpdir();
        let mut s1 = Session::create(&dir, test_meta()).unwrap();
        s1.append_message(&Message::user("first session's question"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mut s2 = Session::create(&dir, test_meta()).unwrap();
        s2.append_message(&Message::user("second session's question"))
            .unwrap();

        let infos = list(&dir).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].id, s2.id); // most recent mtime first
        assert_eq!(infos[0].preview, "second session's question");
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
            Ok(_) => panic!("a missing header must be an error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no valid header"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_skips_non_jsonl_and_unreadable() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("real session")).unwrap();
        // not jsonl: list ignores it
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        // a directory named .jsonl: summarize cannot read it, list skips it silently
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
        s.append_message(&Message::user("x".repeat(60))).unwrap();
        s.set_meta(SessionMeta {
            provider: Some("deepseek".into()),
            model: "deepseek-v4-pro".into(),
            reasoning_effort: None,
        })
        .unwrap();
        // append a corrupt half-written line from a crash
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        f.write_all(br#"{"t":"msg","message":{"role":"use"#)
            .unwrap();
        drop(f);

        let infos = list(&dir).unwrap();
        assert_eq!(infos.len(), 1);
        // corrupt lines and meta do not count as messages, and preview is truncated
        assert_eq!(infos[0].preview.chars().count(), 40);
        assert_eq!(infos[0].message_count, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn latest_returns_most_recent_or_none() {
        let dir = tmpdir();
        assert!(latest(dir.clone()).unwrap().is_none()); // empty directory
        let mut s1 = Session::create(&dir, test_meta()).unwrap();
        s1.append_message(&Message::user("old")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mut s2 = Session::create(&dir, test_meta()).unwrap();
        s2.append_message(&Message::user("new")).unwrap();
        assert_eq!(latest(dir.clone()).unwrap().unwrap(), s2.path);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
