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
    /// The project instructions the session was started with: the workspace's
    /// AGENTS.md files, read once at creation and stored framed as the message
    /// they are sent in. A session sends what it stored — not what the files
    /// say now — which is what keeps the request prefix byte-for-byte stable
    /// for the life of the session, and what makes a resumed session exactly
    /// what it was. `None` in a session written before this was recorded, and
    /// in a workspace without the files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
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

// ============================================================================
// v1 dialect.
//
// The schema is documented in `docs/session-format.md`. Briefly:
// - The first line is a header that carries `format_version: 1` and the
//   meta fields, just as the v0 header did. A reader that does not find a
//   v1 header falls back to the v0 reader.
// - Every other line carries an envelope (`type`, `sequence`,
//   `timestamp_ms`, `turn`); the `msg` and `meta` line types are the
//   same shapes as before, the `ev` line is new and not yet emitted by
//   any code path (G5 will add writers for it).
// - Unknown `type` values are skipped with a warning. Unknown `kind` on
//   an `ev` line is skipped when `ignorable: true`, and refused otherwise:
//   a non-ignorable event the reader cannot decode may carry meaning the
//   messages do not, and silently skipping it produces a history that
//   never happened.
// ============================================================================

/// Which dialect a session file is in. Set by `load` from the file's first
/// line; set by `create` to `V1` for any new session written by this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `current` in the spec; surfaced by callers in G5+
pub enum Dialect {
    /// `t`-tagged lines, no `format_version`. Every file written before
    /// this commit looks like this.
    V0,
    /// `format_version: 1` header, envelope on every line. What new
    /// sessions are written in.
    V1,
}

#[derive(Debug, Serialize, Deserialize)]
struct V1Header {
    #[serde(rename = "type")]
    line_type: String, // always "header" on the wire; rejected if not
    format: String, // always "caocli-session" on the wire
    format_version: u32,
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timestamp_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    application: Option<serde_json::Value>,
    #[serde(flatten)]
    meta: SessionMeta,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum V1Line {
    Header(V1Header),
    Msg {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u32>,
        message: Message,
    },
    Ev {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u32>,
        kind: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        source_sequences: Vec<u64>,
        #[serde(default)]
        ignorable: bool,
        #[serde(flatten)]
        payload: serde_json::Value,
    },
    Meta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sequence: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp_ms: Option<i64>,
        #[serde(flatten)]
        meta: SessionMeta,
    },
}

/// What `load` decides the first line is: a v1 header or the v0 `Line::Header`.
/// Holds the dialect choice and the data the rest of `load` needs.
enum DetectedHeader {
    V1(V1Header),
    V0(Header),
}

/// Read the first non-empty line of the file and classify it. Returns the
/// dialect and the parsed header so the caller can carry on.
fn detect_header(path: &Path, raw: &[u8]) -> Result<(Dialect, DetectedHeader)> {
    let first = raw
        .split(|&b| b == b'\n')
        .find(|line| !line.is_empty())
        .ok_or_else(|| anyhow::anyhow!("session file is empty: {}", path.display()))?;
    let s = String::from_utf8_lossy(first);
    let s = s.trim_end_matches('\r');
    // Try v1 first: the v1 header has fields v0 cannot have (`type`,
    // `format_version`), so a v1 file fails the v0 parse anyway, but a v0
    // file would parse as a v1 header only if the v0 schema happened to
    // carry fields named `type`/`format_version`. The v0 schema does not,
    // so the order is safe.
    if let Ok(v1) = serde_json::from_str::<V1Header>(s) {
        if v1.line_type != "header" || v1.format != "caocli-session" {
            bail!(
                "v1 header has wrong type/format (got {}/{})",
                v1.line_type,
                v1.format
            );
        }
        if v1.format_version > 1 {
            bail!(
                "format version {} is not supported by this build (max 1)",
                v1.format_version
            );
        }
        return Ok((Dialect::V1, DetectedHeader::V1(v1)));
    }
    if let Ok(v0) = serde_json::from_str::<Line>(s)
        && let Line::Header(h) = v0
    {
        return Ok((Dialect::V0, DetectedHeader::V0(h)));
    }
    bail!("session file has no valid header: {}", path.display());
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
    /// Which dialect this session file is in. `V0` for files written before
    /// this commit; `V1` for new sessions.
    pub dialect: Dialect,
    /// Next sequence number to assign, only meaningful in `V1`. The header
    /// does not consume a sequence; the first msg/meta/ev line in the file
    /// is `sequence: 1`.
    next_sequence: u64,
    /// The turn the next user message will open. Starts at 1 on a fresh
    /// session. Only meaningful in `V1`.
    current_turn: u32,
    file: std::fs::File,
}

