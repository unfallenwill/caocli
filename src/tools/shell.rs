use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::MAX_OUTPUT;
use crate::types::{FunctionDef, ToolDef};

pub const NAME: &str = "Bash";

/// How long a call is given when it does not ask for a length of its own.
pub const TIMEOUT_SECS: u64 = 120;

/// The longest a call may ask to be given (seconds).
///
/// A cap rather than a longer default because a turn is a conversation: a call
/// that runs for an hour is an hour in which the user cannot be answered, and the
/// background is the right shape for a job that long -- it costs the turn nothing
/// and its output can be read whenever it is wanted.
pub const MAX_TIMEOUT_SECS: u64 = 1800;

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

/// The programs that draw a screen, and cannot draw one here.
///
/// Measured, with the standard input at an end of file and the output a pipe as
/// every call's is: `vim` starts, warns twice, and then never exits -- the screen
/// it is drawing is not at its end and there is no keyboard to leave it with, so the
/// call runs until the timeout kills it and the result is 10 KB of terminal control
/// codes that changed no file. The same command with its output redirected to a file
/// exits by itself in two seconds, which is why the pipe is what the measurement is
/// made with. Nothing else is refused, because everything else either fails at once
/// on its own (`top: failed tty get`, `sudo: A terminal is required to authenticate`
/// -- both measured in a tenth of a second) or is a long-lived command whose place
/// is the background.
const SCREEN_EDITORS: &[&str] = &[
    "vi", "vim", "nvim", "nano", "pico", "emacs", "micro", "joe", "mcedit",
];

/// The programs whose own program is the one that follows them.
///
/// A call to `sudo vim f` is a call to an editor, and reading it as a call to sudo
/// would be reading the command for what it does not say. What is stepped over is a
/// wrapper and its flags: a wrapper's own argument (`sudo -u root vim f`) is read as
/// the program and hides what follows it, which costs a refusal rather than making
/// one that is wrong.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "nice", "ionice", "time", "command", "exec", "nohup", "setsid",
    "stdbuf", "xargs",
];

/// The flags that ask one of them for the mode where nothing is drawn.
///
/// A screen editor is not refused for existing, but for drawing: `vim -es` reads
/// its commands from where a screen would have typed them, which is a way to edit
/// a file from here (measured: it edits one and prints nothing), and `emacs
/// -batch` is the same idea.
const SCRIPT_FLAGS: &[&str] = &["-es", "-e", "-E", "--batch", "-batch"];

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
                 A command that runs past its timeout is killed, with everything it started: 120s \
                 unless timeout says otherwise (at most 1800). A job that should outlive the call \
                 -- a build, a server, a long test run -- is started with background:true instead: \
                 the call returns at once and the result names the pid and the file the output \
                 goes to. \
                 A screen editor (vim, nano, ...) is refused: nothing a call runs has a terminal, \
                 so one would only draw its interface into the result. Edit a file with Edit/Write, \
                 or with sed -i, python3 -c, or the editor's own script mode (vim -es)."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "the bash command to run" },
                    "timeout": { "type": "integer", "description": "seconds to let the command run before it is killed (default 120, at most 1800); a background run returns at once and takes none" },
                    "background": { "type": "boolean", "description": "start the command and return at once, its output going to the file the result names (default false)" }
                },
                "required": ["command"]
            })),
        },
    }
}

/// Run a shell command. Never returns Err: bad arguments, command failure and
/// timeouts all come back as tool result text.
pub async fn execute(args_json: &str) -> String {
    let args = match super::parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
        return "error: missing required argument command (string)".into();
    };
    let budget = match timeout(&args) {
        Ok(budget) => budget,
        Err(e) => return e,
    };
    let background = match args.get("background") {
        None => false,
        Some(given) => match given.as_bool() {
            Some(background) => background,
            None => return "error: background must be true or false".into(),
        },
    };
    // Refused before the command is run, the way a write refuses a path that is a
    // directory: what cannot work is answered with what to do instead rather than
    // with whatever it does to a screen it cannot have.
    if let Some(editor) = screen_editor(command) {
        return format!(
            "error: {editor} is a screen editor, and a call has no screen: it would draw its \
             whole interface into the result as escape codes and edit nothing. Edit a file with \
             Edit/Write, or with something that needs no screen -- sed -i, python3 -c, or the \
             editor's own script mode (vim -es)."
        );
    }

    if background {
        return start_in_background(command);
    }
    run(command, budget).await
}

