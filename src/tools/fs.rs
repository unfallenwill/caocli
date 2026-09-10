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
                "Read the full contents of a text file. Confirm the original text with this tool \
                 before modifying a file. \
                 UTF-8 text only: a directory raises an error, while a binary file is decoded \
                 into garbage without raising one. \
                 Output over 10240 bytes is truncated on a byte boundary and reported, and a \
                 file over 10MB raises an error; in either case read it in pieces with Bash \
                 instead, e.g. sed -n '100,200p'."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "path of the file to read" }
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

/// Read: returns the whole file; truncated with a notice beyond MAX_OUTPUT.
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
                format!(
                    "{out}\n[truncated to {MAX_OUTPUT} bytes; read the rest in pieces with Bash]"
                )
            } else {
                out
            }
        }
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
        assert_eq!(read(&args(&p, "")), "content");
        let missing = read(&args(&dir.join("nope.txt"), ""));
        assert!(missing.contains("cannot access"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_truncates_large_file() {
        let dir = tmpdir();
        let p = dir.join("big.txt");
        std::fs::write(&p, "x".repeat(MAX_OUTPUT + 100)).unwrap();
        let out = read(&args(&p, ""));
        assert!(out.contains("[truncated to"), "should report truncation");
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
