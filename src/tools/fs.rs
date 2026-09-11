use serde_json::json;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{MAX_FILE_BYTES, MAX_OUTPUT, MAX_READ_BYTES, parse_args, str_arg};
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
                "Read a UTF-8 text file, one numbered row per line: the line number, a tab, \
                 then the line. The number and the tab are not part of the file content. \
                 Confirm the original text with this tool before modifying a file. \
                 Use offset (the 1-based line number to start at; default 1) and limit (how \
                 many lines to read; default to the end of the file) to page through a long \
                 file: it is read a page at a time, so a page of a huge file costs no more \
                 than a page of a small one. \
                 Output is capped at 10240 bytes, cut on a line boundary. The marker after the \
                 last row says which lines were shown, how many lines the file has, and the \
                 offset to read on from. A note says so when the page's lines end with CRLF \
                 or the file has no newline after its last line; a single line longer than the \
                 whole cap is shown cut and named, and the rest of it is read with Bash. \
                 A directory, a file that is not regular, a page that is not valid UTF-8, and \
                 a file over 256MB are reported as errors."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "path of the file to read" },
                    "offset": { "type": "integer", "description": "the 1-based line number to start reading at; default 1, the first line" },
                    "limit": { "type": "integer", "description": "how many lines to read; default all the way to the end" }
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
                "Make an exact string replacement in an existing file \
                 (old_string -> new_string). \
                 Cannot create a new file; use Write to create one or to rewrite a whole file. \
                 old_string must match the file content character for character, including \
                 indentation, tab-versus-space differences and line endings — one character off \
                 is reported as not found, so use Read to check the original when the \
                 indentation is uncertain. \
                 old_string must occur exactly once in the file (zero or multiple occurrences is \
                 an error); one call replaces one occurrence, so make several calls for several \
                 edits. old_string must not be empty; an empty new_string deletes the matched text."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "path of the file to modify" },
                    "old_string": { "type": "string", "description": "the original text to replace; must occur exactly once in the file" },
                    "new_string": { "type": "string", "description": "the replacement text; an empty string deletes the matched text" }
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
                "Create a file, or overwrite a whole file; missing parent directories are \
                 created automatically. \
                 Content is written in the line endings the file already uses -- a CRLF file \
                 stays CRLF and an LF one stays LF, and the call's own newlines are brought \
                 into them -- so a whole-file rewrite does not turn every line of a long file \
                 into a change. A file that does not exist yet, one whose endings are mixed, \
                 and one over 10MB are written exactly as they were sent. \
                 An existing file is overwritten completely and unrecoverably, so use Read to \
                 check it first; \
                 use this only to create a file or rewrite one wholesale, and use Edit for \
                 partial changes to an existing file. \
                 A content larger than 10MB is rejected."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "path of the file to write" },
                    "content": { "type": "string", "description": "the full contents to write" }
                },
                "required": ["file_path", "content"]
            })),
        },
    }
}

/// The chunk a paged read streams a file in: what bounds a read is the page it
/// keeps, not this buffer.
const CHUNK: usize = 64 * 1024;

/// Bytes held back from the output cap so the markers and the notes that follow
/// the last row always fit inside it: the longest of them is the cut-line
/// marker with the CRLF note under it.
const MARKER_BYTES: usize = 256;

/// Read: pages through a text file, one numbered line per row.
pub fn read(args_json: &str) -> String {
    let v = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let path = match str_arg(&v, "file_path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let offset = match int_arg(&v, "offset") {
        Ok(n) => n.unwrap_or(1),
        Err(e) => return e,
    };
    let limit = match int_arg(&v, "limit") {
        Ok(n) => n,
        Err(e) => return e,
    };
    if offset == 0 {
        return "error: offset is the 1-based line number to start at, so the first line is \
                offset=1"
            .into();
    }
    if limit == Some(0) {
        return "error: limit must be at least 1 line".into();
    }
    match page_file(&path, offset, limit) {
        Err(e) => e,
        // A file with no lines at all is answered before the offset is checked:
        // there is nowhere in it to start from.
        Ok(page) if page.total == 0 => "[empty file]".into(),
        Ok(page) if offset > page.total => {
            format!(
                "error: offset {offset} is past the end of {path} ({} lines)",
                page.total
            )
        }
        Ok(page) => render_page(&page),
    }
}

/// One page out of a file: the rows, and what the rows cannot say about the part
/// of the file the page is.
struct Page {
    /// The rows in order, as the file reads once it is split into lines, and
    /// without their numbers. A page of a file that has a line at all keeps at
    /// least one row: a page that kept none could not say where to read on.
    rows: Vec<String>,
    /// The 1-based number of the first row.
    first: usize,
    /// The number of lines in the whole file.
    total: usize,
    /// The last row is the head of a line longer than the whole page.
    cut: bool,
    /// A line the page shows ends with CRLF.
    crlf: bool,
    /// The last byte of the file is not a newline.
    no_final_newline: bool,
}

