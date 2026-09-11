//! The Glob tool: the files a name pattern asks for, newest first.
//!
//! The other three file tools each name one file; this one is how a file is
//! found to name. It is the read side of the workspace without being a read:
//! nothing here opens a file, so nothing here can be slow by way of a file's
//! size. What a call costs is the walk, and the walk is bounded on purpose --
//! see [`Limits`] for what the three numbers are and why an answer that ran
//! into one says so rather than reading like an answer that did not.

use std::collections::HashSet;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::time::SystemTime;

use serde_json::json;

use super::{MAX_OUTPUT, parse_args, required_string, truncate};
use crate::types::{FunctionDef, ToolDef};

pub const NAME: &str = "Glob";

/// The most paths one call lists.
///
/// A hundred is more than a person reads and more than a model needs to pick
/// from; a call that matches more is a call to narrow, and the answer says how
/// many there were so that the narrowing is an informed one.
const MAX_MATCHES: usize = 100;

/// The most entries one call examines while walking.
///
/// The cap is about time, not memory. A walk is a readdir and a stat per entry,
/// and a workspace that has grown a build tree holds hundreds of thousands of
/// them; a call is one beat of a turn, and the turn waits for its answer.
/// Nothing about a pattern bounds the tree it is asked of, so the call bounds
/// itself and says when it stopped early: an answer that silently missed files
/// would be worse than a slow one.
const MAX_ENTRIES: usize = 100_000;

/// Bytes held back from the output cap for the note that follows the list when
/// a cap cut the answer short: an answer that is all paths and no note never
/// says that it is not all of them.
const NOTE_BYTES: usize = 256;

/// How much of a pattern is echoed back when nothing matched.
const PATTERN_COLUMNS: usize = 200;

/// The directory names, other than the dotted ones, that the walk holds back
/// from: whole trees of files nobody here wrote or of files this machine
/// wrote, and a workspace holds more in them than in its source.
///
/// A name belongs on this list when a pattern like `**/*.rs` is never asking
/// about it. The walk enters one anyway when the pattern names it exactly, so
/// `target/**/*.rs` and naming it as the path are the two ways to ask about it
/// on purpose.
const HEAVY: [&str; 2] = ["node_modules", "target"];

/// The caps one call runs under: how much of the tree it walks, how many paths
/// it lists, and how many bytes the answer may take.
///
/// The three are constants of the program; they are a value here so that a test
/// can drive each to its edge without a tree of a hundred thousand entries
/// behind it.
#[derive(Debug, Clone, Copy)]
struct Limits {
    entries: usize,
    matches: usize,
    bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: MAX_ENTRIES,
            matches: MAX_MATCHES,
            bytes: MAX_OUTPUT,
        }
    }
}

pub fn definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: NAME.into(),
            description: Some(
                "Find the files whose name matches a pattern, under a directory and below it: \
                 where a file is, without walking the tree by hand. \
                 The pattern is a glob, matched against each path below the directory searched. \
                 * matches any run of characters within one name and never a /, ? matches \
                 exactly one character, ** as a whole name matches any number of directories \
                 including none, [abc] matches one character out of a set ([!abc], or [^abc], \
                 the ones outside it; a-c a range), and a backslash makes the next character \
                 literal. A pattern with no / in it is asked at any depth, as if it began with \
                 **/: *.rs is where the .rs files are. \
                 A directory whose name starts with a dot, and node_modules and target, are \
                 entered only when the pattern names that directory exactly -- dot and all, no \
                 wildcards: they are whole cache, vendor and build trees, and a pattern like \
                 **/*.rs is not asking about them. \
                 The answer is the absolute path of every file that matched, most recently \
                 modified first, ties broken by path. Files only: a directory is not listed, and \
                 nothing about a file's contents is read -- searching inside files is a Bash \
                 call (grep, rg). At most 100 paths and 10240 bytes are listed, and the walk \
                 stops after examining 100000 entries; when a cap cut the answer short, the \
                 last line says so. Finding nothing is not a failure: the answer is a sentence \
                 saying so, and the pattern or the path is what to change. A fault in the call \
                 -- a pattern that is absolute, ends at a directory or goes up one, or a path \
                 that is not there or is not a directory -- is answered with what to write \
                 instead."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "the glob matched against each path below path; * and ? stop at a /, ** stands for any number of directories, and a pattern with no / is asked at any depth" },
                    "path": { "type": "string", "description": "the directory searched, and the one the pattern's paths are relative to; default the working directory" }
                },
                "required": ["pattern"]
            })),
        },
    }
}