/// How many jobs this process has started in the background, which is what numbers
/// the files their output goes to: two jobs writing one file would answer with each
/// other's output, and the calls that started them cannot see each other.
static BACKGROUND_JOBS: AtomicU64 = AtomicU64::new(0);

/// Start the command and return, leaving it running.
///
/// A call that waits is a call the turn waits for, and a build that takes ten
/// minutes is ten minutes of a conversation that cannot be answered. What comes
/// back instead is what a call needs to follow it: the pid to kill, and the file
/// its output goes to. Neither stream is a pipe, so nothing this process does can
/// block the job, and the job itself is in a session of its own -- it outlives the
/// turn, and the session, until somebody or something ends it.
fn start_in_background(command: &str) -> String {
    let number = BACKGROUND_JOBS.fetch_add(1, Ordering::Relaxed) + 1;
    let path = background_log(number);
    let dir = path.parent().expect("a log has a directory");
    if let Err(e) = std::fs::create_dir_all(dir) {
        return format!("error: cannot make {}: {e}", dir.display());
    }
    let file = match std::fs::File::create(&path) {
        Ok(file) => file,
        Err(e) => return format!("error: cannot write {}: {e}", path.display()),
    };
    let errors = match file.try_clone() {
        Ok(errors) => errors,
        Err(e) => return format!("error: cannot write {}: {e}", path.display()),
    };

    // No `kill_on_drop` here: the handle is dropped as soon as the job is named, and
    // the job is not this call's to end -- the runtime reaps it when it finishes.
    match bash(command).stdout(file).stderr(errors).spawn() {
        Err(e) => format!("exit_code: 127\n--- stderr ---\nfailed to start bash: {e}"),
        Ok(child) => {
            let pid = child.id().unwrap_or_default();
            drop(child);
            format!(
                "exit_code: 0\nbackground: started as pid {pid}, its output going to {}; the call \
                 returned as soon as it was started. Read that file to follow it. Nothing else \
                 ends it -- not this session, and not a timeout -- so `kill -9 -{pid}` kills the \
                 whole job when it is no longer wanted.",
                path.display()
            )
        }
    }
}

/// Where a background job's output goes: one file per call, in a directory of this
/// process's own under the temporary directory.
fn background_log(number: u64) -> PathBuf {
    Path::new(&std::env::temp_dir())
        .join(format!("caocli-{}", std::process::id()))
        .join(format!("bash-{number}.log"))
}

/// The program this call runs that cannot run here, if there is one.
///
/// The command is read the way a shell splits it, quotes and all: a separator
/// inside quotes is text a command is being given rather than the end of one, so
/// `echo "a; vim b"` is not a call to an editor. A wrapper is not seen through --
/// `sudo vim`, or a program that opens an editor of its own (`git commit` with no
/// message): those are left to fail by themselves, which they now do in a tenth of
/// a second, and the editor variables the child is given turn the second kind into
/// an error of its own instead of a screen.
fn screen_editor(command: &str) -> Option<&'static str> {
    for simple in simple_commands(command) {
        let mut words = simple.split_whitespace();
        // An assignment in front of the program is part of the setting of the
        // command rather than the command: `FOO=1 vim f` is a call to vim.
        let mut wrapped = false;
        let program = loop {
            let Some(word) = words.next() else { break None };
            if is_assignment(word) || (wrapped && word.starts_with('-')) {
                continue;
            }
            if WRAPPERS.contains(&word) {
                wrapped = true;
                continue;
            }
            break Some(word);
        };
        let Some(program) = program else { continue };
        let name = program.rsplit('/').next().unwrap_or(program);
        let script_mode = words.any(|word| SCRIPT_FLAGS.contains(&word));
        if script_mode {
            continue;
        }
        if let Some(editor) = SCREEN_EDITORS.iter().find(|e| **e == name) {
            return Some(editor);
        }
    }
    None
}

