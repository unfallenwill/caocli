//! One submitted line, handled the same way by every front end.
//!
//! The commands and the session handling do not depend on how the line was
//! typed, so they live here rather than inside either front end.

use std::path::Path;

use anyhow::Result;

use crate::agent::{Agent, Approve, Interrupt};
use crate::session::{self, Session};
use crate::ui::Front;

/// What a submitted line asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Keep the session going.
    Continue,
    /// The user asked to quit.
    Exit,
}

/// Handle one submitted line: a command, or a conversational turn.
///
/// `interrupt` and `approve` only reach the model when the line is a turn; they
/// are passed through so that a front end choosing when to run the turn also
/// chooses where its cancel and approval answers come from.
pub async fn handle(
    agent: &mut Agent,
    ui: &mut dyn Front,
    sdir: &Path,
    line: &str,
    interrupt: &mut dyn Interrupt,
    approve: &mut dyn Approve,
) -> Result<Outcome> {
    match line {
        "/exit" | "/quit" | "/q" => return Ok(Outcome::Exit),
        "/help" => ui.info(&help()),
        "/sessions" => {
            for s in session::list(sdir)? {
                ui.info(&format!(
                    "{}\t{} messages\t{}",
                    s.id, s.message_count, s.preview
                ));
            }
        }
        "/new" => match Session::create(sdir, agent.session.meta.clone()) {
            Ok(s) => {
                ui.info(&format!("new session {}", s.id));
                agent.adopt(s);
                ui.reset_stats();
                ui.set_model(&agent.session.meta.model);
            }
            Err(e) => ui.error(&format!("{e:#}")),
        },
        _ if line.starts_with("/resume ") => {
            let id = line.trim_start_matches("/resume ").trim();
            match Session::load(&sdir.join(format!("{id}.jsonl"))) {
                Ok(s) => {
                    ui.info(&format!(
                        "switched to session {} ({} messages)",
                        s.id,
                        s.messages.len()
                    ));
                    ui.replay(&s.messages);
                    agent.adopt(s);
                    ui.reset_stats();
                    ui.set_model(&agent.session.meta.model);
                }
                Err(e) => ui.error(&format!("{e:#}")),
            }
        }
        _ if line.starts_with('/') => {
            ui.info("unknown command; /help lists the available commands")
        }
        _ => {
            if let Err(e) = agent.turn(line, ui, interrupt, approve).await {
                ui.error(&format!("{e:#}"));
            }
        }
    }
    Ok(Outcome::Continue)
}

/// One slash command, as the picker and the help text both need it.
pub struct Command {
    pub name: &'static str,
    /// One line, shown beside the name by the picker.
    pub description: &'static str,
}

/// The commands, in the order the picker lists them.
///
/// This is the single source: the help text and the completion list are both
/// built from it, so a command cannot be dispatched but undocumented, or
/// documented but not dispatched.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "/help",
        description: "show this",
    },
    Command {
        name: "/new",
        description: "start a new session",
    },
    Command {
        name: "/sessions",
        description: "list sessions",
    },
    Command {
        name: "/resume",
        description: "switch to a session by its id",
    },
    Command {
        name: "/exit",
        description: "quit",
    },
    Command {
        name: "/quit",
        description: "quit",
    },
    Command {
        name: "/q",
        description: "quit",
    },
];

/// The commands whose name matches what has been typed so far.
///
/// Empty unless the line is a command still being named: once there is a space
/// the rest is an argument, and an empty line is not a command at all. The
/// caller gets the same list whether it filters one command or all of them.
pub fn completions(input: &str) -> Vec<&'static Command> {
    if !input.starts_with('/') || input.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(input))
        .collect()
}

/// The keys the prompt accepts, and the startup flags. Separate from the table
/// above because these are not commands.
const INPUT_AND_FLAGS: &str = "Input:\n  Enter            submit\n  Ctrl-J           newline (multi-line input)\n  Tab              complete a command\n  Up / Down        pick a command, or browse history\nStartup flags:\n  -c / --continue  continue the most recent session\n  --resume <id>    resume a specific session\n  --provider deepseek|glm\n  --effort low|high|max --model <id>\n  -p \"prompt\"      run once and exit";