/// Read one page out of a file, in a single streaming pass: skip to the line the
/// call asked for, keep the rows the page has room for, and count every line of
/// the file on the way -- the count is what the marker after the last row is
/// read from.
///
/// What is held is the page, never the file, so a file far larger than the
/// output cap can be paged. The cap on a file's size is therefore about how long
/// a read may take, and not about how much memory it takes.
fn page_file(path: &str, offset: usize, limit: Option<usize>) -> Result<Page, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("error: cannot access {path}: {e}"))?;
    check_readable(path, &meta)?;
    if meta.len() > MAX_READ_BYTES {
        return Err(format!(
            "error: file {path} is {} bytes, over the {MAX_READ_BYTES}-byte limit; read a range \
             of it with Bash (e.g. sed -n '1,200p')",
            meta.len()
        ));
    }
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("error: failed to read {path}: {e}"))?;
    let mut pager = Pager::new(
        path,
        offset,
        limit.unwrap_or(usize::MAX),
        MAX_OUTPUT - MARKER_BYTES,
    );
    let mut chunk = vec![0u8; CHUNK];
    let mut last = None;
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|e| format!("error: failed to read {path}: {e}"))?;
        if n == 0 {
            break;
        }
        last = Some(chunk[n - 1]);
        pager.feed(&chunk[..n])?;
    }
    // A file whose last byte is not a newline ends in a line of its own, and
    // that line is finished here rather than by a newline that never came.
    if last.is_some_and(|b| b != b'\n') {
        pager.page.total += 1;
        pager.page.no_final_newline = true;
        pager.finish()?;
    }
    Ok(pager.page)
}

/// Refuse what cannot be read as a file, with the reason it cannot: a directory,
/// or anything that is not a regular file. A fifo has no end and reports no
/// size, so a read of one blocks forever, and a device can be endless in a way
/// that has nothing to do with how much memory a reader has.
fn check_readable(path: &str, meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.is_dir() {
        return Err(format!("error: {path} is a directory, not a file"));
    }
    if !meta.is_file() {
        return Err(format!(
            "error: {path} is not a regular file, so it cannot be read as text"
        ));
    }
    Ok(())
}

/// The state of one streaming pass: what the page has kept, and where in the
/// file the pass has got to.
struct Pager {
    /// The path, for the one failure that names a line rather than the file.
    path: String,
    page: Page,
    /// Lines the call asked for (`usize::MAX` when it did not say).
    wanted: usize,
    /// What a page may take, and what it has taken: a row costs its number, the
    /// tab, the newline that separates it from the row before, and its text.
    budget: usize,
    kept: usize,
    /// The line being read, while the page still has room for it.
    cur: Option<Vec<u8>>,
    /// The last byte of that line, which is what tells a CRLF line from an LF
    /// one however much of the line was kept.
    tail: Option<u8>,
    /// That line is longer than the room left for it.
    over: bool,
    /// The page is full: from here on the pass only counts lines.
    closed: bool,
    /// The 1-based number of the line being read.
    line: usize,
}

impl Pager {
    fn new(path: &str, first: usize, wanted: usize, budget: usize) -> Self {
        Pager {
            path: path.to_owned(),
            page: Page {
                rows: Vec::new(),
                first,
                total: 0,
                cut: false,
                crlf: false,
                no_final_newline: false,
            },
            wanted,
            budget,
            kept: 0,
            cur: None,
            tail: None,
            over: false,
            closed: false,
            line: 1,
        }
    }

    /// Take one chunk of the file: count the lines it holds, and collect the
    /// ones the page has room for.
    fn feed(&mut self, chunk: &[u8]) -> Result<(), String> {
        for (run, terminated) in runs(chunk) {
            if terminated {
                self.page.total += 1;
            }
            if self.cur.is_none() && !self.closed && self.line >= self.page.first {
                self.cur = Some(Vec::new());
                self.tail = None;
                self.over = false;
            }
            if let Some(bytes) = self.cur.as_mut() {
                if !self.over {
                    let cost = overhead(self.line, self.page.rows.is_empty());
                    let room = self.budget.saturating_sub(self.kept + cost);
                    let take = run.len().min(room.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&run[..take]);
                    self.over = take < run.len();
                }
                if let Some(&b) = run.last() {
                    self.tail = Some(b);
                }
            }
            if terminated && let Some(bytes) = self.cur.take() {
                self.keep(bytes)?;
            }
            if terminated {
                self.line += 1;
            }
        }
        Ok(())
    }

    /// The end of the file: the last line, if no newline ended it.
    fn finish(&mut self) -> Result<(), String> {
        if let Some(bytes) = self.cur.take() {
            self.keep(bytes)?;
        }
        Ok(())
    }

    /// What the page does with the line it has just read: keep it, keep its head
    /// and say the line is longer than a page, or end the page before it -- and
    /// then go on counting lines, since the marker names the file's size as well
    /// as where to read on from.
    fn keep(&mut self, mut bytes: Vec<u8>) -> Result<(), String> {
        // A line that ends in a carriage return is a CRLF line: the row is shown
        // without it -- a reader cannot see it -- and the note says what a
        // multi-line old_string has to carry.
        if self.tail == Some(b'\r') {
            self.page.crlf = true;
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        let cost = overhead(self.line, self.page.rows.is_empty());
        if !self.over && self.kept + cost + bytes.len() <= self.budget {
            let len = bytes.len();
            self.push(bytes, false)?;
            self.kept += cost + len;
            if self.page.rows.len() >= self.wanted {
                self.closed = true;
            }
        } else if self.page.rows.is_empty() {
            // The page's first line does not fit in a whole page: show the head
            // of it and say so, rather than a page that never advances.
            self.push(bytes, true)?;
            self.page.cut = true;
            self.closed = true;
        } else {
            self.closed = true;
        }
        Ok(())
    }

    /// Add one row to the page. What cannot be read as text is refused with its
    /// line number; a row cut out of a longer line backs off to a character
    /// boundary instead, since half a character there is the cut and not the
    /// file.
    fn push(&mut self, bytes: Vec<u8>, cut: bool) -> Result<(), String> {
        match String::from_utf8(bytes) {
            Ok(row) => self.page.rows.push(row),
            Err(e) if cut && e.utf8_error().error_len().is_none() => {
                let end = e.utf8_error().valid_up_to();
                let row = String::from_utf8(e.into_bytes()[..end].to_vec())
                    .expect("a prefix of valid UTF-8 is valid UTF-8");
                self.page.rows.push(row);
            }
            Err(_) => {
                return Err(format!(
                    "error: {} is not valid UTF-8 text at line {}",
                    self.path, self.line
                ));
            }
        }
        Ok(())
    }
}

/// Number the rows, and say what a reader of the page cannot see: that the last
/// row is the head of a longer line, which lines were shown and where to read on
/// from, and the two facts about a file's shape that decide whether an Edit will
/// match -- CRLF line endings, and a last line with no newline.
fn render_page(page: &Page) -> String {
    let mut out = String::new();
    for (i, row) in page.rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&(page.first + i).to_string());
        out.push('\t');
        out.push_str(row);
    }
    let last = page.first + page.rows.len() - 1;
    if page.cut {
        out.push_str(&format!(
            "\n[line {last} is longer than the {MAX_OUTPUT}-byte output limit; read the rest of \
             it with Bash]"
        ));
    } else if last < page.total {
        out.push_str(&format!(
            "\n[showing lines {}-{last} of {}; read on with offset={}]",
            page.first,
            page.total,
            last + 1
        ));
    } else if page.no_final_newline {
        out.push_str("\n[note: the file does not end with a newline]");
    }
    if page.crlf {
        out.push_str("\n[note: CRLF line endings; keep \\r\\n in a multi-line old_string]");
    }
    out
}