/// Glob: the files under `path` that match `pattern`.
pub fn glob(args_json: &str) -> String {
    let v = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pattern = match required_string(&v, "pattern") {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    let path = match v.get("path") {
        None => PathBuf::from("."),
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => PathBuf::from(s),
        Some(serde_json::Value::String(_)) => {
            return "error: path must not be empty; leave it out to search the working directory"
                .into();
        }
        Some(_) => return "error: path must be a string".into(),
    };
    let root = match absolute(&path) {
        Ok(root) => root,
        Err(e) => return e,
    };
    run(&root, &pattern, Limits::default())
}

/// The call on a directory and a pattern that are already decided.
///
/// The directory is checked before the pattern is compiled, because a fault in
/// the world is worth naming over a fault in the call: a call that asks about a
/// directory which is not there is answered with that, and the next call is
/// written from it.
fn run(root: &Path, pattern: &str, limits: Limits) -> String {
    match std::fs::metadata(root) {
        Err(e) => return format!("error: cannot access {}: {e}", root.display()),
        Ok(meta) if !meta.is_dir() => {
            return format!(
                "error: {} is not a directory; a pattern is matched against the files below one",
                root.display()
            );
        }
        Ok(_) => {}
    }
    let compiled = match Pattern::parse(pattern) {
        Ok(compiled) => compiled,
        Err(e) => return e,
    };
    let found = walk(root, &compiled, limits);
    render(root, pattern, limits, found)
}

/// The directory a call searches, as an absolute path.
///
/// An answer is a list of paths the next call is written from, and a relative
/// one would be read against a working directory only this process knows. The
/// `.` components a path picks up on the way are dropped as well: `/repo/./src`
/// is the same directory as `/repo/src` and says so worse.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    let cwd = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("error: cannot read the working directory: {e}"))?
    };
    Ok(cwd
        .components()
        .chain(path.components())
        .filter(|c| !matches!(c, PathComponent::CurDir))
        .collect())
}

/// A compiled pattern: the segments of the call's glob.
#[derive(Debug)]
struct Pattern {
    segments: Vec<Segment>,
}

/// One segment of a pattern, which is one name's worth of path.
#[derive(Debug, PartialEq)]
enum Segment {
    /// `**`: any number of whole names, including none at all.
    Deep,
    /// One name, matched by a [`Component`].
    Name(Component),
}

impl Segment {
    /// Whether this segment takes a name that is `name`.
    ///
    /// A `**` matches no name by itself: it is the walk that lets it take them,
    /// by staying on it.
    fn matches(&self, name: &str) -> bool {
        match self {
            Segment::Deep => false,
            Segment::Name(component) => component.matches(name),
        }
    }

    /// Whether this segment is exactly `name`, wildcards and all: what "the
    /// pattern names that directory" means for the directories a walk holds
    /// back from by default.
    fn names(&self, name: &str) -> bool {
        matches!(self, Segment::Name(component) if component.names(name))
    }

    /// Whether this segment may be the one that enters a directory named
    /// `name`. Any of them may, except that the walk holds back from the dotted
    /// and the heavy directories unless a segment names them exactly -- and a
    /// `**` names nothing.
    fn opens(&self, name: &str) -> bool {
        !held_back(name) || self.names(name)
    }
}

impl Pattern {
    /// Compile a pattern. Every fault is answered with what to write instead:
    /// a pattern is written by the model, and a fault it cannot read is a fault
    /// it cannot correct.
    fn parse(pattern: &str) -> Result<Self, String> {
        if pattern.starts_with('/') {
            return Err(format!(
                "error: pattern {pattern:?} is absolute; a pattern is matched against the paths \
                 below the directory searched, so name that directory with path and keep the \
                 pattern relative"
            ));
        }
        if pattern.ends_with('/') {
            return Err(format!(
                "error: pattern {pattern:?} ends at a directory, and this tool lists files; ask \
                 for what is inside it (e.g. {inside:?})",
                inside = format!("{pattern}*")
            ));
        }
        let mut segments = Vec::new();
        for part in pattern.split('/') {
            match part {
                // A `.` is where the pattern already is and a doubled slash is
                // one slash; both are written by hand often enough to take.
                "" | "." => continue,
                ".." => {
                    return Err(format!(
                        "error: pattern {pattern:?} goes up a directory; name that directory with \
                         path instead"
                    ));
                }
                "**" => segments.push(Segment::Deep),
                name => segments.push(Segment::Name(Component::parse(name))),
            }
        }
        if segments.is_empty() {
            return Err(format!(
                "error: pattern {pattern:?} names no file; a pattern ends at the name of what is \
                 being looked for (e.g. \"*.rs\")"
            ));
        }
        // A pattern with no `/` in it is asked at any depth. A bare `*.rs` is
        // a question about where the files named like that are, and the answer
        // a writer expects is not "the ones in the top directory": the top
        // directory is rarely where a file is.
        if !pattern.contains('/') {
            segments.insert(0, Segment::Deep);
        }
        Ok(Self { segments })
    }
}

