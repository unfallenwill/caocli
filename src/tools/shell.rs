use serde_json::json;
use std::collections::VecDeque;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::MAX_OUTPUT;
use crate::types::{FunctionDef, ToolDef};

pub const NAME: &str = "Bash";
pub const TIMEOUT_SECS: u64 = 120;

/// Bytes of a command's output kept for the result, per stream and per end.
///
/// The two ends are what a result has to carry: a command that cannot start says
/// so in its first lines, a build that fails says why in its last ones, and nobody
/// reads byte 400000 of a log. Keeping both costs one cap and no memory -- the
/// middle is let go of as it arrives rather than held back and cut later -- and
/// what was let go of is counted where it was, so a reader knows how much is
/// missing from between the two ends instead of reading the tail as if it followed
/// the head.
const KEEP: usize = MAX_OUTPUT / 2;

/// How much is read from a pipe at a time.
const CHUNK: usize = 8 * 1024;

/// How long a command's output is waited for once the command itself is gone.
///
/// A process the command left behind can hold its pipes open for as long as it
/// lives, and a result is not worth waiting on that: measured, `sleep 30 &` used to
/// cost the call thirty seconds, because the shell exits at once and its output is
/// not at its end until the last process holding it is gone.
const DRAIN_SECS: u64 = 1;

/// How long a killed child is given to be reaped before the result is written
/// without it.
///
/// SIGKILL cannot be caught, so this is about a process that cannot die where it
/// stands (an uninterruptible wait) rather than about one that will not: a child
/// that is still not reaped is left to the runtime's own reaper, and the group is
/// killed again when this module lets go of it.
const REAP_SECS: u64 = 1;

/// The code a command that ran out of time is reported with, as `timeout(1)` does.
const TIMEOUT_CODE: i32 = 124;

pub fn definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: NAME.into(),
            description: Some(
                "Run one bash command on the local machine; returns exit_code, stdout and stderr \
                 (returned separately). \
                 Every call is a fresh shell: the working directory and environment variables do \
                 not persist, so to target a directory write cd /abs/path && ... inside the same \
                 command, and prefer absolute paths. \
                 Use for: running programs, builds, tests, git, directory operations, bulk text \
                 processing. \
                 Not for: reading a text file (use Read), modifying an existing file (use Edit), \
                 creating or fully rewriting a file (use Write); do not substitute cat/sed -i/tee \
                 for them. \
                 Each stream is kept to 10240 bytes: a longer one is shown from both of its ends, \
                 with what fell between them counted in its place. \
                 Do not run something that waits for a terminal (an editor, a pager, top) or a \
                 long-lived command (a server, watch, tail -f): neither can work here, and a \
                 command that runs past the 120s timeout is killed with everything it started."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "the bash command to run" }
                },
                "required": ["command"]
            })),
        },
    }
}

/// Run a shell command. Never returns Err: bad arguments, command failure and
/// timeouts all come back as tool result text.
pub async fn execute(args_json: &str) -> String {
    let command = match super::parse_args(args_json) {
        Ok(v) => v.get("command").and_then(|c| c.as_str()).map(str::to_owned),
        Err(e) => return e,
    };
    let Some(command) = command else {
        return "error: missing required argument command (string)".into();
    };

    run(&command, Duration::from_secs(TIMEOUT_SECS)).await
}

