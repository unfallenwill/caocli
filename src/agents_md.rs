//! Project instructions: the AGENTS.md files of the workspace.
//!
//! A session carries the instructions of the workspace it was started in: the
//! AGENTS.md files found walking up from the current directory, read once when
//! the session is created and stored framed in its meta. From then on the
//! session sends what it stored — never what the files say now. That is the
//! cache contract (a request must be buildable from `(provider, meta, history)`
//! alone, and its prefix must stay byte-for-byte stable for the life of the
//! session) and it is also what a session means: a record of one run, not a
//! window onto a moving filesystem.

use std::path::Path;

/// The file the instructions are read from, in the directory the session was
/// started in and in every directory above it up to the root.
pub const FILE_NAME: &str = "AGENTS.md";

/// The most of one file's content a session carries. A file larger than this
/// has outgrown what every turn should pay for, and its head is what its
/// author meant to be read first anyway.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// What opens the message the instructions are sent in, and what says a file
/// was cut. Constants, because both are stored in the session and sent
/// byte-for-byte: a changed letter is a different history and a cache miss.
const LEAD: &str = "Project instructions loaded from AGENTS.md files in this workspace follow. They apply to the entire session.";
const TRUNCATED: &str = "\n[truncated]";

/// Read the workspace's instructions, framed as the message they are sent in.
/// `None` when there is nothing to send: no `AGENTS.md` anywhere up the tree,
/// or none of them readable and non-empty.
pub fn load() -> Option<String> {
    load_from(&std::env::current_dir().ok()?)
}

/// The same, from a directory given rather than the process's own, so that
/// what a workspace produces is a test without moving the process around.
pub fn load_from(dir: &Path) -> Option<String> {
    // Nearest first in the walk, outermost first in the message: the closer a
    // file is to where the session was started, the more specific its
    // instructions are, and the block read last is the one that reads as
    // immediate. A file that cannot be read is skipped, not a failure — the
    // run should not stand or fall by a file it is only offered.
    let mut dirs: Vec<&Path> = dir.ancestors().collect();
    dirs.reverse();
    let mut blocks = Vec::new();
    for dir in dirs {
        let path = dir.join(FILE_NAME);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        blocks.push(format!("{}:\n{}", path.display(), capped(content)));
    }
    if blocks.is_empty() {
        None
    } else {
        Some(format!("{LEAD}\n\n{}", blocks.join("\n\n")))
    }
}

/// The content, no larger than one file's share. A cut lands on a UTF-8
/// character boundary — content is measured in chars where it is cut, never
/// in bytes.
fn capped(content: &str) -> String {
    if content.len() <= MAX_FILE_BYTES {
        return content.to_owned();
    }
    let mut cut = MAX_FILE_BYTES;
    while !content.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATED}", &content[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-agents-md-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_workspace_without_the_file_sends_nothing() {
        let dir = tmpdir();
        assert_eq!(load_from(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_file_is_framed_as_the_message_it_is_sent_in() {
        let dir = tmpdir();
        std::fs::write(dir.join(FILE_NAME), "be brief\n").unwrap();
        let text = load_from(&dir).unwrap();
        assert!(text.starts_with(LEAD), "the lead says what the blocks are");
        assert!(text.contains(&format!("{}:\nbe brief", dir.join(FILE_NAME).display())));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_file_sends_nothing() {
        let dir = tmpdir();
        std::fs::write(dir.join(FILE_NAME), "   \n").unwrap();
        assert_eq!(load_from(&dir), None, "a whitespace file says nothing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_walk_takes_the_parent_directories_it_passes_through() {
        // Started deep in a workspace, a session is still in that workspace:
        // the file at the root is found from a subdirectory.
        let root = tmpdir();
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join(FILE_NAME), "root rules").unwrap();
        let text = load_from(&deep).unwrap();
        assert!(
            text.contains(&format!("{}:\nroot rules", root.join(FILE_NAME).display())),
            "the root file is found from below: {text}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nested_files_are_read_outermost_first() {
        let root = tmpdir();
        let deep = root.join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join(FILE_NAME), "outer").unwrap();
        std::fs::write(deep.join(FILE_NAME), "inner").unwrap();
        let text = load_from(&deep).unwrap();
        let outer = text.find("outer").unwrap();
        let inner = text.find("inner").unwrap();
        assert!(outer < inner, "the more specific file is the one read last");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_very_large_file_is_cut_on_a_character_boundary() {
        let dir = tmpdir();
        // A file over the cap whose cut lands in the middle of a multi-byte
        // character: cutting in bytes alone would split one and produce text
        // that is not UTF-8.
        let mut content = "x".repeat(MAX_FILE_BYTES - 1);
        content.push_str("ééé");
        std::fs::write(dir.join(FILE_NAME), content).unwrap();
        let text = load_from(&dir).unwrap();
        assert!(text.ends_with(TRUNCATED), "the cut is said where it is");
        assert!(
            text.is_char_boundary(text.len() - TRUNCATED.len()),
            "what is kept ends on a boundary"
        );
        assert!(
            !text.contains("é"),
            "the character the cut straddles is dropped whole"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_file_is_skipped_and_the_rest_is_kept() {
        let root = tmpdir();
        let deep = root.join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        // A directory standing where the file would be cannot be read as one;
        // the workspace still has instructions above it.
        std::fs::create_dir(deep.join(FILE_NAME)).unwrap();
        std::fs::write(root.join(FILE_NAME), "root rules").unwrap();
        let text = load_from(&deep).unwrap();
        assert!(text.contains("root rules"), "what is readable is kept");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