/// Split a chunk into the runs of one line's text and the newline that ends it.
/// A run with no newline after it is either the start of a line that continues
/// in the next chunk or the middle of one.
fn runs(chunk: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    chunk
        .split_inclusive(|&b| b == b'\n')
        .map(|piece| match piece.split_last() {
            Some((&b'\n', head)) => (head, true),
            _ => (piece, false),
        })
}

/// What a row costs besides its text: its number, the tab, and the newline that
/// separates it from the row before.
fn overhead(line: usize, first: bool) -> usize {
    digits(line) + 1 + usize::from(!first)
}

/// The digits a line number takes up.
fn digits(n: usize) -> usize {
    n.checked_ilog10().unwrap_or(0) as usize + 1
}

/// Fetch an optional non-negative integer argument. A negative, fractional or
/// non-numeric value is a bad field rather than a number of lines, and saying
/// so is what lets the model correct the call.
fn int_arg(v: &serde_json::Value, key: &str) -> Result<Option<usize>, String> {
    match v.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(x) => x
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| format!("error: {key} must be a non-negative whole number")),
    }
}

/// Edit: replaces only on a unique match; writes back via an atomic tmp+rename.
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
        return "error: old_string must not be empty".into();
    }
    let content = match read_text(&path) {
        Err(e) => return e,
        Ok(c) => c,
    };
    match content.matches(&old).count() {
        0 => {
            return format!(
                "error: old_string not found in {path}. Use Read to check the original text first"
            );
        }
        n if n > 1 => {
            return format!(
                "error: old_string occurs {n} times in {path}, so it is not unique. Add \
                 surrounding context to make it unique and retry"
            );
        }
        _ => {}
    }
    let target = match write_target(&path) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let updated = content.replacen(&old, &new, 1);
    match atomic_write(&target, &updated) {
        Ok(()) => format!(
            "ok: replaced 1 occurrence; {path} is now {} bytes",
            updated.len()
        ),
        Err(e) => format!("error: failed to write {path}: {e}"),
    }
}

/// Write: creates or overwrites a whole file; parent directories are created.
///
/// The write lands on the file the path finally names: a symlink is followed to
/// its target rather than replaced by a regular file, an overwrite keeps the
/// target's own permissions, and content that is already there is left alone.
/// The text lands in the target's own line endings, so that a whole-file rewrite
/// does not turn every line of a file into a change. Everything that can be
/// refused is refused before a byte is written, and a write that fails takes its
/// temporary file with it.
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
            "error: content is {} bytes, over the {MAX_FILE_BYTES} byte limit",
            content.len()
        );
    }
    let target = match write_target(&path) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return format!(
            "error: failed to create directory {}: {e}",
            parent.display()
        );
    }
    // The text that lands is the call's own, in the line endings of the file it
    // replaces: a whole-file rewrite sent in the other endings would otherwise
    // read, to whoever watches the file, as a change to every line of it.
    let kept = in_endings(&content, endings(&target));
    let data = match &kept {
        Some((text, _)) => text.as_str(),
        None => &content,
    };
    if already_holds(&target, data) {
        return format!(
            "ok: {path} already has exactly this content ({} bytes); left unchanged",
            data.len()
        );
    }
    match atomic_write(&target, data) {
        Ok(()) => {
            let ending = match kept {
                Some((_, endings)) => {
                    format!(
                        "; line endings converted to the file's own {}",
                        endings.name()
                    )
                }
                None => String::new(),
            };
            format!("ok: wrote {path} ({} bytes){ending}", data.len())
        }
        Err(e) => format!("error: failed to write {path}: {e}"),
    }
}

/// How the lines of a file end: the one thing a whole-file write carries over
/// from the file it replaces, since it replaces everything else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Endings {
    /// Every line ends with a newline, and no carriage return in front of it.
    Lf,
    /// Every line ends with a carriage return and then a newline.
    Crlf,
}

impl Endings {
    /// What these endings are called, for the one line that says what a write
    /// did to the text the call sent.
    fn name(self) -> &'static str {
        match self {
            Endings::Lf => "LF",
            Endings::Crlf => "CRLF",
        }
    }
}

