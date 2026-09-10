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
        "/help" => ui.info(HELP),
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

/// The `/help` text. It is a notice like any other, so it reaches the screen
/// through the front end rather than straight to stdout -- which the interactive
/// front end owns.
pub const HELP: &str = "Commands:\n  /exit /quit /q   quit\n  /new             start a new session\n  /sessions        list sessions\n  /resume <id>     switch to a specific session\nInput:\n  Enter            submit\n  Ctrl-J           newline (multi-line input)\nStartup flags:\n  -c / --continue  continue the most recent session\n  --resume <id>    resume a specific session\n  --provider deepseek|glm\n  --effort low|high|max --model <id>\n  -p \"prompt\"      run once and exit";

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
        assert_eq!(ui.info, vec![HELP.to_string()]);
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
    fn help_lists_every_command_handle_dispatches() {
        // A command that is dispatched but undocumented, or documented but not
        // dispatched, is a defect in either direction.
        for documented in ["/exit", "/quit", "/q", "/new", "/sessions", "/resume"] {
            assert!(HELP.contains(documented), "{documented} is undocumented");
        }
    }

    #[test]
    fn help_lists_the_startup_flags_and_input_keys() {
        for expected in [
            "/resume <id>",
            "-c / --continue",
            "--provider deepseek|glm",
            "--effort low|high|max",
            "-p \"prompt\"",
            "Ctrl-J",
        ] {
            assert!(HELP.contains(expected), "missing {expected:?}");
        }
    }

    #[test]
    fn help_is_a_single_notice() {
        // It reaches the screen through info(), which writes one cell: a trailing
        // newline would show up as a blank row.
        assert!(!HELP.ends_with('\n'));
        assert!(HELP.contains('\n'), "the commands are one per line");
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