/// One name's worth of pattern: the tokens matched against one path component,
/// which is as much of a path as a `/`-separated name ever sees.
#[derive(Debug, PartialEq)]
struct Component(Vec<Token>);

/// One token of a name's pattern.
#[derive(Debug, PartialEq)]
enum Token {
    /// `*`: any run of characters.
    Star,
    /// `?`: exactly one character.
    Any,
    /// A character that stands for itself, after a backslash or without one.
    Literal(char),
    /// `[...]`: one character out of a set, or, negated, out of its complement.
    Class { negated: bool, items: Vec<Item> },
}

/// One member of a character class.
#[derive(Debug, PartialEq)]
enum Item {
    Char(char),
    Range(char, char),
}

impl Component {
    fn parse(name: &str) -> Self {
        let chars: Vec<char> = name.chars().collect();
        let mut tokens = Vec::with_capacity(chars.len());
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '*' => {
                    tokens.push(Token::Star);
                    i += 1;
                }
                '?' => {
                    tokens.push(Token::Any);
                    i += 1;
                }
                '[' => match class(&chars, i) {
                    Some((token, next)) => {
                        tokens.push(token);
                        i = next;
                    }
                    // A `[` with no class behind it is a `[`: a name may hold
                    // one, and a pattern that reads as what it is written as is
                    // a pattern its writer can predict.
                    None => {
                        tokens.push(Token::Literal('['));
                        i += 1;
                    }
                },
                '\\' if i + 1 < chars.len() => {
                    tokens.push(Token::Literal(chars[i + 1]));
                    i += 2;
                }
                // A backslash with nothing after it is a backslash, for the
                // same reason a lone `[` is a bracket.
                c => {
                    tokens.push(Token::Literal(c));
                    i += 1;
                }
            }
        }
        Self(tokens)
    }

    /// Whether one name is matched by every token, in order.
    ///
    /// The only token of a length nobody wrote down is `*`, and the walk over
    /// the two sequences is the classic backtracking match: the star last seen
    /// is remembered, and a name that fails after it hands that star one more
    /// character and tries again from there. A star is what makes this
    /// quadratic at worst; a name is short, and a pattern that is all stars
    /// never backtracks at all.
    fn matches(&self, name: &str) -> bool {
        let text: Vec<char> = name.chars().collect();
        let tokens = &self.0;
        let (mut t, mut p) = (0, 0);
        // Where the last `*` is and how much of the name it has taken so far.
        let mut star: Option<(usize, usize)> = None;
        while t < text.len() {
            match tokens.get(p) {
                Some(Token::Star) => {
                    star = Some((p, t));
                    p += 1;
                }
                Some(token) if token.matches_char(text[t]) => {
                    t += 1;
                    p += 1;
                }
                _ => match star {
                    Some((sp, st)) => {
                        p = sp + 1;
                        t = st + 1;
                        star = Some((sp, st + 1));
                    }
                    None => return false,
                },
            }
        }
        while matches!(tokens.get(p), Some(Token::Star)) {
            p += 1;
        }
        p == tokens.len()
    }

    /// Whether this component is exactly `name`, wildcards and all: what "the
    /// pattern names that directory" means for the directories a walk holds
    /// back from by default.
    fn names(&self, name: &str) -> bool {
        let mut chars = name.chars();
        self.0.iter().all(|token| match token {
            Token::Literal(c) => chars.next() == Some(*c),
            Token::Star | Token::Any | Token::Class { .. } => false,
        }) && chars.next().is_none()
    }
}

impl Token {
    /// Whether one character of a name is this token. A star is never asked:
    /// how much of the name it takes is the match loop's business.
    fn matches_char(&self, c: char) -> bool {
        match self {
            Token::Star | Token::Any => true,
            Token::Literal(l) => *l == c,
            Token::Class { negated, items } => {
                items.iter().any(|item| item.contains(c)) != *negated
            }
        }
    }
}

impl Item {
    fn contains(&self, c: char) -> bool {
        match self {
            Item::Char(l) => *l == c,
            Item::Range(from, to) => *from <= c && c <= *to,
        }
    }
}