/// The `/help` text, built from the command table so the two cannot drift.
pub fn help() -> String {
    let mut out = String::from("Commands:");
    for c in COMMANDS {
        out.push_str(&format!("\n  {:<15} {}", c.name, c.description));
    }
    out.push('\n');
    out.push_str(INPUT_AND_FLAGS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use crate::session::SessionMeta;
    use crate::types::{Message, ToolCall, Usage};
    use crate::ui::Renderer;

    /// A front end that records what it was told, so the line handling can be
    /// tested without a terminal.
    #[derive(Default)]
    struct Recording {
        info: Vec<String>,
        errors: Vec<String>,
        replayed: usize,
        model: Option<String>,
        resets: usize,
    }

    impl crate::ui::Ui for Recording {
        fn reasoning_delta(&mut self, _s: &str) {}
        fn content_delta(&mut self, _s: &str) {}
        fn finish_turn(&mut self) {}
        fn tool_start(&mut self, _name: &str, _args: &str) {}
        fn tool_result(&mut self, _result: &str) {}
        fn usage(&mut self, _u: &Usage) {}
        fn interrupted(&mut self) {}
        fn approval_requested(&mut self, _name: &str, _args: &str) {}
    }

    impl Front for Recording {
        fn replay(&mut self, messages: &[Message]) {
            self.replayed = messages.len();
        }
        fn info(&mut self, s: &str) {
            self.info.push(s.to_owned());
        }
        fn error(&mut self, s: &str) {
            self.errors.push(s.to_owned());
        }
        fn set_model(&mut self, model: &str) {
            self.model = Some(model.to_owned());
        }
        fn reset_stats(&mut self) {
            self.resets += 1;
        }
    }

    /// Never cancels: no command path below needs a cancel.
    struct Silent;
    impl Interrupt for Silent {
        fn wait(&mut self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> {
            Box::pin(std::future::pending())
        }
    }

    /// Never answers: only reached if a tool call gets that far.
    struct Deny;
    impl Approve for Deny {
        fn ask(
            &mut self,
            _call: &ToolCall,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + '_>> {
            Box::pin(async { false })
        }
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-repl-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// An agent whose backend is never reached: every command path below either
    /// sends nothing, or expects the failure.
    fn agent_in(dir: &std::path::Path, model: &str) -> Agent {
        let api = Client::new("test-key".into(), "http://127.0.0.1:1/never".into()).unwrap();
        let session = Session::create(
            dir,
            SessionMeta {
                model: model.to_owned(),
                reasoning_effort: None,
            },
        )
        .unwrap();
        Agent::new(api, session)
    }

    async fn submit(
        agent: &mut Agent,
        ui: &mut Recording,
        sdir: &std::path::Path,
        line: &str,
    ) -> Outcome {
        handle(agent, ui, sdir, line, &mut Silent, &mut Deny)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn every_exit_spelling_leaves() {
        let dir = tmpdir("exit");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        for spelling in ["/exit", "/quit", "/q"] {
            assert_eq!(
                submit(&mut agent, &mut ui, &dir, spelling).await,
                Outcome::Exit,
                "{spelling} should leave"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn help_is_a_notice_and_does_not_leave() {
        let dir = tmpdir("help");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        assert_eq!(
            submit(&mut agent, &mut ui, &dir, "/help").await,
            Outcome::Continue
        );
        assert_eq!(ui.info, vec![help()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_unknown_command_says_so_instead_of_becoming_a_turn() {
        let dir = tmpdir("unknown");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        assert_eq!(
            submit(&mut agent, &mut ui, &dir, "/bogus").await,
            Outcome::Continue
        );
        assert!(ui.info[0].contains("unknown command"), "{:?}", ui.info);
        assert!(agent.session.messages.is_empty(), "not recorded as a turn");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn sessions_lists_what_the_directory_holds() {
        let dir = tmpdir("sessions");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/sessions").await;
        assert_eq!(ui.info.len(), 1, "{:?}", ui.info);
        assert!(ui.info[0].contains(&agent.session.id), "{:?}", ui.info);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn new_switches_session_and_keeps_the_meta() {
        let dir = tmpdir("new");
        let mut agent = agent_in(&dir, "first-model");
        let mut ui = Recording::default();
        let before = agent.session.id.clone();
        submit(&mut agent, &mut ui, &dir, "/new").await;
        assert_ne!(agent.session.id, before, "a new session was created");
        assert_eq!(ui.resets, 1, "the cache statistics are cleared");
        assert_eq!(
            ui.model.as_deref(),
            Some("first-model"),
            "meta carried over"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn resume_switches_session_and_replays_its_history() {
        let dir = tmpdir("resume");
        let mut agent = agent_in(&dir, "m");
        let mut other = Session::create(
            &dir,
            SessionMeta {
                model: "second-model".to_owned(),
                reasoning_effort: None,
            },
        )
        .unwrap();
        other
            .append_message(&Message::user("earlier question"))
            .unwrap();
        let id = other.id.clone();
        // A session file has a single writer, and `other` still holds it: drop
        // the handle or the resume below is refused.
        drop(other);
        // The file looks locked for a moment after the handle goes: a `fork` in
        // another test copies the file description the lock lives on. See
        // `session::wait_until_released`.
        session::wait_until_released(&dir.join(format!("{id}.jsonl")));

        let mut ui = Recording::default();
        assert_eq!(
            submit(&mut agent, &mut ui, &dir, &format!("/resume {id}")).await,
            Outcome::Continue
        );
        assert_eq!(agent.session.id, id);
        assert!(
            ui.replayed >= 1,
            "the resumed history was replayed: {}",
            ui.replayed
        );
        assert_eq!(ui.model.as_deref(), Some("second-model"));
        assert_eq!(ui.resets, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn resuming_something_that_is_not_there_reports_instead_of_switching() {
        let dir = tmpdir("resume-missing");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        let before = agent.session.id.clone();
        submit(&mut agent, &mut ui, &dir, "/resume 19700101-000000").await;
        assert_eq!(agent.session.id, before, "the session did not change");
        assert_eq!(ui.errors.len(), 1, "{:?}", ui.errors);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn anything_else_is_a_turn_whose_failure_is_reported_not_fatal() {
        let dir = tmpdir("turn");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        assert_eq!(
            submit(&mut agent, &mut ui, &dir, "hello there").await,
            Outcome::Continue
        );
        assert_eq!(ui.errors.len(), 1, "{:?}", ui.errors);
        assert!(
            agent
                .session
                .messages
                .iter()
                .any(|m| m.content.as_deref() == Some("hello there")),
            "the user line was persisted before the attempt"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn help_lists_every_command() {
        // Structural now: the help text is built from the table, so this is the
        // one place that could still drift -- a command in the table that the
        // help text dropped.
        let text = help();
        for command in COMMANDS {
            assert!(
                text.contains(command.name),
                "{} is undocumented",
                command.name
            );
            assert!(
                text.contains(command.description),
                "{} has no description",
                command.name
            );
        }
    }

    #[test]
    fn help_lists_the_startup_flags_and_input_keys() {
        for expected in [
            "-c / --continue",
            "--provider deepseek|glm",
            "--effort low|high|max",
            "-p \"prompt\"",
            "Ctrl-J",
            "Tab",
        ] {
            assert!(help().contains(expected), "missing {expected:?}");
        }
    }

    #[test]
    fn help_is_a_single_notice() {
        // It reaches the screen through info(), which writes one cell: a trailing
        // newline would show up as a blank row.
        assert!(!help().ends_with('\n'));
        assert!(help().contains('\n'), "the commands are one per line");
    }

    #[test]
    fn an_empty_line_offers_no_commands() {
        assert!(completions("").is_empty());
        assert!(completions("hello").is_empty());
    }

    #[test]
    fn a_command_prefix_narrows_to_matching_names() {
        assert_eq!(names(completions("/res")), vec!["/resume"]);
        assert_eq!(names(completions("/s")), vec!["/sessions"]);
        // a bare slash offers everything, in table order
        assert_eq!(completions("/").len(), COMMANDS.len());
    }

    #[test]
    fn a_line_with_an_argument_is_no_longer_a_name() {
        // "/resume 2026" is the command plus its argument: completing it again
        // would fight what the user is typing.
        assert!(completions("/resume 2026").is_empty());
        assert!(completions("/ ").is_empty());
    }

    #[test]
    fn a_command_that_matches_nothing_offers_nothing() {
        assert!(completions("/nope").is_empty());
    }

    fn names(commands: Vec<&'static Command>) -> Vec<&'static str> {
        commands.into_iter().map(|c| c.name).collect()
    }

    #[test]
    fn the_plain_renderer_is_a_front_end_too() {
        // Compile-time guard: the handler is written against `Front`, and the
        // plain renderer is the implementation that must keep satisfying it.
        fn assert_front<T: Front>() {}
        assert_front::<Renderer>();
        assert_front::<Recording>();
    }
}