/// One command, from `bash -c` to the text the model reads.
///
/// Everything awaited here is awaited on a budget: the command's own, the grace its
/// output is given after it, and the time a killed child is given to be reaped.
/// Nothing can wait forever, and what the command printed is never thrown away --
/// a timeout used to answer with a note where the output should have been.
async fn run(command: &str, budget: Duration) -> String {
    let mut group = match Group::spawn(command) {
        Ok(group) => group,
        Err(e) => return format!("exit_code: 127\n--- stderr ---\nfailed to start bash: {e}"),
    };
    let pgroup = group.pgroup;
    let stdout = group.child.stdout.take().expect("stdout is piped");
    let stderr = group.child.stderr.take().expect("stderr is piped");

    // One channel for both pipes: the result keeps the two streams apart, but the
    // order they arrived in is the order the command wrote them.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out_kept = Arc::new(Mutex::new(Kept::new()));
    let err_kept = Arc::new(Mutex::new(Kept::new()));
    let out_read = tokio::spawn(read(stdout, tx.clone(), Arc::clone(&out_kept)));
    let err_read = tokio::spawn(read(stderr, tx, Arc::clone(&err_kept)));

    let mut deadline = Box::pin(tokio::time::sleep(budget));
    let mut timed_out = false;
    let mut ended = false;
    let code = loop {
        tokio::select! {
            biased;
            // A chunk that has already been read: the readers keep the bytes, so
            // nothing is lost by not looking at it here.
            chunk = rx.recv(), if !ended => ended = chunk.is_none(),
            status = group.wait() => break status.ok().and_then(|s| s.code()),
            _ = &mut deadline => {
                if timed_out {
                    // It did not die where it stood: leave it to the runtime's own
                    // reaper rather than hold the result for it.
                    break None;
                }
                timed_out = true;
                kill_group(pgroup);
                deadline.as_mut().reset(Instant::now() + Duration::from_secs(REAP_SECS));
            }
        }
    };

    // What the readers still have, and no longer than the last chunk of it takes to
    // arrive: the channel closing is both pipes reaching their end.
    let held_open = loop {
        match tokio::time::timeout(Duration::from_secs(DRAIN_SECS), rx.recv()).await {
            Ok(Some(_)) => {}
            Ok(None) => break false,
            Err(_) => break true,
        }
    };
    out_read.abort();
    err_read.abort();

    let mut result = format!(
        "exit_code: {}\n",
        if timed_out {
            TIMEOUT_CODE
        } else {
            code.unwrap_or(-1)
        }
    );
    if timed_out {
        result.push_str(&format!(
            "timeout: the command ran longer than {}s and was killed, along with everything it \
             had started; what it printed up to then is below. Use a faster command, or run a \
             long task in the background and poll an output file.\n",
            budget.as_secs()
        ));
    }
    result.push_str("--- stdout ---\n");
    result.push_str(&out_kept.lock().expect("not held across an await").render());
    result.push_str("\n--- stderr ---\n");
    result.push_str(&err_kept.lock().expect("not held across an await").render());
    if held_open {
        result.push_str(
            "\nnote: a process the command started is still running and holding the pipes open, \
             so this result may be short of what it prints.",
        );
    }
    result
}