/// Read a character class out of a pattern, `[` at `start`: the token, and the
/// index to go on from.
///
/// `None` when there is no class there -- no `]` to close it, or nothing
/// between the two: the caller reads the `[` as the character it is rather than
/// as a class nobody wrote.
fn class(chars: &[char], start: usize) -> Option<(Token, usize)> {
    let mut i = start + 1;
    let negated = matches!(chars.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let body = i;
    while i < chars.len() && chars[i] != ']' {
        i += 1;
    }
    if i >= chars.len() || i == body {
        return None;
    }
    let mut items = Vec::new();
    let mut j = body;
    while j < i {
        // `a-c` is a range; a `-` with nothing on one side of it is a `-`.
        if j + 2 < i && chars[j + 1] == '-' {
            items.push(Item::Range(chars[j], chars[j + 2]));
            j += 3;
        } else {
            items.push(Item::Char(chars[j]));
            j += 1;
        }
    }
    Some((Token::Class { negated, items }, i + 1))
}

/// Whether a file named `name` is what the rest of a pattern asks for.
///
/// A file ends a path, so the rest of the pattern has to be spent on the file's
/// own name: the `**`s before it may match no directory at all, a `**` left
/// after a name has been matched is spent on nothing, and a `**` with nothing
/// but `**`s after it matches the file itself -- which is what makes `src/**`
/// every file below `src`.
fn leaf(rest: &[Segment], name: &str) -> bool {
    let spent = |tail: &[Segment]| tail.iter().all(|s| matches!(s, Segment::Deep));
    match rest.split_first() {
        // Unreachable as the walk is written -- a state is never entered with
        // nothing left to match -- and kept so the function is total.
        None => false,
        Some((Segment::Deep, tail)) => leaf(tail, name) || spent(tail),
        Some((Segment::Name(component), tail)) => component.matches(name) && spent(tail),
    }
}

/// One file that matched: what the answer prints, and what it is sorted by.
#[derive(Debug)]
struct Hit {
    path: PathBuf,
    /// When the file was last modified, if the filesystem says.
    modified: Option<SystemTime>,
}

/// What a walk came back with: the files that matched, and the one thing about
/// the walk that the list of paths itself cannot say.
#[derive(Debug, Default)]
struct Found {
    /// One entry per way the walk matched a file. A `**` can match the path to
    /// one file in more than one alignment -- taking the directories on the way
    /// to it, or leaving them to the segment named after them -- so the same
    /// file can be reached twice; [`render`] is where the ways are folded back
    /// into the one answer.
    hits: Vec<Hit>,
    /// The walk stopped at its entry budget with the tree unwalked.
    over: bool,
}

/// Walk the tree below `root` for the files `pattern` asks for.
///
/// A worklist rather than a recursion: a filesystem can be deeper than a stack,
/// and this runs in the middle of a turn, where an overflow is a crash. A state
/// is a directory paired with the segment index its path has been matched up to
/// -- a `**` at that index, if there is one, being the segment that took the
/// last names of the path. A pair is entered once: every `**` in a pattern
/// multiplies the ways of reaching the same directory, and without that memo a
/// pattern with a few of them in it would be a walk of its own, exponential in
/// its length.
fn walk(root: &Path, pattern: &Pattern, limits: Limits) -> Found {
    let segments = &pattern.segments;
    let mut found = Found::default();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut seen: HashSet<(PathBuf, usize)> = HashSet::new();
    let mut examined = 0usize;
    while let Some((dir, at)) = pending.pop() {
        if !seen.insert((dir.clone(), at)) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // A directory that cannot be listed is a corner of the tree this
            // call does not see into; the walk goes on without it rather than
            // failing over ground nobody named.
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            if examined == limits.entries {
                found.over = true;
                return found;
            }
            examined += 1;
            let name = entry.file_name();
            // A name that is not text cannot be written into a result the model
            // reads back as a path, and a path it cannot write is not a file it
            // can ask for.
            let Some(name) = name.to_str() else { continue };
            let path = dir.join(name);
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // A symlink is read only to ask what it names: a link to a file is
            // a path one can open, and a link to a directory is never walked --
            // a link that points back up the tree is a walk with no end.
            if kind.is_symlink() {
                if let Ok(meta) = std::fs::metadata(&path)
                    && meta.is_file()
                {
                    found.hits.push(Hit {
                        path,
                        modified: meta.modified().ok(),
                    });
                }
                continue;
            }
            if kind.is_dir() {
                push_states(segments, at, name, &path, &mut pending);
            } else if kind.is_file() && leaf(&segments[at..], name) {
                found.hits.push(Hit {
                    modified: entry.metadata().ok().and_then(|m| m.modified().ok()),
                    path,
                });
            }
        }
    }
    found
}

