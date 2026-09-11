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
                "Read a UTF-8 text file, one numbered row per line: the line number, a tab, \
                 then the line. The number and the tab are not part of the file content. \
                 Confirm the original text with this tool before modifying a file. \
                 Use offset (the 1-based line number to start at; default 1) and limit (how \
                 many lines to read; default to the end of the file) to page through a long \
                 file. A directory, a non-UTF-8 file, and a file over 10MB are reported as \
                 errors. Output is capped at 10240 bytes: it is cut on a line boundary, and \
                 the marker after the last row says which lines were shown and the offset to \
                 read on from."
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

/// Bytes held back from the output cap so the continuation marker always fits
/// inside it.
const MARKER_BYTES: usize = 128;

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
    match read_text(&path) {
        Err(e) => e,
        Ok(text) => render_lines(&path, &text, offset, limit),
    }
}

/// Number the requested lines and cut them to the output cap, on a line
/// boundary: a row is kept whole or the result says where to go on from.
fn render_lines(path: &str, text: &str, offset: usize, limit: Option<usize>) -> String {
    let total = text.lines().count();
    if total == 0 {
        return "[empty file]".into();
    }
    if offset > total {
        return format!("error: offset {offset} is past the end of {path} ({total} lines)");
    }
    let wanted = limit.unwrap_or(usize::MAX).min(total - (offset - 1));
    let width = total.to_string().len();
    let budget = MAX_OUTPUT.saturating_sub(MARKER_BYTES);
    let mut out = String::new();
    let mut shown = 0usize;
    let mut cut_mid_line = false;
    for (i, line) in text.lines().skip(offset - 1).take(wanted).enumerate() {
        let prefix = format!("{:>width$}\t", offset + i);
        let separator = usize::from(shown > 0);
        if out.len() + separator + prefix.len() + line.len() <= budget {
            if shown > 0 {
                out.push('\n');
            }
            out.push_str(&prefix);
            out.push_str(line);
            shown += 1;
            continue;
        }
        if shown == 0 {
            // A single line longer than the whole budget: show what fits and
            // say so, rather than a page that never advances.
            let (head, _) = truncate(line, budget.saturating_sub(prefix.len()));
            out.push_str(&prefix);
            out.push_str(&head);
            shown = 1;
            cut_mid_line = true;
        }
        break;
    }
    let last = offset + shown - 1;
    if cut_mid_line {
        out.push_str(&format!(
            "\n[line {last} is longer than the {MAX_OUTPUT}-byte output limit; read the rest \
             of it with Bash]"
        ));
    } else if last < total {
        out.push_str(&format!(
            "\n[showing lines {offset}-{last} of {total}; read on with offset={}]",
            last + 1
        ));
    }
    out
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
    let updated = content.replacen(&old, &new, 1);
    match atomic_write(Path::new(&path), &updated) {
        Ok(()) => format!(
            "ok: replaced 1 occurrence; {path} is now {} bytes",
            updated.len()
        ),
        Err(e) => format!("error: failed to write {path}: {e}"),
    }
}

/// Write: creates or overwrites a whole file; parent directories are created.
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
    let p = Path::new(&path);
    if let Some(parent) = p.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return format!(
            "error: failed to create directory {}: {e}",
            parent.display()
        );
    }
    match atomic_write(p, &content) {
        Ok(()) => format!("ok: wrote {path} ({} bytes)", content.len()),
        Err(e) => format!("error: failed to write {path}: {e}"),
    }
}

/// Read a UTF-8 text file. Every failure becomes error text for the model.
fn read_text(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p).map_err(|e| format!("error: cannot access {path}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("error: {path} is a directory, not a file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "error: file {path} is {} bytes, over the {MAX_FILE_BYTES} byte limit; read it in \
             pieces with Bash (e.g. sed -n '1,200p')",
            meta.len()
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("error: failed to read {path}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("error: {path} is not valid UTF-8 text"))
}

/// tmp file in the same directory + rename, so a crash mid-write cannot corrupt
/// the original file.
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
        std::fs::write(&p, "content").unwrap();
        assert_eq!(read(&args(&p, "")), "1\tcontent");
        let missing = read(&args(&dir.join("nope.txt"), ""));
        assert!(missing.contains("cannot access"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_numbers_lines_and_pages_with_offset_and_limit() {
        let dir = tmpdir();
        let p = dir.join("f.txt");
        std::fs::write(&p, "a\nb\nc\nd\ne").unwrap();
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
        std::fs::write(&p, lines.join("\n")).unwrap();
        // Page through to the end: every line is seen once, in order, and no
        // page goes over the cap.
        let mut offset = 1usize;
        let mut seen: Vec<String> = Vec::new();
        let mut pages = 0;
        loop {
            let out = read(&args(&p, &format!(r#","offset":{offset}"#)));
            assert!(out.len() <= MAX_OUTPUT, "page is {} bytes", out.len());
            let mut next = None;
            for row in out.lines() {
                if let Some(rest) = row.strip_prefix("[showing lines ") {
                    next = Some(
                        rest.rsplit_once("offset=")
                            .expect("the marker says how to read on")
                            .1
                            .trim_end_matches(']')
                            .parse::<usize>()
                            .unwrap(),
                    );
                    continue;
                }
                let (number, text) = row.split_once('\t').expect("every row is numbered");
                let number = number
                    .trim()
                    .parse::<usize>()
                    .unwrap_or_else(|e| panic!("row {row:?}: {e}"));
                assert_eq!(number, seen.len() + 1);
                seen.push(text.to_owned());
            }
            match next {
                Some(n) => offset = n,
                None => break,
            }
            pages += 1;
            assert!(pages < 100, "paging is not advancing");
        }
        assert_eq!(seen, lines);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_marks_a_line_longer_than_the_output_limit() {
        let dir = tmpdir();
        let p = dir.join("one-line.txt");
        std::fs::write(&p, "x".repeat(MAX_OUTPUT + 100)).unwrap();
        let out = read(&args(&p, ""));
        assert!(out.len() <= MAX_OUTPUT, "{} bytes", out.len());
        assert!(out.contains("line 1 is longer than the"), "{out}");
        assert!(out.contains("with Bash"), "{out}");
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
    fn read_rejects_directory_and_oversized_file() {
        let dir = tmpdir();
        // a directory reports "is a directory"
        let out = read(&args(&dir, ""));
        assert!(out.contains("is a directory"), "{out}");
        // over the per-file limit
        let big = dir.join("huge.bin");
        std::fs::write(&big, vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        let out = read(&args(&big, ""));
        assert!(out.contains("over the"), "{out}");
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
