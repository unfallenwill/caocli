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
//!
//! A directory *below* where the session was started can carry instructions of
//! its own, and those are picked up when the model first operates on a file
//! there: the interpreter appends the nearest file's content to the log as a
//! user message, where one is legal, and never twice for one directory — what
//! has been sent is a fold of the log, not state of its own.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::types::Message;

/// The file the instructions are read from, in the directory the session was
/// started in and in every directory above it up to the root.
pub const FILE_NAME: &str = "AGENTS.md";

/// The most of one file's content a session carries. A file larger than this
/// has outgrown what every turn should pay for, and its head is what its
/// author meant to be read first anyway.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// What opens the message the startup instructions are sent in, and what says
/// a file was cut. Constants, because both are stored in the session and sent
/// byte-for-byte: a changed letter is a different history and a cache miss.
const LEAD: &str = "Project instructions loaded from AGENTS.md files in this workspace follow. They apply to the entire session.";
const TRUNCATED: &str = "\n[truncated]";

/// What opens an injected message, and the one shape both the replay and the
/// sent-scan recognize. The directory is named on the line after it, alone:
/// that line is all the scan reads back.
pub const INJECT_LEAD: &str = "Project instructions from the AGENTS.md in the directory below follow. They apply to the files there and below.\n";

/// The dim line a front end shows when a directory's instructions are picked
/// up — the one the live turn emits and the replayed one folds, so the two
/// cannot say different things about the same message.
pub fn notice_text(dir: &str) -> String {
    format!("project instructions: {dir}/AGENTS.md")
}

/// The directory an injected message names, if the message is one.
pub fn injected_dir(text: &str) -> Option<&str> {
    text.strip_prefix(INJECT_LEAD)?.lines().next()
}

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
        if let Some(content) = read(dir) {
            blocks.push(format!("{}:\n{}", dir.join(FILE_NAME).display(), content));
        }
    }
    if blocks.is_empty() {
        None
    } else {
        Some(format!("{LEAD}\n\n{}", blocks.join("\n\n")))
    }
}

/// The nearest AGENTS.md at `dir` or above it: the directory that carries it,
/// and the message its content is sent in. `None` when nothing readable and
/// non-empty stands between the directory and the root.
pub fn nearest(dir: &Path) -> Option<(PathBuf, String)> {
    dir.ancestors().find_map(|dir| {
        read(dir).map(|content| {
            let text = format!("{INJECT_LEAD}{}\n\n{}", dir.display(), content);
            (dir.to_path_buf(), text)
        })
    })
}

/// The message to append for a file in `dir`, when its nearest instructions
/// have not reached this session yet: the directory found and the text.
///
/// Two ways to have been sent already, both answered without reading anything
/// twice: the nearest file stands at or above where the session was started,
/// which is the chain the startup message carried; or the log carries an
/// injection naming the directory, which [`sent_dirs`] folds out of it.
pub fn message_for(
    dir: &Path,
    workspace: &Path,
    messages: &[Message],
) -> Option<(PathBuf, String)> {
    let (found, text) = nearest(dir)?;
    if workspace.starts_with(&found) || sent_dirs(messages).contains(&found) {
        return None;
    }
    Some((found, text))
}

/// The directories whose instructions a session has already been sent, folded
/// from the log: a user message in the injection shape names its directory on
/// its first line, and nothing else is kept anywhere.
pub fn sent_dirs(messages: &[Message]) -> HashSet<PathBuf> {
    messages
        .iter()
        .filter_map(|m| {
            let text = m.text()?;
            injected_dir(&text).map(PathBuf::from)
        })
        .collect()
}

/// The content of the directory's AGENTS.md, trimmed and capped; `None` when
/// there is nothing to read there.
fn read(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join(FILE_NAME)).ok()?;
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    Some(capped(content))
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

    #[test]
    fn nearest_is_the_closest_directory_that_carries_the_file() {
        let root = tmpdir();
        let deep = root.join("src").join("foo");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join(FILE_NAME), "outer").unwrap();
        std::fs::write(root.join("src").join(FILE_NAME), "middle").unwrap();
        let (found, text) = nearest(&deep).unwrap();
        assert_eq!(found, root.join("src"), "the closest one wins");
        assert!(text.contains("middle"), "{text}");
        assert!(!text.contains("outer"), "only the nearest is sent: {text}");
        let (found, _) = nearest(&root.join("src").join("bar")).unwrap();
        assert_eq!(found, root.join("src"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nearest_without_the_file_anywhere_is_none() {
        let root = tmpdir();
        let deep = root.join("a");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(nearest(&deep).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_injected_message_names_its_directory_and_reads_back_as_one() {
        let dir = tmpdir();
        let text = format!("{INJECT_LEAD}{}\n\nbe brief", dir.display());
        assert_eq!(injected_dir(&text), Some(dir.to_str().unwrap()));
        assert_eq!(sent_dirs(&[Message::user(text)]), {
            let mut s = HashSet::new();
            s.insert(dir.clone());
            s
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_below_the_workspace_is_new_and_one_it_carries_is_not() {
        let workspace = tmpdir();
        std::fs::write(workspace.join(FILE_NAME), "workspace rules").unwrap();
        let sub = workspace.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(FILE_NAME), "sub rules").unwrap();
        let file_dir = sub.join("deeper");
        std::fs::create_dir_all(&file_dir).unwrap();

        // First touch: the nearest file is below the workspace, so the startup
        // message did not carry it, and nothing in the log has it either.
        let (found, text) = message_for(&file_dir, &workspace, &[]).unwrap();
        assert_eq!(found, sub);
        assert!(text.contains("sub rules"), "{text}");

        // Second touch: the log already carries it.
        let sent = [Message::user(text)];
        assert!(message_for(&file_dir, &workspace, &sent).is_none());

        // A file in the workspace itself: its nearest is the workspace's own
        // file, which the startup message carried.
        assert!(message_for(&workspace, &workspace, &[]).is_none());
        std::fs::remove_dir_all(&workspace).unwrap();
    }

    #[test]
    fn a_directory_outside_the_workspace_gets_its_own_nearest() {
        let workspace = tmpdir();
        std::fs::write(workspace.join(FILE_NAME), "workspace rules").unwrap();
        let elsewhere = tmpdir();
        std::fs::write(elsewhere.join(FILE_NAME), "elsewhere rules").unwrap();
        let (found, text) = message_for(&elsewhere, &workspace, &[]).unwrap();
        assert_eq!(found, elsewhere);
        assert!(text.contains("elsewhere rules"), "{text}");
        std::fs::remove_dir_all(&workspace).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }
}