/// Add the states a directory named `name` opens up for the pattern, if it
/// opens any.
///
/// A state is a directory and the segment index its path has been matched up
/// to. A segment that names a name is one index further along once it has taken
/// one, and a `**` is a segment the walk may stay on while it takes any number
/// of names -- which is why a run of `**`s gives one state per `**` in it (any
/// of them may be the one that took this name), and the segment after the run
/// gives one more (the `**`s took nothing at all, and that segment took the
/// name itself).
fn push_states(
    segments: &[Segment],
    at: usize,
    name: &str,
    dir: &Path,
    pending: &mut Vec<(PathBuf, usize)>,
) {
    let mut k = at;
    while let Some(Segment::Deep) = segments.get(k) {
        // A `**` names nothing, so it never enters a held-back directory.
        if !held_back(name) {
            pending.push((dir.to_path_buf(), k));
        }
        k += 1;
    }
    if let Some(segment) = segments.get(k)
        && k + 1 < segments.len()
        && segment.matches(name)
        && segment.opens(name)
    {
        pending.push((dir.to_path_buf(), k + 1));
    }
}

/// Whether the walk keeps out of a directory of this name unless the pattern
/// names it exactly: the dotted ones -- caches, virtualenvs, and the history of
/// the repository itself -- and the two vendored or generated trees a workspace
/// of the language they belong to holds.
fn held_back(name: &str) -> bool {
    name.starts_with('.') || HEAVY.contains(&name)
}

/// The answer: the paths, newest first, and a note when a cap cut them short.
fn render(root: &Path, pattern: &str, limits: Limits, mut found: Found) -> String {
    // Newest first: the file just written is the one a call is usually looking
    // for, and the one the user was speaking about last. Ties go by path, so
    // the same tree gives the same answer every time.
    found.hits.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    // A file the walk reached by more than one alignment of the pattern is one
    // file, and the answer says so once: the count in the note below is a count
    // of files, and the list is a list of them.
    found.hits.dedup_by(|a, b| a.path == b.path);
    if found.hits.is_empty() {
        let (pattern, _) = truncate(pattern, PATTERN_COLUMNS);
        let mut out = format!("no files match {pattern} under {}", root.display());
        if found.over {
            out.push_str(&over_note(limits));
        }
        return out;
    }
    let mut out = String::new();
    // The note is written outside this room and fits in it: the list may take
    // what the cap leaves once the note's own bytes are held back.
    let room = limits.bytes.saturating_sub(NOTE_BYTES);
    let mut listed = 0usize;
    for hit in &found.hits {
        if listed == limits.matches {
            break;
        }
        let line = hit.path.to_string_lossy();
        let cost = line.len() + usize::from(listed > 0);
        if out.len() + cost > room {
            break;
        }
        if listed > 0 {
            out.push('\n');
        }
        out.push_str(&line);
        listed += 1;
    }
    if listed < found.hits.len() {
        out.push_str(&format!(
            "\n[showing the {listed} newest of {} matches; narrow the pattern or the path]",
            found.hits.len()
        ));
    }
    if found.over {
        out.push_str(&over_note(limits));
    }
    out
}