/// Read one of a command's pipes to its end: every byte is kept for the result.
///
/// The pipe is read even when nobody is watching any more: it has to be, or a
/// command that fills it would block writing where it would otherwise have
/// finished.
async fn read(
    mut pipe: impl AsyncRead + Unpin,
    tx: mpsc::UnboundedSender<String>,
    kept: Arc<Mutex<Kept>>,
) {
    let mut buf = vec![0u8; CHUNK];
    // The bytes of a character split across two reads: held rather than replaced,
    // so that where the kernel happened to break a chunk is not visible in what the
    // command is said to have printed.
    let mut pending: Vec<u8> = Vec::new();
    loop {
        match pipe.read(&mut buf).await {
            // The end, or a read that will never be answered.
            Ok(0) | Err(_) => break,
            Ok(n) => {
                kept.lock()
                    .expect("not held across an await")
                    .push(&buf[..n]);
                pending.extend_from_slice(&buf[..n]);
                let text = text_prefix(&pending);
                let chunk = String::from_utf8_lossy(&pending[..text]).into_owned();
                pending.drain(..text);
                if !chunk.is_empty() {
                    let _ = tx.send(chunk);
                }
            }
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(String::from_utf8_lossy(&pending).into_owned());
    }
}

/// How much of `buf` is text, leaving out the front of a character whose rest has
/// not been read yet.
///
/// Bytes that are not text at all stay in and are replaced when they are converted:
/// only an unfinished character is held back, so the held bytes cannot grow past
/// one character's worth.
fn text_prefix(buf: &[u8]) -> usize {
    match std::str::from_utf8(buf) {
        Ok(_) => buf.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => buf.len(),
    }
}

/// Kill a command's whole process group.
///
/// A shell that has started a build is not the thing to kill: the compiler would
/// carry on with nobody reading what it writes and nobody waiting for it, which is
/// the opposite of what a cancel or a timeout is for. The group is the child's own
/// because [`Group::spawn`] put it in a session of its own -- and where that did not
/// happen the pid names no group at all, so nothing is killed rather than something
/// that is not ours.
fn kill_group(pgroup: Option<u32>) {
    #[cfg(unix)]
    if let Some(pgroup) = pgroup {
        // SAFETY: a signal sent to a process group, which is what SIGKILL to a
        // negative pid means; the group is the one this call spawned.
        unsafe { libc::killpg(pgroup as libc::pid_t, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = pgroup;
}

/// A command and the process group it leads.
///
/// The group is killed when this is dropped without the child having been reaped,
/// which is what a cancelled call is: the future is dropped where it stands, and
/// what the command started goes with it instead of outliving the turn.
struct Group {
    child: Child,
    /// The pid the child was given, which is the group's id for as long as it leads
    /// one.
    pgroup: Option<u32>,
    /// Whether the child was waited on. A child that was reaped has been killed
    /// already if it was ever going to be, and its pid may name somebody else's
    /// group by the time this is dropped.
    reaped: bool,
}

impl Group {
    /// Spawn `bash -c command` in a session of its own, with its output piped and
    /// its input at an end of file.
    ///
    /// The session is what keeps the command away from the user's terminal.
    /// A program that wants to ask the user something opens `/dev/tty` rather than
    /// reading its standard input, and it would find the terminal the front end is
    /// reading: the front end owns the screen and the keys, so a question typed into
    /// it by a command is a question the front end answers, or a keystroke that
    /// never reaches the line being typed. Measured: with the terminal reachable,
    /// `sudo true` waits minutes for a password; without one it fails in a tenth of
    /// a second, which is a result the model can act on.
    fn spawn(command: &str) -> std::io::Result<Self> {
        let mut spawn = Command::new("bash");
        spawn
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        // SAFETY: `setsid` takes no arguments and touches nothing but the child's
        // own process attributes; it is called after the fork, before the exec.
        unsafe {
            spawn.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = spawn.spawn()?;
        Ok(Self {
            pgroup: child.id(),
            child,
            reaped: false,
        })
    }

    /// Wait for the child to exit, and remember that it was waited on.
    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.reaped = true;
        }
        status
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.reaped {
            kill_group(self.pgroup);
        }
    }
}

/// One stream's output as the result will show it.
///
/// The head fills up first and the tail is what follows it, which is what makes the
/// two pieces the whole stream while it fits and its two ends once it does not: the
/// tail only starts once the head is full, so no byte is in both of them, and it is
/// a ring, so what is left when the command stops is the last bytes it wrote rather
/// than the first ones after the head.
struct Kept {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    /// What the ring let go of, and the lines among it: counted as it goes rather
    /// than derived later, because those bytes are not here to be counted.
    dropped: u64,
    dropped_lines: u64,
}

impl Kept {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            dropped: 0,
            dropped_lines: 0,
        }
    }

    /// Take `bytes` of one stream.
    fn push(&mut self, bytes: &[u8]) {
        let taken = (KEEP - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..taken]);
        let rest = &bytes[taken..];
        if rest.is_empty() {
            return;
        }
        // What follows the head is the ring's to keep, and the ring is what the
        // stream ends with: the front of a chunk too big for it is dropped before it
        // is ever in there, the way the front of the ring is dropped to make room
        // for what comes after it. Counting both here is what makes the bytes and
        // the lines of the middle add up to the stream.
        let too_big = rest.len().saturating_sub(KEEP);
        self.count_dropped(&rest[..too_big]);
        let rest = &rest[too_big..];
        let over = (self.tail.len() + rest.len()).saturating_sub(KEEP);
        for _ in 0..over {
            if self.tail.pop_front() == Some(b'\n') {
                self.dropped_lines += 1;
            }
        }
        self.dropped += over as u64;
        self.tail.extend(rest);
    }

    /// Count `bytes` as dropped, lines and all: they never reached the ring, so
    /// what they are has to be read off them here.
    fn count_dropped(&mut self, bytes: &[u8]) {
        self.dropped += bytes.len() as u64;
        self.dropped_lines += bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    }

    /// The stream as the result shows it: all of it when it fits, and otherwise its
    /// two ends with what is missing from between them counted in its place.
    fn render(&self) -> String {
        if self.dropped == 0 {
            let mut whole = self.head.clone();
            whole.extend(self.tail.iter().copied());
            return String::from_utf8_lossy(&whole).into_owned();
        }
        // Both ends of the cut are cut back to whole lines: half a line at the
        // splice would read as a line the command printed, and the two halves of it
        // would be counted as neither shown nor dropped.
        let head = match self.head.iter().rposition(|&b| b == b'\n') {
            Some(i) => &self.head[..=i],
            None => &self.head[..],
        };
        let from_tail = self
            .tail
            .iter()
            .position(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let lost_head = &self.head[head.len()..];
        let lost_tail = self.tail.iter().take(from_tail);
        let bytes = self.dropped + lost_head.len() as u64 + from_tail as u64;
        let lines = self.dropped_lines
            + lost_head.iter().filter(|&&b| b == b'\n').count() as u64
            + lost_tail.filter(|&&b| b == b'\n').count() as u64;

        let mut out = String::from_utf8_lossy(head).into_owned();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[{}]\n", omitted(bytes, lines)));
        let tail: Vec<u8> = self.tail.iter().skip(from_tail).copied().collect();
        out.push_str(&String::from_utf8_lossy(&tail));
        out
    }
}

/// How a result says what it is not showing, with the lines named when there are
/// any to count: a stream cut in the middle of one enormous line has none.
fn omitted(bytes: u64, lines: u64) -> String {
    match lines {
        0 => format!("… {bytes} bytes omitted …"),
        1 => format!("… 1 line ({bytes} bytes) omitted …"),
        n => format!("… {n} lines ({bytes} bytes) omitted …"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn execute_bad_json_returns_error_text() {
        let out = execute("not json").await;
        assert!(out.starts_with("error: arguments are not valid JSON"));
    }

    #[tokio::test]
    async fn execute_missing_command_returns_error_text() {
        let out = execute(r#"{"cmd":"ls"}"#).await;
        assert!(out.starts_with("error: missing required argument command"));
    }

    #[tokio::test]
    async fn execute_runs_command() {
        let out = execute(r#"{"command":"echo hello; echo err >&2; exit 3"}"#).await;
        assert!(out.contains("exit_code: 3"));
        assert!(out.contains("hello"));
        assert!(out.contains("err"));
    }

    /// A short output is the whole output: the head and the tail put back together
    /// are the bytes the command printed, in the order it printed them, and nothing
    /// is said about a middle because there is none.
    #[tokio::test]
    async fn a_short_output_is_kept_whole() {
        let out = execute(r#"{"command":"echo one; echo two; echo three >&2"}"#).await;
        assert!(out.contains("--- stdout ---\none\ntwo\n"), "{out}");
        assert!(out.contains("--- stderr ---\nthree"), "{out}");
        assert!(!out.contains("omitted"), "{out}");
    }

    /// A long one is shown from both ends with the middle counted: what a build
    /// says when it fails is at the end, and cutting the end off is what used to
    /// throw the answer away.
    #[tokio::test]
    async fn a_long_output_keeps_both_ends_and_counts_the_middle() {
        let out =
            execute(r#"{"command":"awk 'BEGIN{for(i=1;i<=4000;i++) printf \"line %d\\n\", i}'"}"#)
                .await;
        assert!(
            out.contains("line 1\n"),
            "the first line is kept: {out:.200}"
        );
        assert!(
            out.contains("line 4000"),
            "the last line is kept: {out:.200}"
        );
        assert!(out.contains("bytes) omitted …"), "{out:.200}");
        assert!(
            out.len() < MAX_OUTPUT + 512,
            "one stream is {} bytes",
            out.len()
        );
    }

    /// A command that runs past its budget is stopped, and what it printed before
    /// that is the result rather than a note saying it was lost.
    #[tokio::test]
    async fn a_timeout_keeps_what_the_command_printed() {
        let out = run("echo before; sleep 30", Duration::from_secs(1)).await;
        assert!(out.contains("exit_code: 124"), "{out}");
        assert!(
            out.contains("timeout: the command ran longer than 1s"),
            "{out}"
        );
        assert!(out.contains("before"), "{out}");
    }

    /// A killed command does not hold the call up: the budget is what it is, and
    /// the result is written when it is spent.
    #[tokio::test]
    async fn a_timeout_does_not_wait_for_the_command() {
        let started = Instant::now();
        let out = run("sleep 30", Duration::from_millis(200)).await;
        assert!(out.contains("exit_code: 124"), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    /// The kill takes the whole group: a build is a shell plus a compiler, and
    /// killing only the shell leaves the compiler running with nobody reading it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_timeout_kills_what_the_command_started() {
        let pidfile = std::env::temp_dir().join(format!("caocli-kill-{}", std::process::id()));
        let command = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let out = run(&command, Duration::from_secs(1)).await;
        assert!(out.contains("exit_code: 124"), "{out}");
        let pid = read_pid(&pidfile);
        assert!(!still_running(pid).await, "{pid} outlived the timeout");
    }

    /// A cancelled call is dropped where it stands: the group goes with the future,
    /// which is what the `Drop` for a child that was never reaped is for.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_call_kills_what_the_command_started() {
        let pidfile = std::env::temp_dir().join(format!("caocli-drop-{}", std::process::id()));
        let command = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            run(&command, Duration::from_secs(30)),
        )
        .await;
        assert!(dropped.is_err(), "the call was dropped, not finished");
        let pid = read_pid(&pidfile);
        assert!(!still_running(pid).await, "{pid} outlived the call");
    }

    /// A process the command leaves behind holds its pipes open, and a result is
    /// not worth waiting for it: measured, this used to cost the call the whole
    /// lifetime of the process that was left.
    #[tokio::test]
    async fn a_process_left_behind_does_not_hold_the_result() {
        let started = Instant::now();
        let out = run("sleep 5 & echo done", Duration::from_secs(30)).await;
        assert!(out.contains("exit_code: 0"), "{out}");
        assert!(out.contains("done"), "{out}");
        assert!(out.contains("still running"), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn kept_is_the_whole_stream_while_it_fits() {
        let mut kept = Kept::new();
        kept.push(b"one\ntwo\n");
        kept.push(b"three");
        assert_eq!(kept.render(), "one\ntwo\nthree");
    }

    /// The two ends and a counted middle, cut back to whole lines on both sides, and
    /// the sum of what is shown and what is said to be missing is the stream.
    #[test]
    fn kept_keeps_the_two_ends_of_a_long_stream() {
        let mut stream = String::new();
        for i in 1..=4000 {
            stream.push_str(&format!("line {i}\n"));
        }
        let mut kept = Kept::new();
        kept.push(stream.as_bytes());
        let out = kept.render();
        assert!(out.starts_with("line 1\n"), "{:.80}", out);
        assert!(out.ends_with("line 4000\n"), "{:.80}", out);
        let (bytes, lines) = counts(&out);
        assert_eq!(bytes + shown(&out) as u64, stream.len() as u64);
        assert_eq!(lines as usize + shown_lines(&out), 4000);
        assert!(out.len() < 2 * KEEP + 96, "{} bytes", out.len());
    }

    /// A stream with no line breaks has no lines to count, and the bytes are then
    /// the whole of what can be said about what was dropped. The one enormous line
    /// is kept whole at both ends rather than cut back to a line boundary there is
    /// none of.
    #[test]
    fn kept_counts_bytes_when_there_are_no_lines_to_count() {
        let mut kept = Kept::new();
        kept.push(&vec![b'x'; 4 * KEEP]);
        let out = kept.render();
        assert!(out.contains("bytes omitted …"), "{out:.200}");
        let (bytes, lines) = counts(&out);
        assert_eq!(lines, 0);
        assert_eq!(bytes, (4 * KEEP - 2 * KEEP) as u64, "the middle is counted");
        assert!(
            out.starts_with("xxxx") && out.ends_with("xxxx"),
            "{out:.80}"
        );
    }

    #[test]
    fn text_prefix_holds_back_a_split_character() {
        // A three-byte character with only its first two bytes read so far.
        let bytes = "a€".as_bytes();
        assert_eq!(text_prefix(&bytes[..bytes.len() - 1]), 1);
        assert_eq!(text_prefix(bytes), bytes.len());
        // Bytes that are not text at all are converted rather than held.
        assert_eq!(text_prefix(&[0x41, 0x80]), 2);
    }

    #[test]
    fn omitted_names_the_lines_it_can_count() {
        assert_eq!(omitted(1024, 0), "… 1024 bytes omitted …");
        assert_eq!(omitted(1024, 1), "… 1 line (1024 bytes) omitted …");
        assert_eq!(omitted(1024, 7), "… 7 lines (1024 bytes) omitted …");
    }

    /// The bytes and lines a rendered stream says it is not showing.
    fn counts(out: &str) -> (u64, u64) {
        let marker = out
            .split("… ")
            .nth(1)
            .expect("a rendered stream counts what it dropped");
        let count = |s: &str| s.parse().expect("a count");
        match marker.split_once(" line") {
            Some((lines, rest)) => {
                let bytes = rest
                    .split_once('(')
                    .and_then(|(_, b)| b.split_once(' '))
                    .map(|(b, _)| count(b))
                    .expect("a byte count");
                (bytes, count(lines))
            }
            None => (count(marker.split_once(' ').expect("a count").0), 0),
        }
    }

    /// How many bytes of a rendered stream are the stream itself rather than the
    /// marker saying what is missing from it, for a stream whose head ends with a
    /// line of its own.
    fn shown(out: &str) -> usize {
        let start = out.find("[… ").expect("a marker");
        let end = out[start..].find("]\n").expect("the marker's end") + start + 2;
        out.len() - (end - start)
    }

    /// How many lines of a rendered stream are the stream itself.
    fn shown_lines(out: &str) -> usize {
        out.matches('\n').count() - 1
    }

    /// A pid written to a file by a command, read once it has been written.
    #[cfg(target_os = "linux")]
    fn read_pid(pidfile: &std::path::Path) -> i32 {
        let pid = std::fs::read_to_string(pidfile)
            .expect("the background pid was written")
            .trim()
            .parse()
            .expect("a pid");
        let _ = std::fs::remove_file(pidfile);
        pid
    }

    /// Whether a process is still running, waiting a moment for a killed one to be
    /// reaped: a killed process is a zombie until something reaps it, and its parent
    /// was killed with it, so "gone" is either of the two states.
    #[cfg(target_os = "linux")]
    async fn still_running(pid: i32) -> bool {
        for _ in 0..40 {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit(')').next().and_then(|s| s.chars().nth(1)));
            match state {
                None | Some('Z') => return false,
                Some(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
        true
    }
}