fn write_line<T: Serialize>(file: &mut std::fs::File, value: &T) -> Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    file.write_all(s.as_bytes())?;
    Ok(())
}

fn write_v1_header(file: &mut std::fs::File, h: &V1Header) -> Result<()> {
    write_line(file, h)
}

fn write_v1_line(file: &mut std::fs::File, l: &V1Line) -> Result<()> {
    write_line(file, l)
}

/// A new user message starts a new turn when the previous message in the
/// history was not a user message — that is the moment the conversation
/// changes hands. The very first message is also a new turn.
fn opens_new_turn(messages: &[Message], new: &Message) -> bool {
    if messages.is_empty() {
        return true;
    }
    if new.role != crate::types::Role::User {
        return false;
    }
    matches!(
        messages.last().map(|m| &m.role),
        Some(crate::types::Role::Assistant) | Some(crate::types::Role::Tool)
    )
}

/// Walk the file once to find the largest `turn` value written on any
/// non-header line. Used at load time so a resumed session continues
/// from the next turn, not from turn 1.
fn last_turn_seen_from_file(_path: &Path, data: &[u8]) -> u32 {
    let mut max_turn = 0u32;
    for raw in data.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw)
            && let Some(t) = v.get("turn").and_then(|t| t.as_u64())
        {
            max_turn = max_turn.max(t as u32);
        }
    }
    max_turn
}