/// The simple commands in `command`, split where a shell would run one instead of
/// another, and nowhere else: a separator inside quotes is text, and a run of them
/// (`&&`, `||`, `;;`) is one break rather than several.
fn simple_commands(command: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    // Whether the character just read was part of a break.
    let mut broken = false;
    for (i, c) in command.char_indices() {
        if escaped {
            escaped = false;
        } else {
            match quote {
                Some(q) if c == q => quote = None,
                Some('"') if c == '\\' => escaped = true,
                Some(_) => {}
                None if c == '\'' || c == '"' => quote = Some(c),
                None if c == '\\' => escaped = true,
                None if is_break(c) => {
                    if !broken {
                        out.push(&command[start..i]);
                    }
                    start = i + c.len_utf8();
                    broken = true;
                    continue;
                }
                None => {}
            }
        }
        // Anything that is not a break ends one.
        broken = false;
    }
    out.push(&command[start..]);
    out
}

/// Whether a character is a place a shell would end one simple command and begin the
/// next.
fn is_break(c: char) -> bool {
    matches!(c, ';' | '|' | '&' | '\n' | '(' | ')')
}

/// Whether a word sets a variable for the command that follows it.
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// How long the call asked to be given, and [`TIMEOUT_SECS`] when it said nothing.
///
/// A length of its own is what a call needs for a build or a test suite that is
/// slow rather than stuck, and the cap is what keeps one call from being a turn
/// nobody can answer: a job longer than that is a job for the background.
fn timeout(args: &serde_json::Value) -> Result<Duration, String> {
    let Some(given) = args.get("timeout") else {
        return Ok(Duration::from_secs(TIMEOUT_SECS));
    };
    let Some(seconds) = given.as_u64() else {
        return Err("error: timeout must be a whole number of seconds".into());
    };
    if seconds == 0 {
        return Err("error: timeout must be at least 1 second".into());
    }
    if seconds > MAX_TIMEOUT_SECS {
        return Err(format!(
            "error: timeout is at most {MAX_TIMEOUT_SECS} seconds; a job longer than that belongs \
             in the background, where its output can be polled from a file"
        ));
    }
    Ok(Duration::from_secs(seconds))
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
             had started; what it printed up to then is below. A call may ask for up to \
             {MAX_TIMEOUT_SECS}s with the timeout argument, and a job that wants longer belongs in \
             the background, where its output can be polled from a file.\n",
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

/// `bash -c command`, with nothing of ours on its standard input, in a session of
/// its own, and with the editor variables pointed somewhere harmless.
///
/// The session is what keeps the command away from the user's terminal. A program
/// that wants to ask a person something opens `/dev/tty` rather than reading its
/// standard input, and it would find the very terminal the front end is reading:
/// the front end owns the screen and the keys, so a question typed into it by a
/// command is a question the front end answers, or a keystroke that never reaches
/// the line being typed. Measured: with the terminal reachable, `sudo true` waits
/// minutes for a password; without one it fails in a tenth of a second, which is a
/// result the model can act on. The session is also what makes the pid a process
/// group's id, which is what a timeout, a cancel and a background job are killed
/// by.
///
/// The editor variables are the one thing that cannot be refused by name: an editor
/// opened by a program of its own (`git commit` with no message) is named by a
/// variable rather than by the command. Both therefore point at `true`, which
/// changes nothing and says so -- measured: without them that call draws a screen
/// into the result before giving its own error, and with them it gives only the
/// error. A call that sets either one itself still overrides this.
fn bash(command: &str) -> Command {
    let mut spawn = Command::new("bash");
    spawn
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .env("GIT_EDITOR", "true")
        .env("EDITOR", "true");
    #[cfg(unix)]
    // SAFETY: `setsid` takes no arguments and touches nothing but the child's own
    // process attributes; it is called after the fork, before the exec.
    unsafe {
        spawn.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    spawn
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
    /// Spawn the command with its output piped, so that this call can read it as it
    /// arrives.
    fn spawn(command: &str) -> std::io::Result<Self> {
        let child = bash(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
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

    /// A call that asks for a length of its own is given it, and one that asks for
    /// nothing is given the default.
    #[test]
    fn a_call_may_ask_for_its_own_timeout() {
        assert_eq!(
            timeout(&json!({})).unwrap(),
            Duration::from_secs(TIMEOUT_SECS)
        );
        assert_eq!(
            timeout(&json!({"timeout": 900})).unwrap(),
            Duration::from_secs(900)
        );
        assert_eq!(
            timeout(&json!({"timeout": MAX_TIMEOUT_SECS})).unwrap(),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
    }

    /// What a call cannot have is said rather than rounded to something it did not
    /// ask for: a timeout of zero would kill the command before it started, and one
    /// past the cap would be a turn nobody can answer.
    #[test]
    fn an_impossible_timeout_is_answered_with_what_is_possible() {
        for (given, expected) in [
            (json!({"timeout": 0}), "at least 1 second"),
            (
                json!({"timeout": MAX_TIMEOUT_SECS + 1}),
                "at most 1800 seconds",
            ),
            (json!({"timeout": "900"}), "a whole number of seconds"),
            (json!({"timeout": 1.5}), "a whole number of seconds"),
            (json!({"timeout": -1}), "a whole number of seconds"),
        ] {
            let e = timeout(&given).expect_err("refused");
            assert!(e.starts_with("error: "), "{e}");
            assert!(e.contains(expected), "{e}");
        }
    }

    /// The length a call asked for is the one it is told about: the note names the
    /// budget rather than the default.
    #[tokio::test]
    async fn a_timeout_note_names_the_budget_that_ran_out() {
        let out = execute(r#"{"command":"sleep 30","timeout":1}"#).await;
        assert!(out.contains("exit_code: 124"), "{out}");
        assert!(out.contains("longer than 1s"), "{out}");
        assert!(out.contains("with the timeout argument"), "{out}");
    }

    /// A screen editor in the place a shell would run one is found, whatever else
    /// the command does around it. Read rather than run, on purpose: these commands
    /// are the ones that hang, so a regression here must fail a test rather than
    /// hang the suite.
    #[test]
    fn a_screen_editor_in_command_position_is_found() {
        for command in [
            "vim notes.txt",
            "cd /tmp && nano notes.txt",
            "cat notes.txt | vi",
            "/usr/bin/vim notes.txt",
            "FOO=1 vim notes.txt",
            "printf x > f\nemacs -nw f",
            "grep -n x f; vim f",
            "vi",
            "nice vim f",
            "sudo vim /etc/hostname",
            "env FOO=1 -i vim f",
            "xargs -0 vim",
        ] {
            assert!(screen_editor(command).is_some(), "{command}");
        }
    }

    /// What a refused call is answered with says what to do instead. The editor
    /// named here is one this machine does not have, so the assertion is about the
    /// refusal reaching the model and cannot be satisfied by running the editor.
    #[tokio::test]
    async fn a_refusal_says_what_to_do_instead() {
        let out = execute(r#"{"command":"nvim notes.txt"}"#).await;
        assert!(refused(&out), "{out}");
        assert!(out.contains("Edit/Write"), "{out}");
        assert!(out.contains("vim -es"), "{out}");
    }

    /// The command is a command, not the text it contains: a name is only worth
    /// refusing where a shell would have run it.
    #[test]
    fn naming_an_editor_is_not_running_one() {
        for command in [
            "echo vim",
            "echo 'a; vim b'",
            "grep -rn vim src | head -3",
            "git log --oneline --grep=vim",
            "test -f vim || echo 'no vim here'",
            r#"echo "an editor: vim""#,
            "sed -n 1p vim",
            "cp vim vim.bak",
            "git commit -m 'a message about vim'",
        ] {
            assert_eq!(screen_editor(command), None, "{command}");
        }
    }

    /// A wrapper's own argument hides what follows it. That is a refusal missed
    /// rather than one made wrongly, which is the way round a list of names has to
    /// err: a missed one costs a call that hangs, a wrong one takes a command away
    /// from the model.
    #[test]
    fn a_wrappers_own_argument_is_not_read_as_the_program() {
        assert_eq!(screen_editor("sudo -u root cat README.md"), None);
        assert_eq!(screen_editor("nice -n 5 vim f"), None);
    }

    /// The editor an editor variable names cannot be refused by name, so the
    /// variables are pointed at something that changes nothing: a program that opens
    /// one fails with its own error instead of drawing a screen first.
    #[tokio::test]
    async fn a_program_that_opens_an_editor_fails_by_itself() {
        // `/` is not a repository: what git says here is that there is nothing to
        // commit from, which is its own error rather than a screen.
        let out =
            execute(r#"{"command":"echo \"$GIT_EDITOR $EDITOR\"; cd / && git commit"}"#).await;
        assert!(out.contains("true true"), "{out}");
        assert!(!out.contains("\u{1b}["), "{out}");
    }

    /// The editor's own script mode is the exception the list is written with, and
    /// it is a real way to edit a file from here.
    #[tokio::test]
    async fn an_editors_script_mode_may_edit_a_file() {
        let path = std::env::temp_dir().join(format!("caocli-vim-{}", std::process::id()));
        std::fs::write(&path, "before\n").unwrap();
        let command = format!(
            "vim -es -c '%s/before/after/' -c wq {}",
            path.to_string_lossy()
        );
        let out = execute(&json!({ "command": command }).to_string()).await;
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text, "after\n", "{out}");
    }

    #[test]
    fn simple_commands_split_where_a_shell_would_run_one() {
        assert_eq!(simple_commands("a; b && c | d"), ["a", " b ", " c ", " d"]);
        assert_eq!(simple_commands("echo 'a; b'"), ["echo 'a; b'"]);
        assert_eq!(simple_commands(r#"echo "a | b""#), [r#"echo "a | b""#]);
        assert_eq!(simple_commands(r#"echo a\;\ b"#), [r#"echo a\;\ b"#]);
        assert_eq!(simple_commands("(cd x && ls)"), ["", "cd x ", " ls", ""]);
        assert_eq!(simple_commands("ls;"), ["ls", ""]);
    }

    /// Whether a result is the refusal rather than whatever the command did.
    fn refused(out: &str) -> bool {
        out.starts_with("error: ") && out.contains("a call has no screen")
    }

    /// A background call returns as soon as the job is started, and what it returns
    /// is what a call needs to follow and to end it: the pid and the file.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_background_call_returns_at_once_with_where_its_output_went() {
        let started = Instant::now();
        let out = execute(r#"{"command":"echo started; sleep 30","background":true}"#).await;
        assert!(out.contains("exit_code: 0"), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        let pid: i32 = field(&out, "pid ").parse().expect("a pid");
        let path = field(&out, "going to ");
        // The job writes its output where the result says it does, and it is still
        // running when the call has long since returned.
        let mut log = String::new();
        for _ in 0..40 {
            log = std::fs::read_to_string(path).unwrap_or_default();
            if log.contains("started") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(log.contains("started"), "{log:?}");
        let alive = still_running(pid).await;
        kill_group(Some(pid as u32));
        assert!(alive, "the job outlived the call");
        assert!(!still_running(pid).await, "the job did not die");
    }

    /// Each job gets a file of its own: two calls writing one file would each answer
    /// with the other's output.
    #[tokio::test]
    async fn every_background_job_gets_a_file_of_its_own() {
        let first = execute(r#"{"command":"true","background":true}"#).await;
        let second = execute(r#"{"command":"true","background":true}"#).await;
        assert_ne!(
            field(&first, "going to "),
            field(&second, "going to "),
            "{first}\n{second}"
        );
    }

    /// What cannot be read as a background is said rather than guessed at.
    #[tokio::test]
    async fn background_has_to_be_a_boolean() {
        let out = execute(r#"{"command":"true","background":"yes"}"#).await;
        assert_eq!(out, "error: background must be true or false");
    }

    /// The word that follows `label` in a result, without the punctuation that ends
    /// it.
    fn field<'a>(out: &'a str, label: &str) -> &'a str {
        out.split_once(label)
            .expect("the result names it")
            .1
            .split_whitespace()
            .next()
            .expect("a value")
            .trim_end_matches([',', ';'])
    }

    #[test]
    fn assignments_are_not_the_program() {
        assert!(is_assignment("FOO=1"));
        assert!(is_assignment("_x="));
        assert!(is_assignment("echo=1=2"));
        assert!(!is_assignment("1FOO=1"));
        assert!(!is_assignment("FOO"));
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
