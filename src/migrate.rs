//! Lift v0 session files to v1 by writing a new file beside each one.
//!
//! A v0 file has no envelope and no event vocabulary; a v1 file does. A
//! v0 file is read with the same reader the rest of the crate uses, the
//! in-memory fold is rebuilt into a v1 writer, and a new file is written
//! with the same id plus a `-v1` suffix so the original is never touched.
//!
//! The migration is one-shot: running it twice on the same directory
//! leaves the original v0 files alone and produces another `-v1` file
//! with `-v2` in its id. The original's existence is part of the
//! diagnostic — until it is deleted by hand, both files can be loaded,
//! and `--resume <original-id>` continues to work.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::session::{Dialect, Session, SessionMeta, validate_resume_id};

/// Migrate a single v0 session file to v1.
///
/// The new file is written into `dst_dir` (typically the same directory as
/// the original) under the id `<original>-v1`, so the original is left
/// untouched. Returns the path of the new file. Errors out if the source
/// file is already in v1 or if the destination already exists.
pub fn migrate_v0_to_v1(src: &Path, dst_dir: &Path) -> Result<PathBuf> {
    let original = Session::load(src).with_context(|| {
        format!(
            "failed to load session file for migration: {}",
            src.display()
        )
    })?;
    if original.dialect == Dialect::V1 {
        bail!(
            "session is already in v1: {} (nothing to migrate)",
            src.display()
        );
    }

    let new_id = format!("{}-v1", original.id);
    let new_path = dst_dir.join(format!("{new_id}.jsonl"));
    if new_path.exists() {
        bail!(
            "refusing to overwrite existing migration target: {}",
            new_path.display()
        );
    }

    // `Session::create` picks its own timestamp-based id and would not
    // produce a file at `new_path`. Build the v1 file by writing the
    // header and each message with the helpers in `session.rs`, which
    // share the line shapes `Session::create` uses so a migrated file
    // is byte-for-byte the same as one written live.
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&new_path)
        .with_context(|| format!("failed to create v1 file: {}", new_path.display()))?;
    let created_at = chrono::Utc::now().timestamp();
    let header = crate::session::V1HeaderForMigration {
        line_type: "header".into(),
        format: "caocli-session".into(),
        format_version: 1,
        id: new_id.clone(),
        timestamp_ms: Some(created_at * 1000),
        working_directory: None,
        application: None,
        meta: original.meta.clone(),
    };
    crate::session::write_v1_header_for_migration(&mut file, &header)?;
    let mut seq: u64 = 0;
    for msg in &original.messages {
        seq += 1;
        crate::session::append_message_for_migration(&mut file, seq, msg)?;
    }
    drop(file);

    Ok(new_path)
}

/// Migrate every v0 session in `dir` (the directory returned by
/// `config::sessions_dir`). Sessions already in v1 are left alone. The
/// returned paths are the new files, in the order they were written.
pub fn migrate_all_v0_to_v1(dir: &Path) -> Result<Vec<PathBuf>> {
    // Walk the directory ourselves rather than going through
    // `session::list`, because `list` opens each file under an exclusive
    // flock to summarise it; calling `Session::load` on the same path
    // right after would deadlock against our own handle.
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        // Cheap first pass: read the first line and look for the v0
        // tag. A v1 file starts with `{"type":"header"` and is skipped
        // here so we never open a flock we are not going to use.
        if let Some(first) = std::fs::read(&path).ok().and_then(|d| {
            d.split(|&b| b == b'\n')
                .find(|l| !l.is_empty())
                .map(|s| s.to_vec())
        }) {
            let head = std::str::from_utf8(&first).unwrap_or("");
            if head.starts_with(r#"{"type":"header""#) {
                continue;
            }
        }
        candidates.push(path);
    }
    let mut migrated = Vec::new();
    for path in candidates {
        let new_path = migrate_v0_to_v1(&path, dir)?;
        migrated.push(new_path);
    }
    Ok(migrated)
}

/// Run the migration on a CLI selection: either the named session id or
/// every v0 file in the sessions directory. The id is looked up in `dir`
/// as `<id>.jsonl`. The returned paths are the files written.
pub fn run(dir: &Path, id: Option<&str>, all: bool) -> Result<Vec<PathBuf>> {
    match (id, all) {
        (Some(id), false) => {
            validate_resume_id(id)?;
            let path = dir.join(format!("{id}.jsonl"));
            if !path.exists() {
                bail!("session file not found: {}", path.display());
            }
            Ok(vec![migrate_v0_to_v1(&path, dir)?])
        }
        (None, true) => {
            // Walk every layout directory under `~/.caocli/sessions/`
            // and migrate v0 files in each. A v0 file found anywhere
            // gets a v1 sibling in the same directory.
            let dirs = crate::config::all_sessions_dirs()
                .context("looking up workspaces for --migrate --all")?;
            let mut all_migrated = Vec::new();
            for (path, _cwd) in dirs {
                all_migrated.extend(migrate_all_v0_to_v1(&path)?);
            }
            Ok(all_migrated)
        }
        (Some(_), true) | (None, false) => {
            bail!("--migrate takes either an id or --all, not both / neither");
        }
    }
}