/// The line endings of the file a write is about to replace, as far as reading
/// it can say: `Crlf` when every line in it ends with a carriage return and a
/// newline, `Lf` when no line does, and `None` -- no one style for a write to
/// follow, so the call's own text is what lands -- for a file with no line
/// ending to go by, one that mixes the two, one larger than a write may replace,
/// and one that cannot be read at all; the last of those is every write that
/// creates a file.
fn endings(path: &Path) -> Option<Endings> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut chunk = vec![0u8; CHUNK];
    let (mut crlf, mut lf) = (0usize, 0usize);
    // The byte before the one in hand, chunk boundaries included: whether a
    // newline ends a CRLF line is decided by what stands in front of it.
    let mut before = None;
    loop {
        let n = file.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        for &b in &chunk[..n] {
            if b == b'\n' {
                if before == Some(b'\r') {
                    crlf += 1;
                } else {
                    lf += 1;
                }
            }
            before = Some(b);
        }
        // With both kinds in hand the file has no one style to follow, and what
        // is left of it cannot give it one.
        if crlf > 0 && lf > 0 {
            return None;
        }
    }
    match (crlf, lf) {
        (0, 0) => None,
        (_, 0) => Some(Endings::Crlf),
        _ => Some(Endings::Lf),
    }
}

/// The text this write puts on disk, in the endings of the file it replaces --
/// `Some` when the call's own text had to be brought into them, with the endings
/// it was brought into. `None` when there is nothing to bring it into: the file
/// has no one style to follow, or the text is already in its style, and either
/// way the bytes the call sent are the bytes that land.
fn in_endings(content: &str, endings: Option<Endings>) -> Option<(String, Endings)> {
    let endings = endings?;
    match endings {
        Endings::Crlf => crlf_lines(content).map(|text| (text, Endings::Crlf)),
        Endings::Lf => lf_lines(content).map(|text| (text, Endings::Lf)),
    }
}

/// The text with a carriage return in front of every newline that has none --
/// what a CRLF file's own text has. `None` when every newline in it has one
/// already.
fn crlf_lines(content: &str) -> Option<String> {
    let bytes = content.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut added = false;
    for (i, &b) in bytes.iter().enumerate() {
        // A newline with nothing in front of it, or with something other than a
        // carriage return, is where a line ends in a file of LF lines.
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            out.push(b'\r');
            added = true;
        }
        out.push(b);
    }
    added.then(|| String::from_utf8(out).expect("a carriage return added to UTF-8 is UTF-8"))
}

/// The text without the carriage return in front of a newline -- what an LF
/// file's own text has. `None` when there is no CRLF line to take one off.
fn lf_lines(content: &str) -> Option<String> {
    let bytes = content.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut dropped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            dropped = true;
            continue;
        }
        out.push(b);
    }
    dropped.then(|| String::from_utf8(out).expect("a carriage return dropped from UTF-8 is UTF-8"))
}

/// The file a write lands on: the path with any symlink at its end followed, so
/// that writing through a link changes what the link points at instead of
/// replacing the link itself -- which is what a rename lands on.
///
/// A directory, and anything else that is not a regular file, is refused here,
/// before anything is created: a rename onto a path that cannot take a file is
/// how a temporary file used to be left behind.
fn write_target(path: &str) -> Result<PathBuf, String> {
    let mut target = PathBuf::from(path);
    // A chain longer than this is a loop, which is what the kernel calls it too.
    for _ in 0..40 {
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = std::fs::read_link(&target).map_err(|e| {
                    format!("error: cannot follow the link at {}: {e}", target.display())
                })?;
                target = if link.is_absolute() {
                    link
                } else {
                    // A relative link is relative to the directory the link sits
                    // in, not to the process's own directory.
                    target.parent().unwrap_or(Path::new(".")).join(link)
                };
            }
            Ok(meta) if meta.is_dir() => {
                return Err(format!("error: {path} is a directory, not a file"));
            }
            Ok(meta) if !meta.is_file() => {
                return Err(format!(
                    "error: {path} is not a regular file, so it cannot be written"
                ));
            }
            _ => return Ok(target),
        }
    }
    Err(format!("error: {path} is a loop of symbolic links"))
}

/// Whether the file already holds exactly this content. The size is read off
/// the metadata first: a target of another size cannot match, and the read of
/// one that could is bounded by the content's own size, so a file far larger
/// than the write cap is never pulled into memory to answer this.
fn already_holds(path: &Path, content: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() == content.len() as u64 => {
            std::fs::read(path).is_ok_and(|bytes| bytes == content.as_bytes())
        }
        _ => false,
    }
}

/// Read a whole file as text: Edit's reader, which has to hold all of a file to
/// change part of it, and so is capped where a paged read is not. Every failure
/// becomes error text for the model.
fn read_text(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p).map_err(|e| format!("error: cannot access {path}: {e}"))?;
    check_readable(path, &meta)?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "error: file {path} is {} bytes, over the {MAX_FILE_BYTES}-byte limit, too large to \
             edit in one piece",
            meta.len()
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("error: failed to read {path}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("error: {path} is not valid UTF-8 text"))
}

/// tmp file in the same directory + rename, so a crash mid-write cannot corrupt
/// the original file. The temp file is created fresh and exclusively, and the
/// mode of the file it is about to replace is copied onto it -- a rename hands
/// the target the temp file's own permissions otherwise -- and a write or a
/// rename that fails removes the temp file again, so a failed write leaves the
/// directory as it found it.
fn atomic_write(path: &Path, data: &str) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let (tmp, mut file) = temp_file(path, &file_name)?;
    let written = (|| -> std::io::Result<()> {
        file.write_all(data.as_bytes())?;
        if let Ok(meta) = std::fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        Ok(())
    })();
    // The handle is closed before the rename: the file is whole on disk by then,
    // and one that never landed is this call's to remove.
    drop(file);
    let result = written.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A fresh temp file beside the target, opened exclusively: the name is one