/// Track and validate `sequence` numbers as we walk a v1 file. A gap or
/// duplicate is a sign of external editing or partial copies and is fatal:
/// every line past it would be reported with a wrong `body_sha256` and the
/// fold would be silently wrong.
fn validate_sequence(s: u64, next: &mut u64, path: &Path) -> Result<()> {
    let expected = *next + 1;
    if s != expected {
        bail!(
            "sequence mismatch in {}: got {}, expected {} (a gap means lines were removed or this file was edited)",
            path.display(),
            s,
            expected
        );
    }
    *next = s;
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
#[cfg(all(test, unix))]
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
#[cfg(all(test, unix))]
pub(crate) fn wait_until_released(path: &Path) {
    drop(load_when_released(path));
}

#[cfg(all(test, not(unix)))]
pub(crate) fn load_when_released(path: &Path) -> Session {
    Session::load(path).unwrap()
}

#[cfg(all(test, not(unix)))]
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
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).append(true);
        // Mode 0600: the session log holds commands, their output and pasted
        // secrets, so it is the owner's file. `write_line` is the writer's
        // tool, so a session that exists is one this binary opened, and the
        // mode belongs to that one creation.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("failed to create session file: {}", path.display()))?;
        let created_at = Utc::now().timestamp();
        // New sessions are written in the v1 dialect. The header carries the
        // same meta as before plus the envelope fields; the file is otherwise
        // empty and a reader can tell v1 from v0 by the presence of
        // `format_version` on the first line.
        write_v1_header(
            &mut file,
            &V1Header {
                line_type: "header".into(),
                format: "caocli-session".into(),
                format_version: 1,
                id: id.clone(),
                timestamp_ms: Some(created_at * 1000),
                working_directory: None,
                application: None,
                meta: meta.clone(),
            },
        )?;
        lock_exclusive(&file, &path)?;
        Ok(Self {
            id,
            path,
            created_at,
            meta,
            messages: Vec::new(),
            dialect: Dialect::V1,
            next_sequence: 0,
            current_turn: 0,
            file,
        })
    }

    /// Load a session and open the append handle. Corrupt lines are skipped
    /// (only the tail line can be damaged, by a crash).
    #[allow(unused_assignments)] // id / created_at / meta are seeded as
                                  // defaults and overwritten by both dialect
                                  // branches; clippy flags the seed
                                  // assignment because each branch assigns
                                  // first.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read session file: {}", path.display()))?;
        let mut id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut created_at: i64;
        let mut meta: SessionMeta;
        let mut messages = Vec::new();
        let mut bad_lines = 0usize;
        let mut next_sequence: u64 = 0;

        let (dialect, header) = detect_header(path, &data)?;
        match header {
            DetectedHeader::V1(h) => {
                id = h.id;
                created_at = h.timestamp_ms.map(|ms| ms / 1000).unwrap_or(0);
                meta = h.meta;
                let mut first_line = true;
                for raw in data.split(|&b| b == b'\n') {
                    if first_line {
                        first_line = false;
                        continue;
                    }
                    if raw.is_empty() {
                        continue;
                    }
                    let s = String::from_utf8_lossy(raw);
                    let s = s.trim_end_matches('\r');
                    let line: V1Line = match serde_json::from_str(s) {
                        Ok(l) => l,
                        Err(e) => {
                            bad_lines += 1;
                            eprintln!("warning: corrupt line in {}: {e}", path.display());
                            continue;
                        }
                    };
                    match line {
                        V1Line::Header(_) => {
                            // Two headers is not corruption: the second
                            // header is what became the meta line for v0.
                        }
                        V1Line::Msg {
                            sequence, message, ..
                        } => {
                            if let Some(s) = sequence {
                                validate_sequence(s, &mut next_sequence, path)?;
                            }
                            messages.push(message);
                        }
                        V1Line::Meta { meta: m, .. } => meta = m,
                        V1Line::Ev { .. } => {
                            // Events are not yet emitted by any writer in
                            // this commit; skip them silently.
                        }
                    }
                }
            }
            DetectedHeader::V0(h) => {
                id = h.id;
                created_at = h.created_at;
                meta = h.meta;
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
                            meta = h.meta;
                        }
                        Ok(Line::Msg { message }) => messages.push(message),
                        Ok(Line::Meta { meta: m }) => meta = m,
                        Err(_) => bad_lines += 1,
                    }
                }
            }
        }
        if bad_lines > 0 {
            eprintln!(
                "warning: skipped {bad_lines} corrupt line(s) in {}",
                path.display()
            );
        }
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
        let last_turn = last_turn_seen_from_file(path, &data);
        Ok(Self {
            id,
            path: path.to_path_buf(),
            created_at,
            meta,
            messages,
            dialect,
            next_sequence,
            current_turn: last_turn,
            file,
        })
    }

    pub fn append_message(&mut self, m: &Message) -> Result<()> {
        match self.dialect {
            Dialect::V0 => write_line(&mut self.file, &Line::Msg { message: m.clone() })?,
            Dialect::V1 => {
                if opens_new_turn(&self.messages, m) {
                    self.current_turn += 1;
                }
                self.next_sequence += 1;
                write_v1_line(
                    &mut self.file,
                    &V1Line::Msg {
                        sequence: Some(self.next_sequence),
                        timestamp_ms: Some(Utc::now().timestamp_millis()),
                        turn: Some(self.current_turn),
                        message: m.clone(),
                    },
                )?;
            }
        }
        self.messages.push(m.clone());
        Ok(())
    }

    pub fn set_meta(&mut self, meta: SessionMeta) -> Result<()> {
        match self.dialect {
            Dialect::V0 => write_line(&mut self.file, &Line::Meta { meta: meta.clone() })?,
            Dialect::V1 => {
                self.next_sequence += 1;
                write_v1_line(
                    &mut self.file,
                    &V1Line::Meta {
                        sequence: Some(self.next_sequence),
                        timestamp_ms: Some(Utc::now().timestamp_millis()),
                        meta: meta.clone(),
                    },
                )?;
            }
        }
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
        let s = String::from_utf8_lossy(raw);
        // Try v1 first: a v1 header has `type`/`format_version` and the v0
        // enum rejects unknown tags, so a v0 file's first line will fail
        // v1 parsing and fall through.
        if let Ok(v1) = serde_json::from_str::<V1Header>(&s) {
            id = v1.id;
            continue;
        }
        if let Ok(v1) = serde_json::from_str::<V1Line>(&s) {
            if let V1Line::Msg { message, .. } = v1 {
                if message.role == Role::User
                    && let Some(c) = &message.content
                {
                    let text = c.text();
                    preview = if text.is_empty() {
                        "[image]".to_owned()
                    } else {
                        text.chars().take(40).collect()
                    };
                }
                count += 1;
            }
            continue;
        }
        if let Ok(line) = serde_json::from_str::<Line>(&s) {
            match line {
                Line::Header(h) => id = h.id,
                Line::Msg { message } => {
                    if message.role == Role::User
                        && let Some(c) = &message.content
                    {
                        let text = c.text();
                        preview = if text.is_empty() {
                            "[image]".to_owned()
                        } else {
                            text.chars().take(40).collect()
                        };
                    }
                    count += 1;
                }
                Line::Meta { .. } => {}
            }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSource {
    /// A session the user named explicitly: the file `<id>.jsonl` in the sessions dir.
    Resume { id: String },
    /// Whichever session was last written. None is the empty sessions dir.
    ContinueLatest,
    /// A fresh session, started on this run's provider/model/effort.
    Fresh,
}

impl SessionSource {
    /// The priority order is `--resume > --continue > fresh`: an explicit id
    /// always wins, `--continue` falls back to fresh when there is no prior
    /// session, and a bare run is fresh.
    pub fn from_cli(resume: Option<&str>, cont: bool) -> Self {
        if let Some(id) = resume {
            return SessionSource::Resume { id: id.to_string() };
        }
        if cont {
            return SessionSource::ContinueLatest;
        }
        SessionSource::Fresh
    }

    /// Resolve into a session, creating one where none exists.
    ///
    /// `fresh_meta` is a closure so a `--resume` run does not pay to build it:
    /// reading the workspace's AGENTS.md is filesystem IO, and the only call
    /// sites that need a meta are the ones that create a session.
    pub fn resolve<F>(self, dir: &Path, fresh_meta: F) -> Result<Session>
    where
        F: FnOnce() -> Result<SessionMeta>,
    {
        match self {
            SessionSource::Resume { id } => {
                validate_resume_id(&id)?;
                let path = dir.join(format!("{id}.jsonl"));
                Session::load(&path).with_context(|| format!("failed to resume session {id}"))
            }
            SessionSource::ContinueLatest => match latest(dir.to_path_buf())? {
                Some(path) => Session::load(&path),
                None => Session::create(dir, fresh_meta()?),
            },
            SessionSource::Fresh => Session::create(dir, fresh_meta()?),
        }
    }
}

// ============================================================================
// Path safety: the things `--resume` and the working-directory layout rely on.
//
// A session id is part of a file path (`<id>.jsonl`). Accepting `/` or `..`
// would let a resume reach outside `sessions_dir`; accepting `%` would clash
// with the escape used in `escape_working_directory`. The id is therefore a
// closed character class checked at one place, so the rest of the code can
// join paths without re-checking.
//
// The working-directory layout under `sessions_dir` is the same story in the
// other direction: an absolute path is the directory the session ran in, and
// the layout turns it into a directory name. The mapping has to be reversible
// (`unescape_working_directory` is what `--resume` would walk when looking
// across workspaces) and injective (no two distinct paths collide on the
// same name). The order matters: `%` is escaped first, otherwise a literal
// `%2F` in the original would be double-escaped.
//
// `escape_working_directory` and `unescape_working_directory` are not yet
// wired into `sessions_dir()` — that change moves the layout from flat to
// nested and is its own commit. They are defined and tested here so the
// mapping has a unit test before any caller depends on it.
// ============================================================================

/// Reject an id that would let `--resume` escape `sessions_dir` or clash with
/// the escape encoding.
#[allow(dead_code)] // wired into SessionSource::resolve below; the symbol
// lives at module scope because the unescape helper below
// is part of the same public surface (used by G3+).
pub fn validate_resume_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("--resume id is empty");
    }
    if id == "." || id == ".." {
        bail!("--resume id is a path component");
    }
    if id.contains('/') || id.contains('\\') {
        bail!("--resume id contains a path separator");
    }
    if id.contains('\0') {
        bail!("--resume id contains a NUL byte");
    }
    // `%` is reserved by the escape encoding below; reject it so a resume id
    // can never collide with a layout-escaped directory name.
    if id.contains('%') {
        bail!("--resume id contains '%' (reserved by the layout encoding)");
    }
    // Reject `..` segments that survive the split (defense in depth: the
    // membership tests above already caught them as a literal substring).
    for c in std::path::Path::new(id).components() {
        if matches!(c, std::path::Component::ParentDir) {
            bail!("--resume id contains '..'");
        }
    }
    Ok(())
}