/// What the answer says when the walk stopped before the tree ran out.
fn over_note(limits: Limits) -> String {
    format!(
        "\n[the walk stopped after examining {} entries; narrow the path]",
        limits.entries
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-glob-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A file in a tree, with the directories on the way made for it and, when
    /// a test needs an order the filesystem's own clock is too coarse to give,
    /// the modification time it is to have.
    fn file(root: &Path, name: &str, modified: Option<SystemTime>) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "x").unwrap();
        if let Some(time) = modified {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
        path
    }

    /// A moment `secs` after the epoch, for the mtimes the tests make up.
    fn at(secs: u64) -> SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    /// The paths an answer lists, in the order it lists them.
    fn listed(out: &str) -> Vec<PathBuf> {
        out.lines()
            .filter(|line| !line.starts_with('['))
            .filter(|line| !line.starts_with("no files match"))
            .map(PathBuf::from)
            .collect()
    }

    /// One name matched by one component, for the tests whose subject is the
    /// matcher rather than the walk.
    fn m(pattern: &str, name: &str) -> bool {
        Component::parse(pattern).matches(name)
    }

    #[test]
    fn a_star_takes_any_run_of_characters() {
        assert!(m("*.rs", "a.rs"));
        assert!(m("*.rs", ".rs"));
        assert!(!m("*.rs", "a.rss"));
        assert!(m("a*c", "ac"));
        assert!(m("a*c", "abbbc"));
        assert!(!m("a*c", "acb"));
        // Backtracking: the earlier star has to give characters back to the
        // later one.
        assert!(m("*a*b", "xaybzab"));
        assert!(m("*a*a*", "aa"));
        assert!(!m("*a*a*", "a"));
    }

    #[test]
    fn a_question_mark_is_exactly_one_character() {
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(!m("a?c", "abbc"));
        assert!(m("?", "a"));
        assert!(!m("?", ""));
    }

    #[test]
    fn a_class_matches_a_set_and_the_complement_of_one() {
        assert!(m("[abc]x", "ax"));
        assert!(!m("[abc]x", "dx"));
        assert!(m("[a-c]x", "bx"));
        assert!(!m("[a-c]x", "dx"));
        assert!(m("[!a-c]x", "dx"));
        assert!(!m("[!a-c]x", "bx"));
        assert!(m("[^a-c]x", "dx"));
        // A `-` with nothing on one side of it is a `-`.
        assert!(m("[a-]x", "-x"));
        assert!(m("[-a]x", "-x"));
        // A `?` inside a class is a character of the set, not a wildcard.
        assert!(m("[?]x", "?x"));
        assert!(!m("[?]x", "ax"));
    }

    #[test]
    fn a_class_that_never_closes_is_the_bracket_it_is() {
        assert!(m("a[b", "a[b"));
        assert!(m("a[]b", "a[]b"));
        assert!(m("a[!b", "a[!b"));
    }

    #[test]
    fn a_backslash_makes_the_next_character_literal() {
        assert!(m(r"a\*b", "a*b"));
        assert!(!m(r"a\*b", "axb"));
        assert!(m(r"\?x", "?x"));
        assert!(!m(r"\?x", "ax"));
        // A backslash with nothing after it is a backslash.
        assert!(m(r"a\", "a\\"));
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert!(m("*.rs", "A.rs"));
        assert!(!m("*.RS", "a.rs"));
    }

    #[test]
    fn a_pattern_without_a_slash_is_asked_at_any_depth() {
        let dir = tmpdir();
        let top = file(&dir, "a.rs", None);
        let deep = file(&dir, "x/y/b.rs", None);
        file(&dir, "x/c.txt", None);
        let out = run(&dir, "*.rs", Limits::default());
        let mut got = listed(&out);
        got.sort();
        let mut want = vec![top, deep];
        want.sort();
        assert_eq!(got, want, "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_star_in_a_name_does_not_cross_a_directory() {
        let dir = tmpdir();
        let direct = file(&dir, "src/a.rs", None);
        file(&dir, "src/x/b.rs", None);
        let out = run(&dir, "src/*.rs", Limits::default());
        assert_eq!(listed(&out), vec![direct], "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_deep_segment_matches_no_directory_at_all() {
        let dir = tmpdir();
        let direct = file(&dir, "src/a.rs", None);
        let deep = file(&dir, "src/x/y/b.rs", None);
        let mut got = listed(&run(&dir, "src/**/*.rs", Limits::default()));
        got.sort();
        let mut want = vec![direct, deep];
        want.sort();
        assert_eq!(got, want);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `src/**` is every file below `src`: a `**` at the end takes the names of
    /// the files themselves and not only of the directories on the way to them.
    #[test]
    fn a_deep_segment_at_the_end_is_every_file_below() {
        let dir = tmpdir();
        let direct = file(&dir, "src/a.rs", None);
        let deep = file(&dir, "src/x/b.txt", None);
        let mut got = listed(&run(&dir, "src/**", Limits::default()));
        got.sort();
        let mut want = vec![direct, deep];
        want.sort();
        assert_eq!(got, want);
        // The bare `**` is the same ask from the root, and a directory is
        // never one of the answers: this tool lists files.
        let mut from_root = listed(&run(&dir, "**", Limits::default()));
        from_root.sort();
        assert_eq!(from_root, want);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A directory whose name matches is not an answer: this tool lists the
    /// files one can be read from, and the pattern for the files inside a
    /// directory is the pattern with that directory in front of it.
    #[test]
    fn a_directory_is_not_listed_even_when_the_pattern_matches_it() {
        let dir = tmpdir();
        file(&dir, "src.rs/inside.txt", None);
        let out = run(&dir, "*.rs", Limits::default());
        assert!(listed(&out).is_empty(), "{out}");
        assert!(out.starts_with("no files match"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_paths_are_absolute_and_the_newest_ones_come_first() {
        let dir = tmpdir();
        let old = file(&dir, "old.rs", Some(at(1_000)));
        let new = file(&dir, "new.rs", Some(at(2_000)));
        let mid = file(&dir, "mid.rs", Some(at(1_500)));
        let out = run(&dir, "*.rs", Limits::default());
        assert_eq!(listed(&out), vec![new, mid, old], "{out}");
        // A tie goes by path, so one tree gives one answer.
        let same = tmpdir();
        let b = file(&same, "b.rs", Some(at(5)));
        let a = file(&same, "a.rs", Some(at(5)));
        assert_eq!(listed(&run(&same, "*.rs", Limits::default())), vec![a, b]);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&same).unwrap();
    }

    /// The dotted and the heavy directories are not what a pattern like
    /// `**/*.rs` is asking about, and a pattern that names one exactly is: the
    /// rule is one rule, and both halves of it are the same half of it.
    #[test]
    fn the_walk_holds_back_from_a_dotted_or_heavy_directory_unless_named() {
        let dir = tmpdir();
        let source = file(&dir, "src/a.rs", None);
        let cache = file(&dir, ".git/x.rs", None);
        let modules = file(&dir, "node_modules/y.rs", None);
        let built = file(&dir, "target/z.rs", None);
        assert_eq!(
            listed(&run(&dir, "**/*.rs", Limits::default())),
            vec![source],
            "the walk keeps out of what the pattern is not naming"
        );
        assert_eq!(
            listed(&run(&dir, ".git/*.rs", Limits::default())),
            vec![cache]
        );
        assert_eq!(
            listed(&run(&dir, "node_modules/**", Limits::default())),
            vec![modules]
        );
        // A `**` in front of the name is not the name, but the segment that
        // does name it is there to match it, and the directory is reached.
        assert_eq!(
            listed(&run(&dir, "**/target/*.rs", Limits::default())),
            vec![built]
        );
        // A wildcard is not the name either.
        assert!(listed(&run(&dir, "t*rget/*.rs", Limits::default())).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The directory a call names is searched however it is named: holding back
    /// is about what a walk stumbles into on its way, not about where it was
    /// told to start.
    #[test]
    fn the_root_is_searched_even_when_its_name_is_held_back() {
        let dir = tmpdir();
        let built = file(&dir, "target/z.rs", None);
        assert_eq!(
            listed(&run(&dir.join("target"), "**/*.rs", Limits::default())),
            vec![built]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_to_a_file_is_a_match_and_a_symlink_to_a_directory_is_not_walked() {
        let dir = tmpdir();
        let real = file(&dir, "real/a.rs", None);
        let link = dir.join("linked.rs");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A link to a directory, and a link back up the tree inside the tree
        // itself: walking either would be a walk with no end.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("sub/back")).unwrap();
        let mut got = listed(&run(&dir, "**/*.rs", Limits::default()));
        got.sort();
        let mut want = vec![real, link];
        want.sort();
        assert_eq!(got, want);
        // A link that names nothing is not a file either.
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("broken.rs")).unwrap();
        let mut after = listed(&run(&dir, "**/*.rs", Limits::default()));
        after.sort();
        assert_eq!(after, want);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file name that is not text cannot be handed back as a path the model
    /// can write into the next call, so it is not listed.
    #[test]
    #[cfg(unix)]
    fn a_file_name_that_is_not_text_is_not_listed() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tmpdir();
        std::fs::write(dir.join(std::ffi::OsStr::from_bytes(b"a\xffb.rs")), "x").unwrap();
        let text = file(&dir, "ok.rs", None);
        assert_eq!(listed(&run(&dir, "*.rs", Limits::default())), vec![text]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_matching_is_a_sentence_and_not_an_error() {
        let dir = tmpdir();
        file(&dir, "a.rs", None);
        let out = run(&dir, "*.zzz", Limits::default());
        assert_eq!(out, format!("no files match *.zzz under {}", dir.display()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_listing_is_capped_and_says_how_many_matches_were_left() {
        let dir = tmpdir();
        for i in 0..5 {
            file(&dir, &format!("f{i}.rs"), Some(at(i)));
        }
        let limits = Limits {
            matches: 2,
            ..Limits::default()
        };
        let out = run(&dir, "*.rs", limits);
        assert_eq!(listed(&out).len(), 2, "{out}");
        assert!(
            out.ends_with("[showing the 2 newest of 5 matches; narrow the pattern or the path]"),
            "{out}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The notes ride inside the byte cap as well: an answer is never longer
    /// than the cap, whether the cap cut the list or the walk.
    #[test]
    fn the_answer_stays_inside_its_byte_cap() {
        let dir = tmpdir();
        for i in 0..20 {
            file(&dir, &format!("file-with-a-long-name-{i:02}.rs"), None);
        }
        let limits = Limits {
            bytes: 400,
            ..Limits::default()
        };
        let out = run(&dir, "*.rs", limits);
        assert!(out.len() <= 400, "{} bytes: {out}", out.len());
        assert!(!listed(&out).is_empty(), "{out}");
        assert!(out.contains("showing the "), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The budget is what makes a call's cost bounded rather than the tree's,
    /// and an answer that ran into it says so: the files it did not get to are
    /// files the model must not read its answer as a statement about.
    #[test]
    fn the_walk_stops_at_its_budget_and_says_so() {
        let dir = tmpdir();
        for i in 0..10 {
            file(&dir, &format!("f{i}.txt"), None);
        }
        let limits = Limits {
            entries: 3,
            ..Limits::default()
        };
        let out = run(&dir, "*.txt", limits);
        assert!(out.contains("stopped after examining 3 entries"), "{out}");
        // A walk that found nothing at all still says it did not finish: an
        // empty answer that stopped early is not an empty tree.
        let out = run(&dir, "*.zzz", limits);
        assert!(out.starts_with("no files match"), "{out}");
        assert!(out.contains("stopped after examining 3 entries"), "{out}");
        // A walk that got to the end of the tree says nothing of the sort.
        let all = run(&dir, "*.txt", Limits::default());
        assert_eq!(listed(&all).len(), 10, "{all}");
        assert!(!all.contains("stopped"), "{all}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_leading_dot_slash_and_a_doubled_slash_are_one_slash() {
        let dir = tmpdir();
        let hit = file(&dir, "src/a.rs", None);
        for pattern in ["./src/*.rs", "src//*.rs", "src/./a.rs"] {
            assert_eq!(
                listed(&run(&dir, pattern, Limits::default())),
                vec![hit.clone()],
                "{pattern}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A pattern is written by the model, so a fault in it is answered with
    /// what to write instead rather than with a walk that matches nothing.
    #[test]
    fn a_faulty_pattern_is_answered_with_what_to_write_instead() {
        let dir = tmpdir();
        let cases = [
            ("/etc/*.rs", "absolute"),
            ("src/", "ends at a directory"),
            ("../*.rs", "goes up a directory"),
            (".", "names no file"),
        ];
        for (pattern, want) in cases {
            let out = run(&dir, pattern, Limits::default());
            assert!(out.starts_with("error: "), "{pattern}: {out}");
            assert!(out.contains(want), "{pattern}: {out}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The call's own arguments, and the directory it names: each fault is
    /// answered with the argument that is wrong.
    #[test]
    fn a_faulty_call_is_answered_with_the_argument_that_is_wrong() {
        let dir = tmpdir();
        let root = dir.to_string_lossy().into_owned();
        let cases = [
            (r#"{"path":"/tmp"}"#.to_string(), "pattern"),
            (r#"{"pattern":""}"#.to_string(), "must not be empty"),
            (r#"{"pattern":3}"#.to_string(), "must be a string"),
            (
                r#"{"pattern":"*","path":3}"#.to_string(),
                "path must be a string",
            ),
            (
                r#"{"pattern":"*","path":" "}"#.to_string(),
                "path must not be empty",
            ),
            (
                format!(r#"{{"pattern":"*","path":"{root}/gone"}}"#),
                "cannot access",
            ),
            ("not json".to_string(), "not valid JSON"),
        ];
        for (args, want) in cases {
            let out = glob(&args);
            assert!(out.starts_with("error: "), "{args}: {out}");
            assert!(out.contains(want), "{args}: {out}");
        }
        // A path that names a file is not a directory to search.
        let f = file(&dir, "a.rs", None);
        let out = glob(&format!(
            r#"{{"pattern":"*","path":{:?}}}"#,
            f.to_string_lossy()
        ));
        assert!(out.contains("is not a directory"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// No `path` is the working directory, which is the one directory a call
    /// may leave unnamed.
    #[test]
    fn a_call_without_a_path_searches_the_working_directory() {
        let out = glob(r#"{"pattern":"Cargo.toml"}"#);
        let paths = listed(&out);
        assert!(
            paths.iter().all(|p| p.is_absolute() && p.is_file()),
            "{out}"
        );
        assert!(
            paths.contains(&std::env::current_dir().unwrap().join("Cargo.toml")),
            "{out}"
        );
    }

    /// A path is answered with as an absolute one, with the `.` components it
    /// was written with dropped: these are the paths the next call is written
    /// from.
    #[test]
    fn a_path_is_resolved_against_the_working_directory() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolute(Path::new("./src")).unwrap(), cwd.join("src"));
        assert_eq!(
            absolute(Path::new("/etc/./passwd")).unwrap(),
            PathBuf::from("/etc/passwd")
        );
        assert_eq!(absolute(&cwd).unwrap(), cwd);
    }
}