// Re-export `SessionMeta` so callers building their own (e.g. the
// CLI's `--migrate` handler) can construct one without depending on the
// full `session` surface.
#[allow(dead_code)]
pub(crate) type Meta = SessionMeta;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use std::fs;

    fn tmpdir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-migrate-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        fs::create_dir_all(&d).unwrap();
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

    /// Write a hand-crafted v0 session file with two messages. The
    /// migration target reads v0 from disk, so the source has to be v0
    /// too — `Session::create` writes v1 in this build, which is what
    /// makes migration useful in the first place.
    fn write_v0_source(dir: &Path, id: &str, messages: &[(&str, &str)]) -> PathBuf {
        use std::fmt::Write as _;
        let mut out = String::new();
        writeln!(
            out,
            r#"{{"t":"header","id":"{id}","created_at":1,"provider":"deepseek","model":"deepseek-v4-flash"}}"#
        )
        .unwrap();
        for (role, content) in messages {
            writeln!(
                out,
                r#"{{"t":"msg","message":{{"role":"{role}","content":"{content}"}}}}"#
            )
            .unwrap();
        }
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn migrate_writes_a_v1_file_with_the_same_messages() {
        let dir = tmpdir();
        let original = write_v0_source(
            &dir,
            "20260101-120000",
            &[("user", "hello"), ("user", "again")],
        );
        let new_path = migrate_v0_to_v1(&original, &dir).unwrap();
        let bytes = fs::read_to_string(&new_path).unwrap();
        assert!(
            bytes.starts_with(r#"{"type":"header""#),
            "new file is v1: {bytes}"
        );
        assert!(
            bytes.contains(r#""format_version":1"#),
            "header carries format_version: {bytes}"
        );
        let new = Session::load(&new_path).unwrap();
        assert_eq!(new.dialect, Dialect::V1);
        assert_eq!(new.messages.len(), 2);
        assert_eq!(new.messages[0].text().as_deref(), Some("hello"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_leaves_the_original_file_untouched() {
        let dir = tmpdir();
        let original = write_v0_source(&dir, "20260101-120000", &[("user", "untouched")]);
        let original_bytes = fs::read(&original).unwrap();
        let _new_path = migrate_v0_to_v1(&original, &dir).unwrap();
        let after_bytes = fs::read(&original).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "the original v0 file is byte-for-byte unchanged"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_refuses_to_overwrite_a_present_target() {
        let dir = tmpdir();
        let original = write_v0_source(&dir, "20260101-120000", &[("user", "a")]);
        let target = dir.join("20260101-120000-v1.jsonl");
        fs::write(&target, b"preexisting\n").unwrap();
        let err = match migrate_v0_to_v1(&original, &dir) {
            Ok(_) => panic!("migrate must fail when the target exists"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "the error names the cause: {err:#}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_refuses_a_v1_source() {
        let dir = tmpdir();
        let s = Session::create(&dir, test_meta()).unwrap();
        let path = s.path.clone();
        drop(s);
        let err = match migrate_v0_to_v1(&path, &dir) {
            Ok(_) => panic!("migrate must fail on a v1 source"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("already in v1"),
            "the error names the cause: {err:#}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_all_skips_v1_files() {
        let dir = tmpdir();
        let a = write_v0_source(&dir, "v0-a", &[("user", "a")]);
        let b = write_v0_source(&dir, "v0-b", &[("user", "b")]);
        let c = Session::create(&dir, test_meta()).unwrap();
        let c_path = c.path.clone();
        drop(c);

        let new_paths = migrate_all_v0_to_v1(&dir).unwrap();
        assert_eq!(
            new_paths.len(),
            2,
            "the two v0 sources are migrated, the v1 file is left alone"
        );
        assert!(a.exists());
        assert!(b.exists());
        assert!(c_path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_rejects_an_unsafe_id_before_touching_disk() {
        let dir = tmpdir();
        let err = match run(&dir, Some("../escape"), false) {
            Ok(_) => panic!("run must fail on an unsafe id"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("--resume id"),
            "the error names the cause: {err:#}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