/// nothing else holds, and a name that is somehow taken -- a leftover from a
/// crashed run -- is an error rather than a write through whatever holds it.
fn temp_file(path: &Path, file_name: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_file_name(format!(
        ".{file_name}.caocli-tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    Ok((tmp, file))
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

    /// The offset the continuation marker names, or `None` when the page ended
    /// the file.
    fn read_on(out: &str) -> Option<usize> {
        let row = out.lines().find(|row| row.starts_with("[showing lines "))?;
        Some(
            row.rsplit_once("offset=")
                .expect("the marker says how to read on")
                .1
                .trim_end_matches(']')
                .parse()
                .expect("the marker's offset is a line number"),
        )
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
        std::fs::write(&p, "content\n").unwrap();
        assert_eq!(read(&args(&p, "")), "1\tcontent");
        let missing = read(&args(&dir.join("nope.txt"), ""));
        assert!(missing.contains("cannot access"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_numbers_lines_and_pages_with_offset_and_limit() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "a\nb\nc\nd\ne\n").unwrap();
        // The whole file: one numbered row per line, in order.
        assert_eq!(read(&args(&p, "")), "1\ta\n2\tb\n3\tc\n4\td\n5\te");
        // A null offset is the default, not a bad field.
        assert_eq!(
            read(&args(&p, r#","offset":null"#)),
            "1\ta\n2\tb\n3\tc\n4\td\n5\te"
        );
        // A page that stops short says where to read on from.
        assert_eq!(
            read(&args(&p, r#","offset":3,"limit":2"#)),
            "3\tc\n4\td\n[showing lines 3-4 of 5; read on with offset=5]"
        );
        // Starting at the last line ends the file, so there is nothing to add.
        assert_eq!(read(&args(&p, r#","offset":5"#)), "5\te");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_marks_a_truncated_page_and_reads_on() {
        let dir = tmpdir();
        let p = dir.join("big.txt");
        let lines: Vec<String> = (1..=2000).map(|i| format!("line number {i}")).collect();
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        // Page through to the end: every line is seen once, in order, and no
        // page goes over the cap.
        let mut offset = 1usize;
        let mut seen: Vec<String> = Vec::new();
        let mut pages = 0;
        loop {
            let out = read(&args(&p, &format!(r#","offset":{offset}"#)));
            assert!(out.len() <= MAX_OUTPUT, "page is {} bytes", out.len());
            for row in out.lines() {
                if row.starts_with('[') {
                    continue; // a marker or a note, not a row
                }
                let (number, text) = row.split_once('\t').expect("every row is numbered");
                let number = number
                    .trim()
                    .parse::<usize>()
                    .unwrap_or_else(|e| panic!("row {row:?}: {e}"));
                assert_eq!(number, seen.len() + 1);
                seen.push(text.to_owned());
            }
            match read_on(&out) {
                Some(n) => offset = n,
                None => break,
            }
            pages += 1;
            assert!(pages < 100, "paging is not advancing");
        }
        assert_eq!(seen, lines);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file far larger than the output cap is read a page at a time: the range
    /// a call asks for is found by streaming the file, and the page is the whole
    /// of what is held.
    #[test]
    fn read_pages_a_file_far_larger_than_the_output_cap() {
        let dir = tmpdir();
        let p = dir.join("big.txt");
        let mut text = String::new();
        let mut lines = 0usize;
        while text.len() <= MAX_FILE_BYTES as usize {
            lines += 1;
            text.push_str(&format!("line number {lines}\n"));
        }
        std::fs::write(&p, &text).unwrap();
        // The size Read used to refuse outright, answered with a page the cap
        // bounds -- and the page names the size of the file behind it.
        let out = read(&args(&p, ""));
        assert!(out.len() <= MAX_OUTPUT, "page is {} bytes", out.len());
        assert!(out.starts_with("1\tline number 1\n"), "{out}");
        assert!(
            out.contains(&format!("of {lines};")),
            "the page names the file's size"
        );
        // The page after it starts where the marker said, at that line.
        let next = read_on(&out).expect("a file this size takes more than one page");
        let page = read(&args(&p, &format!(r#","offset":{next}"#)));
        assert!(
            page.starts_with(&format!("{next}\tline number {next}")),
            "{page}"
        );
        // And a line near the end is reached in one call, wherever it is.
        let last = read(&args(&p, &format!(r#","offset":{lines}"#)));
        assert_eq!(last, format!("{lines}\tline number {lines}"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The cap on a read is about how long the scan behind a page may take, not
    /// about memory: a file over it is refused before a byte of it is read.
    #[test]
    fn read_refuses_a_file_over_the_read_limit() {
        let dir = tmpdir();
        let p = dir.join("sparse.txt");
        std::fs::File::create(&p)
            .unwrap()
            .set_len(MAX_READ_BYTES + 1)
            .unwrap();
        let out = read(&args(&p, ""));
        assert!(
            out.contains(&format!("over the {MAX_READ_BYTES}-byte limit")),
            "{out}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A fifo has no end and reports no size, and a device can be endless in a
    /// way that has nothing to do with how much memory a reader has: neither is
    /// opened, and a read of either is text the model can correct itself from.
    /// (The fifo is the case that hangs the test if the check goes.)
    #[test]
    fn read_refuses_a_file_that_is_not_regular() {
        let dir = tmpdir();
        let fifo = dir.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo is how the test makes a fifo");
        for p in [fifo.as_path(), Path::new("/dev/zero")] {
            let out = read(&args(p, ""));
            assert!(out.starts_with("error:"), "{out}");
            assert!(out.contains("not a regular file"), "{out}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The two facts about a file's shape that decide whether an Edit will match
    /// are said where they are known, and only while they hold.
    #[test]
    fn read_names_crlf_and_a_missing_final_newline() {
        let dir = tmpdir();
        let crlf = dir.join("crlf.txt");
        std::fs::write(&crlf, "a\r\nb\r\n").unwrap();
        assert_eq!(
            read(&args(&crlf, "")),
            "1\ta\n2\tb\n[note: CRLF line endings; keep \\r\\n in a multi-line old_string]"
        );
        let no_eol = dir.join("no-eol.txt");
        std::fs::write(&no_eol, "a\nb").unwrap();
        assert_eq!(
            read(&args(&no_eol, "")),
            "1\ta\n2\tb\n[note: the file does not end with a newline]"
        );
        // A page that stops before the end of the file has neither to say.
        assert_eq!(
            read(&args(&no_eol, r#","limit":1"#)),
            "1\ta\n[showing lines 1-1 of 2; read on with offset=2]"
        );
        // And a file that ends with a newline gets no note at all.
        let lf = dir.join("lf.txt");
        std::fs::write(&lf, "a\n").unwrap();
        assert_eq!(read(&args(&lf, "")), "1\ta");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The notes ride inside the cap as well: the longest pair of them -- a cut
    /// line in a CRLF file -- still leaves a page no larger than the cap.
    #[test]
    fn read_keeps_its_notes_inside_the_output_cap() {
        let dir = tmpdir();
        let p = dir.join("cut-crlf.txt");
        std::fs::write(&p, format!("{}\r\n", "x".repeat(MAX_OUTPUT + 100))).unwrap();
        let out = read(&args(&p, ""));
        assert!(out.len() <= MAX_OUTPUT, "{} bytes", out.len());
        assert!(out.contains("is longer than the"), "{out}");
        assert!(out.contains("CRLF"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_marks_a_line_longer_than_the_output_limit() {
        let dir = tmpdir();
        let p = dir.join("one-line.txt");
        std::fs::write(&p, "x".repeat(CHUNK + MAX_OUTPUT)).unwrap();
        let out = read(&args(&p, ""));
        assert!(out.len() <= MAX_OUTPUT, "{} bytes", out.len());
        assert!(out.contains("line 1 is longer than the"), "{out}");
        assert!(out.contains("with Bash"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A row cut out of a long line ends on a character boundary: half a
    /// character there is the cut and not a defect in the file.
    #[test]
    fn read_cuts_a_long_line_on_a_character_boundary() {
        let dir = tmpdir();
        let p = dir.join("wide.txt");
        std::fs::write(&p, "€".repeat(CHUNK)).unwrap();
        let out = read(&args(&p, ""));
        assert!(out.len() <= MAX_OUTPUT, "{} bytes", out.len());
        let row = out
            .lines()
            .next()
            .expect("the head of the line")
            .strip_prefix("1\t")
            .expect("the row is numbered");
        assert_eq!(row.len() % "€".len(), 0, "a whole number of characters");
        assert!(row.chars().all(|c| c == '€'), "{row:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Bytes the page does not show are not the page's business: a file with a
    /// binary line is read up to that line, and refused at it.
    #[test]
    fn read_reports_a_page_that_is_not_utf8_with_its_line() {
        let dir = tmpdir();
        let p = dir.join("binary.bin");
        std::fs::write(&p, [b"first\n".as_slice(), &[0xff, 0xfe], b"\n"].concat()).unwrap();
        let out = read(&args(&p, ""));
        assert!(out.starts_with("error:"), "{out}");
        assert!(out.contains("at line 2"), "{out}");
        assert_eq!(
            read(&args(&p, r#","limit":1"#)),
            "1\tfirst\n[showing lines 1-1 of 2; read on with offset=2]"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_reports_an_empty_file() {
        let dir = tmpdir();
        let p = dir.join("empty.txt");
        std::fs::write(&p, "").unwrap();
        assert_eq!(read(&args(&p, "")), "[empty file]");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_rejects_bad_paging_arguments() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "a\nb").unwrap();
        for extra in [
            r#","offset":0"#,
            r#","limit":0"#,
            r#","offset":-1"#,
            r#","offset":1.5"#,
            r#","offset":"2""#,
            r#","limit":"all""#,
        ] {
            let out = read(&args(&p, extra));
            assert!(out.starts_with("error:"), "{extra} got {out}");
        }
        let past = read(&args(&p, r#","offset":9"#));
        assert!(past.contains("past the end"), "{past}");
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
        assert!(out.contains("not found"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "alpha");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_errors_when_not_unique() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "aa bb aa").unwrap();
        let out = edit(&args(&p, r#","old_string":"aa","new_string":"cc""#));
        assert!(out.contains("occurs 2 times"), "{out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "aa bb aa");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_rejects_empty_old_string() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "x").unwrap();
        let out = edit(&args(&p, r#","old_string":"","new_string":"y""#));
        assert!(out.contains("must not be empty"), "{out}");
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
        assert!(out.contains("not valid UTF-8"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_args_are_reported() {
        assert!(read("{}").contains("missing required argument file_path"));
        assert!(edit(r#"{"file_path":"/x"}"#).contains("missing required argument old_string"));
        assert!(write(r#"{"file_path":"/x"}"#).contains("missing required argument content"));
    }

    #[test]
    fn bad_json_and_missing_args_across_tools() {
        for call in [
            read("not json"),
            edit("not json"),
            write("not json"),
            edit("{}"),                                     // missing file_path
            edit(r#"{"file_path":"/x","old_string":"a"}"#), // missing new_string
            write("{}"),                                    // missing file_path
        ] {
            assert!(call.starts_with("error:"), "{call}");
        }
    }

    #[test]
    fn read_rejects_a_directory() {
        let dir = tmpdir();
        let out = read(&args(&dir, ""));
        assert!(out.contains("is a directory"), "{out}");
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
        assert!(out.contains("over the"), "{out}");
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
            let p = ro.join("sub/x.txt"); // read-only parent, create_dir_all fails
            let out = write(&format!(
                r#"{{"file_path":{:?},"content":"hi"}}"#,
                p.to_string_lossy()
            ));
            assert!(out.contains("failed to create directory"), "{out}");
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A write through a symlink changes the file the link points at: the link
    /// is left a link, which is the difference between following it and letting
    /// a rename land on the link's own path.
    #[cfg(unix)]
    #[test]
    fn write_follows_a_symlink_to_its_target() {
        let dir = tmpdir();
        let target = dir.join("real.txt");
        std::fs::write(&target, "old").unwrap();
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"new"}}"#,
            link.to_string_lossy()
        ));
        assert!(out.starts_with("ok:"), "{out}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "the link is still a link");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A link that names its target relatively is followed from the directory
    /// the link sits in, and a link whose target is not there yet is followed
    /// too: the write creates the target rather than replacing the link.
    #[cfg(unix)]
    #[test]
    fn write_follows_a_relative_link_and_a_dangling_one() {
        let dir = tmpdir();
        let link = dir.join("rel.txt");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"made"}}"#,
            link.to_string_lossy()
        ));
        assert!(out.starts_with("ok:"), "{out}");
        assert_eq!(
            std::fs::read_to_string(dir.join("real.txt")).unwrap(),
            "made"
        );
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "the link is still a link");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A chain of links that comes back to itself is refused, and refused
    /// before anything is created: following it would not end.
    #[cfg(unix)]
    #[test]
    fn write_refuses_a_loop_of_symlinks() {
        let dir = tmpdir();
        let a = dir.join("a");
        std::os::unix::fs::symlink("b", &a).unwrap();
        std::os::unix::fs::symlink("a", dir.join("b")).unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"x"}}"#,
            a.to_string_lossy()
        ));
        assert!(out.contains("loop"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An overwrite keeps the mode the file had: the temp file a write goes
    /// through is a new file, and a rename would hand the target the mode of
    /// whatever replaced it.
    #[cfg(unix)]
    #[test]
    fn write_keeps_the_mode_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        let p = dir.join("run.sh");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = write(&format!(
            r##"{{"file_path":{:?},"content":"#!/bin/sh\necho hi\n"}}"##,
            p.to_string_lossy()
        ));
        assert!(out.starts_with("ok:"), "{out}");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the executable bit survives the overwrite");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "#!/bin/sh\necho hi\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A write that would put back exactly what is already there is answered
    /// without a byte going to disk: a rewrite would bump the mtime and summon
    /// whoever watches the file, for a change that changes nothing.
    #[cfg(unix)]
    #[test]
    fn write_leaves_a_file_that_already_has_the_content_alone() {
        use std::os::unix::fs::MetadataExt;
        let dir = tmpdir();
        let p = dir.join("same.txt");
        let path_arg = format!(
            r#"{{"file_path":{:?},"content":"same"}}"#,
            p.to_string_lossy()
        );
        assert!(
            write(&path_arg).starts_with("ok: wrote"),
            "the first write makes the file"
        );
        let inode = std::fs::metadata(&p).unwrap().ino();
        let again = write(&path_arg);
        assert!(again.contains("left unchanged"), "{again}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "same");
        // A rewrite goes through a temp file and a rename, and the inode it
        // would leave behind is what says one happened.
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), inode);
        // A change of the same length is not mistaken for the same content.
        let changed = format!(
            r#"{{"file_path":{:?},"content":"diff"}}"#,
            p.to_string_lossy()
        );
        assert!(write(&changed).starts_with("ok: wrote"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "diff");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The text of a write lands in the line endings the file it replaces uses:
    /// a whole-file rewrite sent in the other endings would otherwise read, to
    /// whoever watches the file, as a change to every line of it.
    #[test]
    fn write_keeps_the_line_endings_of_the_file_it_replaces() {
        let dir = tmpdir();
        let crlf = dir.join("crlf.txt");
        std::fs::write(&crlf, "a\r\nb\r\n").unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"a\nb\nc\n"}}"#,
            crlf.to_string_lossy()
        ));
        assert!(out.contains("converted to the file's own CRLF"), "{out}");
        assert_eq!(std::fs::read(&crlf).unwrap(), b"a\r\nb\r\nc\r\n");

        // And the other way round: an LF file is not turned into a CRLF one.
        let lf = dir.join("lf.txt");
        std::fs::write(&lf, "a\nb\n").unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"a\r\nb\r\nc\r\n"}}"#,
            lf.to_string_lossy()
        ));
        assert!(out.contains("converted to the file's own LF"), "{out}");
        assert_eq!(std::fs::read(&lf).unwrap(), b"a\nb\nc\n");

        // Text already in the file's own endings is written byte for byte, and
        // there is nothing to say about it.
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"a\nb\n"}}"#,
            lf.to_string_lossy()
        ));
        assert!(out.starts_with("ok: wrote"), "{out}");
        assert!(!out.contains("line endings"), "{out}");
        assert_eq!(std::fs::read(&lf).unwrap(), b"a\nb\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A created file has no endings to follow, so what the call sent is what
    /// lands -- the same for a file with no line ending to go by, one whose
    /// endings are mixed, and one too large to read the endings off.
    #[test]
    fn write_follows_no_endings_it_cannot_read() {
        let dir = tmpdir();
        // A file that is not there yet keeps the endings of the call.
        let fresh = dir.join("fresh.txt");
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"a\r\nb\r\n"}}"#,
            fresh.to_string_lossy()
        ));
        assert!(out.starts_with("ok: wrote"), "{out}");
        assert_eq!(std::fs::read(&fresh).unwrap(), b"a\r\nb\r\n");

        // One line and no ending at all: there is nothing to say how it ends.
        let one = dir.join("one-line.txt");
        std::fs::write(&one, "just one line").unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"a\nb\n"}}"#,
            one.to_string_lossy()
        ));
        assert!(out.starts_with("ok: wrote"), "{out}");
        assert_eq!(std::fs::read(&one).unwrap(), b"a\nb\n");

        // Mixed endings: neither style is the file's, so neither is imposed.
        let mixed = dir.join("mixed.txt");
        std::fs::write(&mixed, "a\nb\r\n").unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"x\nc\r\n"}}"#,
            mixed.to_string_lossy()
        ));
        assert!(out.starts_with("ok: wrote"), "{out}");
        assert_eq!(std::fs::read(&mixed).unwrap(), b"x\nc\r\n");

        // A file larger than a write may replace is not read to find its
        // endings: the call's own text is what lands.
        let big = dir.join("big.txt");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"x\n"}}"#,
            big.to_string_lossy()
        ));
        assert!(out.starts_with("ok: wrote"), "{out}");
        assert_eq!(std::fs::read(&big).unwrap(), b"x\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The endings of a file are read off the whole of it, in chunks: a pair
    /// split across two reads is still one CRLF line, and a file that mixes the
    /// two kinds is not read as the one it happened to start with.
    #[test]
    fn endings_are_read_across_chunk_boundaries() {
        let dir = tmpdir();
        let split = dir.join("split.txt");
        let mut text = vec![b'x'; CHUNK - 1];
        text.extend_from_slice(b"\r\n");
        text.extend_from_slice(b"y\r\n");
        std::fs::write(&split, &text).unwrap();
        assert_eq!(endings(&split), Some(Endings::Crlf));

        let mixed = dir.join("mixed.txt");
        std::fs::write(&mixed, b"a\nb\r\nc\r\n").unwrap();
        assert_eq!(endings(&mixed), None);

        // Nothing to read them off: a file that is not there, a directory, and
        // a file the write cap does not reach.
        assert_eq!(endings(&dir.join("nope.txt")), None);
        assert_eq!(endings(&dir), None);
        let over = dir.join("over.txt");
        std::fs::File::create(&over)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert_eq!(endings(&over), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Text is brought into the file's own endings as a whole line does: a
    /// newline at the very start of the text counts, a carriage return that ends
    /// no line is left where it is, and multi-byte characters survive the trip.
    #[test]
    fn the_text_is_brought_into_the_endings_of_the_file() {
        assert_eq!(crlf_lines("\na\n"), Some("\r\na\r\n".into()));
        assert_eq!(crlf_lines("a\rb\n"), Some("a\rb\r\n".into()));
        assert_eq!(crlf_lines("a\r\n"), None);
        assert_eq!(crlf_lines("no newline at all"), None);
        assert_eq!(lf_lines("é\r\n日\r\n"), Some("é\n日\n".into()));
        assert_eq!(lf_lines("é\n日\n"), None);
        assert_eq!(
            lf_lines("a\rb"),
            None,
            "a carriage return ends no line here"
        );
    }

    /// A text that only reads as what is already on disk once it is in the
    /// file's endings is still left alone: the comparison is made on the bytes a
    /// write would land, not on the ones the call sent.
    #[cfg(unix)]
    #[test]
    fn write_answers_unchanged_for_text_it_brought_over() {
        use std::os::unix::fs::MetadataExt;
        let dir = tmpdir();
        let p = dir.join("crlf.txt");
        std::fs::write(&p, "a\r\nb\r\n").unwrap();
        let inode = std::fs::metadata(&p).unwrap().ino();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"a\nb\n"}}"#,
            p.to_string_lossy()
        ));
        assert!(out.contains("already has exactly this content"), "{out}");
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), inode);
        assert_eq!(std::fs::read(&p).unwrap(), b"a\r\nb\r\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A write that cannot land leaves nothing behind: a directory and a fifo
    /// are refused before a byte is written, and a rename that fails anyway --
    /// the atomic write called with a path it cannot replace -- removes the temp
    /// file it made, so no `.caocli-tmp-` file is orphaned in the directory.
    #[test]
    fn a_write_that_cannot_land_leaves_nothing_behind() {
        let dir = tmpdir();
        let sub = dir.join("a-directory");
        std::fs::create_dir(&sub).unwrap();
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"x"}}"#,
            sub.to_string_lossy()
        ));
        assert!(out.contains("is a directory"), "{out}");
        let fifo = dir.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo is how the test makes a fifo");
        let out = write(&format!(
            r#"{{"file_path":{:?},"content":"x"}}"#,
            fifo.to_string_lossy()
        ));
        assert!(out.contains("not a regular file"), "{out}");
        assert!(atomic_write(&sub, "x").is_err());
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("caocli-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_and_write_report_atomic_write_failure() {
        let dir = tmpdir();
        let ro = dir.join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        let target = ro.join("f.txt");
        std::fs::write(&target, "content").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
            // Edit: read succeeds, writing the tmp file fails
            let out = edit(&args(
                &target,
                r#","old_string":"content","new_string":"changed""#,
            ));
            assert!(out.contains("failed to write"), "{out}");
            // Write: writing the tmp file fails
            let out = write(&format!(
                r#"{{"file_path":{:?},"content":"overwrite"}}"#,
                target.to_string_lossy()
            ));
            assert!(out.contains("failed to write"), "{out}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "content"); // original intact
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