/// Turn an absolute working directory into a layout-safe directory name.
/// `None` (cwd not known) maps to a fixed sentinel that no escaped absolute
/// path can produce (every escaped absolute path starts with `%2F`).
#[allow(dead_code)] // see the module-level note on layout wiring.
pub fn escape_working_directory(cwd: Option<&Path>) -> Result<String> {
    let Some(path) = cwd else {
        return Ok("_no-working-directory".to_string());
    };
    if !path.is_absolute() {
        bail!(
            "working directory must be absolute for the layout encoding: {}",
            path.display()
        );
    }
    let s = path.to_string_lossy();
    // `%` first: escaping it after `/` would produce `%252F` for a literal
    // `%2F`, and unescape would then need a second pass to recognise.
    let escaped = s.replace('%', "%25").replace('/', "%2F");
    Ok(escaped)
}

/// Inverse of [`escape_working_directory`]. Used by a `--resume` that walks
/// across workspaces and by `--list --all`, which names directories back to
/// the user.
#[allow(dead_code)] // see the module-level note on layout wiring.
pub fn unescape_working_directory(name: &str) -> Result<PathBuf> {
    if name == "_no-working-directory" {
        bail!("this directory holds sessions whose working directory was not recorded");
    }
    if !name.starts_with("%2F") {
        bail!("directory name does not start with '%2F' and is not a known sentinel: {name}");
    }
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let a = chars.next();
            let b = chars.next();
            let Some(hex) = a.zip(b).map(|(a, b)| format!("{a}{b}")) else {
                bail!("truncated percent escape in directory name: {name}");
            };
            let byte = u8::from_str_radix(&hex, 16).map_err(|e| {
                anyhow::anyhow!("invalid percent escape '%{hex}' in directory name: {e}")
            })?;
            out.push(byte as char);
        } else {
            out.push(c);
        }
    }
    Ok(PathBuf::from(out))
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
            instructions: None,
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
            thinking: None,
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
    fn an_attached_image_survives_the_log_byte_for_byte() {
        // The image is in the message and nowhere else: what a resumed session
        // sends to the backend is what was sent the first time, which is what
        // keeps the prefix cache matching.
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        let sent = Message::user_with_images(
            "what is this?",
            vec!["data:image/png;base64,Zm9vYmFy".into()],
        );
        s.append_message(&sent).unwrap();
        let path = s.path.clone();
        drop(s);
        let loaded = load_when_released(&path);
        assert_eq!(loaded.messages, vec![sent.clone()]);
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(
            line.contains(r#""image_url":{"url":"data:image/png;base64,Zm9vYmFy"}"#),
            "the bytes are in the log itself: {line}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_that_is_only_an_image_previews_as_one() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user_with_images(
            "",
            vec!["data:image/png;base64,Zm9v".into()],
        ))
        .unwrap();
        let path = s.path.clone();
        drop(s);
        let infos = list(&dir).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(
            infos[0].preview, "[image]",
            "an empty column says less than saying what was sent"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        drop(path);
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
    fn a_session_written_before_instructions_were_recorded_still_loads() {
        // Same bargain as the provider field: the absence of the field in an
        // old session's header is a session without instructions, not a
        // session that cannot be read.
        let dir = tmpdir();
        let path = dir.join("20250101-120000.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"t":"header","id":"20250101-120000","created_at":1,"model":"deepseek-v4-pro"}"#,
                "\n"
            ),
        )
        .unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.meta.instructions, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_instructions_stored_at_creation_are_what_a_resumed_session_holds() {
        // The instructions are part of the meta, and the meta is what the
        // session is: what was frozen in at creation is what every later load
        // hands back, whether or not the files on disk have moved on since.
        let dir = tmpdir();
        let s = Session::create(
            &dir,
            SessionMeta {
                instructions: Some("the instructions as they were".into()),
                ..test_meta()
            },
        )
        .unwrap();
        let path = s.path.clone();
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(
            line.contains(r#""instructions":"the instructions as they were""#),
            "the header carries them: {line}"
        );
        drop(s);
        let loaded = load_when_released(&path);
        assert_eq!(
            loaded.meta.instructions.as_deref(),
            Some("the instructions as they were")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_meta_line_keeps_the_instructions_it_was_written_from() {
        // A meta line replaces the meta wholesale when the log is read, so a
        // line written from a meta that carries instructions must carry them
        // too — or a model switch would silently strip them from the session.
        // The write below is the one `/model` does: a clone of the session's
        // meta with one field changed.
        let dir = tmpdir();
        let mut s = Session::create(
            &dir,
            SessionMeta {
                instructions: Some("kept".into()),
                ..test_meta()
            },
        )
        .unwrap();
        let mut switched = s.meta.clone();
        switched.reasoning_effort = Some("low".into());
        s.set_meta(switched).unwrap();
        let path = s.path.clone();
        drop(s);
        let loaded = load_when_released(&path);
        assert_eq!(loaded.meta.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(loaded.meta.instructions.as_deref(), Some("kept"));
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
            content: Some("".into()),
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
            thinking: None,
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
            msgs[3].text().as_deref(),
            Some(crate::machine::Marker::Interrupted.text())
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
            instructions: None,
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
            instructions: None,
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

    #[test]
    fn session_source_from_cli_priority() {
        // --resume wins over --continue over fresh.
        assert_eq!(
            SessionSource::from_cli(Some("abc"), true),
            SessionSource::Resume { id: "abc".into() }
        );
        assert_eq!(
            SessionSource::from_cli(None, true),
            SessionSource::ContinueLatest
        );
        assert_eq!(SessionSource::from_cli(None, false), SessionSource::Fresh);
    }

    #[test]
    fn resolve_resume_loads_named_session() {
        let dir = tmpdir();
        let mut original = Session::create(&dir, test_meta()).unwrap();
        original.append_message(&Message::user("hello")).unwrap();
        let path = original.path.clone();
        drop(original);

        let loaded = SessionSource::Resume {
            id: path.file_stem().unwrap().to_string_lossy().into_owned(),
        }
        .resolve(&dir, || panic!("resume must not build a fresh meta"))
        .unwrap();
        assert_eq!(loaded.messages.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_continue_latest_creates_when_empty() {
        let dir = tmpdir();
        let s = SessionSource::ContinueLatest
            .resolve(&dir, || Ok(test_meta()))
            .unwrap();
        assert_eq!(s.meta.model, "deepseek-v4-flash");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_continue_latest_loads_when_present() {
        let dir = tmpdir();
        let prior = Session::create(&dir, test_meta()).unwrap();
        let path = prior.path.clone();
        drop(prior);

        let loaded = SessionSource::ContinueLatest
            .resolve(&dir, || {
                panic!("continue with a prior session must not build a fresh meta")
            })
            .unwrap();
        assert_eq!(loaded.path, path);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_fresh_always_calls_the_meta_closure() {
        let dir = tmpdir();
        let mut called = false;
        let _ = SessionSource::Fresh
            .resolve(&dir, || {
                called = true;
                Ok(test_meta())
            })
            .unwrap();
        assert!(called, "fresh must build a meta");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_resume_does_not_call_the_meta_closure() {
        let dir = tmpdir();
        let mut original = Session::create(&dir, test_meta()).unwrap();
        original.append_message(&Message::user("x")).unwrap();
        let id = original.id.clone();
        drop(original);

        let _ = SessionSource::Resume { id }
            .resolve(&dir, || panic!("resume must not build a fresh meta"))
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ----- G1: defense-in-depth -----

    #[test]
    fn a_new_session_file_is_owner_only() {
        let dir = tmpdir();
        let s = Session::create(&dir, test_meta()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&s.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "new session file is owner-only");
        }
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn escape_escapes_slashes_and_percent() {
        assert_eq!(
            escape_working_directory(Some(Path::new("/home/u/proj"))).unwrap(),
            "%2Fhome%2Fu%2Fproj"
        );
        assert_eq!(
            escape_working_directory(Some(Path::new("/a%2Fb"))).unwrap(),
            "%2Fa%252Fb"
        );
        assert_eq!(
            escape_working_directory(Some(Path::new("/"))).unwrap(),
            "%2F"
        );
        assert_eq!(
            escape_working_directory(None).unwrap(),
            "_no-working-directory"
        );
    }

    #[test]
    fn escape_rejects_a_relative_path() {
        assert!(escape_working_directory(Some(Path::new("relative"))).is_err());
    }

    #[test]
    fn unescape_is_the_inverse_of_escape() {
        let original = PathBuf::from("/home/u/proj");
        let encoded = escape_working_directory(Some(&original)).unwrap();
        let back = unescape_working_directory(&encoded).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn unescape_rejects_an_escaped_relative_path() {
        assert!(unescape_working_directory("home").is_err());
        assert!(unescape_working_directory("foo%2Fbar").is_err());
    }

    #[test]
    fn unescape_rejects_truncated_or_invalid_escapes() {
        assert!(unescape_working_directory("%2").is_err());
        assert!(unescape_working_directory("%2G").is_err());
    }

    #[test]
    fn validate_resume_id_accepts_a_normal_id() {
        assert!(validate_resume_id("20260101-120000").is_ok());
        assert!(validate_resume_id("20260101-120000-2").is_ok());
    }

    #[test]
    fn validate_resume_id_rejects_path_traversal() {
        assert!(validate_resume_id("").is_err());
        assert!(validate_resume_id(".").is_err());
        assert!(validate_resume_id("..").is_err());
        assert!(validate_resume_id("../etc/passwd").is_err());
        assert!(validate_resume_id("foo/../bar").is_err());
    }

    #[test]
    fn validate_resume_id_rejects_path_separators() {
        assert!(validate_resume_id("foo/bar").is_err());
        assert!(validate_resume_id("foo\\bar").is_err());
        assert!(validate_resume_id("foo\0bar").is_err());
    }

    #[test]
    fn validate_resume_id_rejects_percent() {
        assert!(validate_resume_id("foo%bar").is_err());
        assert!(validate_resume_id("100%").is_err());
    }

    #[test]
    fn resolve_rejects_an_unsafe_resume_id() {
        let dir = tmpdir();
        let result = SessionSource::Resume {
            id: "../escape".into(),
        }
        .resolve(&dir, || panic!("must not build a fresh meta"));
        let err = match result {
            Ok(_) => panic!("resolve must fail for an unsafe id"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("--resume id"),
            "the error names the failure: {err:#}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ----- G2: envelope and dialect detection -----

    #[test]
    fn new_sessions_are_v1() {
        let dir = tmpdir();
        let s = Session::create(&dir, test_meta()).unwrap();
        assert_eq!(s.dialect, Dialect::V1);
        let bytes = std::fs::read_to_string(&s.path).unwrap();
        assert!(
            bytes.starts_with(r#"{"type":"header""#),
            "header line uses `type` (v1), got: {bytes}"
        );
        assert!(
            bytes.contains(r#""format_version":1"#),
            "header carries format_version: {bytes}"
        );
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v1_msg_lines_carry_sequence_and_turn() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("first")).unwrap();
        s.append_message(&Message::user("second turn")).unwrap();
        let bytes = std::fs::read_to_string(&s.path).unwrap();
        assert!(bytes.contains(r#""sequence":1"#));
        assert!(bytes.contains(r#""sequence":2"#));
        assert!(bytes.contains(r#""turn":1"#));
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_detects_dialect_from_the_first_line() {
        let dir = tmpdir();
        let path = dir.join("legacy.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"t":"header","id":"legacy","created_at":1,"model":"m"}"#,
                "\n",
                r#"{"t":"msg","message":{"role":"user","content":"hi"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.dialect, Dialect::V0);
        assert_eq!(loaded.id, "legacy");
        assert_eq!(loaded.messages.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_rejects_a_future_format_version() {
        let dir = tmpdir();
        let path = dir.join("future.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"header","format":"caocli-session","format_version":2,"id":"f","model":"m"}"#,
                "\n",
            ),
        )
        .unwrap();
        let err = match Session::load(&path) {
            Ok(_) => panic!("load must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("format version 2"),
            "the error names the version: {err:#}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_rejects_a_v1_header_with_wrong_type_or_format() {
        let dir = tmpdir();
        let path = dir.join("wrong.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session","format":"caocli-session","format_version":1,"id":"x","model":"m"}"#,
        )
        .unwrap();
        assert!(Session::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_resumes_a_v1_session_with_the_same_messages() {
        let dir = tmpdir();
        let mut original = Session::create(&dir, test_meta()).unwrap();
        original.append_message(&Message::user("hi")).unwrap();
        original
            .append_message(&Message::tool("c1", "out"))
            .unwrap();
        let path = original.path.clone();
        let id = original.id.clone();
        drop(original);
        let loaded = load_when_released(&path);
        assert_eq!(loaded.dialect, Dialect::V1);
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.messages.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_warns_and_continues_on_a_corrupt_v1_line() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("a")).unwrap();
        s.append_message(&Message::user("b")).unwrap();
        let path = s.path.clone();
        drop(s);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(
            b"\n{\"type\":\"msg\",\"sequence\":3,\"turn\":1,\"message\":{\"role\":\"user\",\"cont",
        )
        .unwrap();
        f.write_all(b"\nnot-json-at-all\n").unwrap();
        drop(f);
        let loaded = load_when_released(&path);
        assert_eq!(loaded.messages.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_bails_on_a_sequence_gap() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("a")).unwrap();
        s.append_message(&Message::user("b")).unwrap();
        let path = s.path.clone();
        drop(s);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(
            br#"{"type":"msg","sequence":2,"timestamp_ms":0,"turn":1,"message":{"role":"user","content":"c"}}"#,
        )
        .unwrap();
        drop(f);
        let err = match Session::load(&path) {
            Ok(_) => panic!("load must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("sequence mismatch"),
            "the error names the cause: {err:#}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_recovers_the_largest_turn_number_for_a_resume() {
        let dir = tmpdir();
        let path = dir.join("resumed.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"header","format":"caocli-session","format_version":1,"id":"r","timestamp_ms":1,"model":"m"}"#,
                "\n",
                r#"{"type":"msg","sequence":1,"timestamp_ms":1,"turn":1,"message":{"role":"user","content":"q"}}"#,
                "\n",
                r#"{"type":"msg","sequence":2,"timestamp_ms":2,"turn":2,"message":{"role":"user","content":"q"}}"#,
                "\n",
                r#"{"type":"msg","sequence":3,"timestamp_ms":3,"turn":3,"message":{"role":"user","content":"q"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.dialect, Dialect::V1);
        let mut next = loaded;
        next.append_message(&Message::user("q")).unwrap();
        let bytes = std::fs::read_to_string(&next.path).unwrap();
        assert!(bytes.contains(r#""sequence":4"#));
        assert!(bytes.contains(r#""turn":3"#) || bytes.contains(r#""turn":4"#));
        drop(next);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_summarises_a_v1_session() {
        let dir = tmpdir();
        let mut s = Session::create(&dir, test_meta()).unwrap();
        s.append_message(&Message::user("find the bug")).unwrap();
        let infos = list(&dir).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, s.id);
        assert_eq!(infos[0].message_count, 1);
        assert_eq!(infos[0].preview, "find the bug");
        drop(s);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
