//! One submitted line, handled the same way by every front end.
//!
//! The commands and the session handling do not depend on how the line was
//! typed, so they live here rather than inside either front end.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::agent::Agent;
use crate::agents_md;
use crate::api::Client;
use crate::config;
use crate::image;
use crate::provider;
use crate::session::{self, Session};
use crate::ui::Front;
use crate::ui::text::{padded, width};
use crate::ui::{Approve, Ask, Cancel};

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
/// The three answer sources only reach the model when the line is a turn; they
/// are passed through so that a front end choosing when to run the turn also
/// chooses where its cancel, approval and question answers come from.
pub async fn handle(
    agent: &mut Agent,
    ui: &mut dyn Front,
    sdir: &Path,
    line: &str,
    cancel: &mut dyn Cancel,
    approve: &mut dyn Approve,
    ask: &mut dyn Ask,
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
        "/new" => {
            // A new session reads the workspace as it stands now: instructions
            // edited since this one began are picked up here, the same way
            // they were picked up when it began. What the new session freezes
            // in is its own, and the old session's log is untouched.
            let mut meta = agent.session.meta.clone();
            meta.instructions = agents_md::load();
            match Session::create(sdir, meta) {
                Ok(s) => {
                    ui.info(&format!(
                        "new session {}{}",
                        s.id,
                        if s.meta.instructions.is_some() {
                            " · AGENTS.md"
                        } else {
                            ""
                        }
                    ));
                    agent.adopt(s);
                    ui.reset_stats();
                    ui.set_model(&agent.model_label());
                }
                Err(e) => ui.error(&format!("{e:#}")),
            }
        }
        // The interactive front end offers the list to choose from instead; this
        // is what both front ends say when there is nothing to offer.
        "/resume" => ui.info("resume needs a session id; /sessions lists them"),
        // Same bargain as `/resume`: the picker is the front end that can offer
        // the list, and this is the menu for the one that can only print.
        "/login" => ui.info(&login_menu()),
        "/model" => ui.info(&model_menu(&agent.model_label())),
        "/effort" => ui.info(&effort_menu(&agent.provider(), agent.effort_label())),
        _ if line.starts_with("/login ") => {
            login(agent, ui, argument(line)).await;
        }
        _ if line.starts_with("/model ") => {
            choose_model(agent, ui, argument(line));
        }
        _ if line.starts_with("/effort ") => {
            choose_effort(agent, ui, argument(line));
        }
        _ if line == "/image" || line.starts_with("/image ") => {
            send_image(agent, ui, argument(line), cancel, approve, ask).await;
        }
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
                    // Where the session runs is the session's to say. A key that
                    // is missing is reported after it is adopted: the transcript
                    // is worth reading either way, and `/login` fixes the rest.
                    if let Err(e) = bind_to_session(agent) {
                        ui.error(&format!("{e:#}"));
                    }
                    ui.reset_stats();
                    ui.set_model(&agent.model_label());
                    ui.set_effort(agent.effort_label());
                }
                Err(e) => ui.error(&format!("{e:#}")),
            }
        }
        _ if line.starts_with('/') => {
            ui.info("unknown command; /help lists the available commands")
        }
        _ => {
            if let Err(e) = agent.turn(line, ui, cancel, approve, ask).await {
                ui.error(&format!("{e:#}"));
            }
        }
    }
    Ok(Outcome::Continue)
}

/// Point the machine at the provider a session names: a resumed session runs
/// where it ran, not where the session before it happened to be. A session that
/// names none — written before providers were recorded — is left as it is.
fn bind_to_session(agent: &mut Agent) -> Result<()> {
    let Some(id) = agent.session.meta.provider.clone() else {
        return Ok(());
    };
    if id == agent.provider().id {
        return Ok(());
    }
    let provider = provider::provider(&id)?;
    let api = Client::for_provider(&provider, config::api_key(&provider)?)?;
    agent.bind(provider, api);
    Ok(())
}

/// The argument of a submitted command line, trimmed; empty when there is none.
fn argument(line: &str) -> &str {
    line.split_once(char::is_whitespace)
        .map(|(_, arg)| arg.trim())
        .unwrap_or("")
}

/// `/login <provider>`: store that provider's API key where it outlives the
/// session — `settings.json` under the caocli directory — and use it right away
/// if it is the provider this session is talking to.
///
/// The key is asked for as a secret: it is not echoed, not put in the
/// transcript, not written to the session log, and not remembered as history.
async fn login(agent: &mut Agent, ui: &mut dyn Front, provider_id: &str) {
    let provider = match provider::provider(provider_id) {
        Ok(provider) => provider,
        Err(e) => return ui.error(&format!("{e:#}")),
    };
    let prompt = format!(
        "{} API key (input hidden · Enter saves · empty cancels)",
        provider.name
    );
    let Some(key) = ui.ask_secret(&prompt).await else {
        return ui.info("nothing was stored");
    };
    let key = key.trim();
    if key.is_empty() {
        return ui.info("nothing was stored");
    }
    let path = match config::store_key(provider.id, key) {
        Ok(path) => path,
        Err(e) => return ui.error(&format!("{e:#}")),
    };
    // A key that is only read at startup would leave the next turn failing with
    // no way to see why, so a login for the provider in use is taken up now.
    if agent.provider().id == provider.id {
        match Client::for_provider(&provider, key.to_owned()) {
            Ok(api) => agent.bind(provider, api),
            Err(e) => return ui.error(&format!("{e:#}")),
        }
    }
    ui.info(&format!(
        "stored the {} API key in {}",
        provider.name,
        path.display()
    ));
}

/// `/model <provider>/<modelid>`: switch the session's model, and the provider
/// it is served by when the model names one. A bare model id stays with the
/// provider the session already runs on.
fn choose_model(agent: &mut Agent, ui: &mut dyn Front, spec: &str) {
    let (provider, model) = match provider::model_spec(spec, agent.provider().id) {
        Ok(choice) => choice,
        Err(e) => return ui.error(&format!("{e:#}")),
    };
    // A model is only usable where there is a key to send it with: storing the
    // choice and failing on the next turn would look like the backend's fault.
    let api = match config::api_key(&provider).and_then(|key| Client::for_provider(&provider, key))
    {
        Ok(api) => api,
        Err(e) => return ui.error(&format!("{e:#}")),
    };
    let mut meta = agent.session.meta.clone();
    meta.provider = Some(provider.id.to_string());
    meta.model = model;
    if meta != agent.session.meta {
        // The log first: it is the state, and a model that is only in memory
        // would be gone at the next resume.
        if let Err(e) = agent.session.set_meta(meta) {
            return ui.error(&format!("{e:#}"));
        }
    }
    agent.bind(provider, api);
    ui.set_model(&agent.model_label());
    // A model can move the session to another provider, whose default tier is
    // sent when the session stored none; the status line says which that is.
    ui.set_effort(agent.effort_label());
    // The cache figures belong to the model that answered, not to the session.
    ui.reset_stats();
    ui.info(&format!("model {}", agent.model_label()));
}

/// `/effort <tier>`: switch the session's reasoning effort tier. The tier is the
/// provider's to offer, so it is checked against the one in use before it is
/// stored — a value out of range is one backend's 400 and another's silent
/// misreading, and neither is worth finding out about on the next turn.
fn choose_effort(agent: &mut Agent, ui: &mut dyn Front, tier: &str) {
    if let Err(e) = agent.provider().validate_effort(tier) {
        return ui.error(&format!("{e:#}"));
    }
    let mut meta = agent.session.meta.clone();
    meta.reasoning_effort = Some(tier.to_owned());
    if meta != agent.session.meta {
        // The log first: it is the state, and a tier that is only in memory
        // would be gone at the next resume.
        if let Err(e) = agent.session.set_meta(meta) {
            return ui.error(&format!("{e:#}"));
        }
    }
    ui.set_effort(agent.effort_label());
    ui.info(&format!("effort {tier}"));
}

/// `/image <path> [text]`: one turn about a picture.
///
/// The image is read now and carried in the message itself, so what the session
/// holds is what the backend was sent, byte for byte, and a resumed session sends
/// the same bytes again rather than going back to a file that may have changed or
/// gone. Which is also why the message is shown the way a resume shows it: the
/// line that was typed is a command, and what the session holds is this message.
async fn send_image(
    agent: &mut Agent,
    ui: &mut dyn Front,
    argument: &str,
    cancel: &mut dyn Cancel,
    approve: &mut dyn Approve,
    ask: &mut dyn Ask,
) {
    let (path, text) = match image_argument(argument) {
        Ok(parts) => parts,
        Err(usage) => return ui.info(&usage),
    };
    let message = match image::user_message(&text, std::slice::from_ref(&path)) {
        Ok(message) => message,
        Err(e) => return ui.error(&format!("{e:#}")),
    };
    ui.replay(std::slice::from_ref(&message));
    if let Err(e) = agent.turn_message(message, ui, cancel, approve, ask).await {
        ui.error(&format!("{e:#}"));
    }
}

/// The path and the text of an `/image` line.
///
/// The path is one token, and a path with spaces in it -- a screenshot's name
/// often has them -- may be quoted with `"` or `'`. Everything after the path is
/// what was asked about the picture, and is empty when the picture is all there
/// was.
fn image_argument(argument: &str) -> Result<(PathBuf, String), String> {
    let argument = argument.trim();
    if argument.is_empty() {
        return Err("usage: /image <path> [text]".into());
    }
    let (path, text) = match argument.strip_prefix(['"', '\'']) {
        Some(rest) => {
            let quote = &argument[..1];
            match rest.split_once(quote) {
                Some((path, text)) => (path, text),
                None => return Err(format!("unterminated quote in {argument:?}")),
            }
        }
        None => match argument.split_once(char::is_whitespace) {
            Some((path, text)) => (path, text),
            None => (argument, ""),
        },
    };
    Ok((PathBuf::from(path), text.trim().to_owned()))
}

/// The `/login` menu as text, for the front end that cannot offer a list to pick
/// from. Built from the preset table, so a provider cannot be logged into
/// without being listed here.
///
/// A person reads the name and types the id, so the plain prompt — which is the
/// one that has to be typed at — shows both, where the picker shows the name and
/// submits the id itself.
pub fn login_menu() -> String {
    let rows = config::provider_choices();
    // Both columns as wide as their widest entry: an id that runs into the
    // column beside it is read as part of it.
    let names = rows.iter().map(|r| width(&r.label)).max().unwrap_or(0);
    let ids = rows.iter().map(|r| width(&r.argument)).max().unwrap_or(0);
    let mut out = String::from("login stores an API key for a provider:");
    for row in &rows {
        out.push_str(&format!(
            "\n  {} {} {}",
            padded(&row.label, names),
            padded(&row.argument, ids),
            row.detail
        ));
    }
    out.push_str("\nrun /login <provider id>");
    out
}

/// The `/model` menu as text, in the same spirit: every model the preset table
/// knows, named the way the status line names the current one.
pub fn model_menu(current: &str) -> String {
    let rows = config::model_menu(current);
    let models = rows.iter().map(|r| width(&r.label)).max().unwrap_or(0);
    let mut out = String::from("choose a model:");
    for row in &rows {
        out.push_str(&format!(
            "\n  {} {}",
            padded(&row.label, models),
            row.detail
        ));
    }
    out.push_str("\nrun /model <provider id>/<modelid>");
    out
}

/// The `/effort` menu as text, in the same spirit: the tiers the provider in use
/// accepts, with the one in effect marked.
pub fn effort_menu(provider: &provider::Provider, current: &str) -> String {
    let rows = config::effort_menu(provider, current);
    let tiers = rows.iter().map(|r| width(&r.label)).max().unwrap_or(0);
    let mut out = String::from("choose a reasoning effort tier:");
    for row in &rows {
        out.push_str(&format!("\n  {} {}", padded(&row.label, tiers), row.detail));
    }
    out.push_str("\nrun /effort <tier>");
    out
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
        name: "/login",
        description: "store a provider's API key",
    },
    Command {
        name: "/model",
        description: "switch model: <provider>/<modelid>",
    },
    Command {
        name: "/effort",
        description: "switch the reasoning effort tier",
    },
    Command {
        name: "/image",
        description: "ask about an image: <path> [text]",
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
const INPUT_AND_FLAGS: &str = "Input:\n  Enter            submit\n  Ctrl-J           newline (multi-line input)\n  Tab              complete a command\n  Up / Down        pick a command, or browse history\nStartup flags:\n  -c / --continue  continue the most recent session\n  --resume <id>    resume a specific session\n  --provider deepseek|zai-coding-cn|minimax\n  --effort low|high|max --model <id>\n  -p \"prompt\"      run once and exit\n  --image <path>   attach an image to -p's prompt";

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
    use crate::types::{Message, Usage};
    use crate::ui::Renderer;
    use crate::ui::doubles::{Answer, NoCancel, NoQuestions};

    /// A front end that records what it was told, so the line handling can be
    /// tested without a terminal.
    #[derive(Default)]
    struct Recording {
        info: Vec<String>,
        errors: Vec<String>,
        replayed: Vec<Message>,
        model: Option<String>,
        effort: Option<String>,
        resets: usize,
        /// What `ask_secret` answers with; `None` is a cancellation, which is
        /// also what a front end that never answers gives.
        secret: Option<String>,
        /// What it was asked for, so a test can say what the question was --
        /// and that the answer to it never came back as a line or a notice.
        prompts: Vec<String>,
    }

    impl crate::ui::Ui for Recording {
        fn reasoning_delta(&mut self, _s: &str) {}
        fn content_delta(&mut self, _s: &str) {}
        fn finish_turn(&mut self) {}
        fn tool_start(&mut self, _name: &str, _args: &str) {}
        fn tool_output(&mut self, _chunk: &str) {}
        fn tool_result(&mut self, _result: &str) {}
        fn instructions(&mut self, _dir: &str) {}
        fn usage(&mut self, _u: &Usage, _stream: std::time::Duration) {}
        fn interrupted(&mut self) {}
        fn approval_requested(&mut self, _name: &str, _args: &str) {}
    }

    impl Front for Recording {
        fn replay(&mut self, messages: &[Message]) {
            self.replayed = messages.to_vec();
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
        fn set_effort(&mut self, effort: &str) {
            self.effort = Some(effort.to_owned());
        }
        fn reset_stats(&mut self) {
            self.resets += 1;
        }
        fn ask_secret(
            &mut self,
            prompt: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + '_>> {
            self.prompts.push(prompt.to_owned());
            let answer = self.secret.clone();
            Box::pin(async move { answer })
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
        agent_in_session_of(dir, "deepseek", model)
    }

    fn agent_in_session_of(dir: &std::path::Path, provider: &str, model: &str) -> Agent {
        let api = Client::new(
            "test-key".into(),
            "http://127.0.0.1:1/never".into(),
            crate::provider::DEEPSEEK.wire,
        )
        .unwrap();
        let session = Session::create(
            dir,
            SessionMeta {
                provider: Some(provider.to_owned()),
                model: model.to_owned(),
                reasoning_effort: None,
                instructions: None,
            },
        )
        .unwrap();
        Agent::new(api, session, crate::provider::provider(provider).unwrap())
    }

    async fn submit(
        agent: &mut Agent,
        ui: &mut Recording,
        sdir: &std::path::Path,
        line: &str,
    ) -> Outcome {
        handle(
            agent,
            ui,
            sdir,
            line,
            &mut NoCancel,
            &mut Answer::denies(),
            &mut NoQuestions,
        )
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
            Some("deepseek/first-model"),
            "meta carried over, named by its provider"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn new_reads_the_workspace_as_it_stands_now() {
        // A fresh session reads the workspace's instructions; `/new` is a fresh
        // session, so it reads them again rather than inheriting what the one
        // before it read — instructions edited since are what the new session
        // carries.
        let _lock = crate::config::env_lock();
        let back = std::env::current_dir().unwrap();
        let workspace = crate::config::scratch_cwd();
        std::fs::write(workspace.join("AGENTS.md"), "be brief").unwrap();
        let dir = tmpdir("new-instructions");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/new").await;
        let text = agent
            .session
            .meta
            .instructions
            .as_deref()
            .unwrap_or_default();
        assert!(
            text.contains("be brief"),
            "the new session carries what the workspace says now: {text}"
        );
        assert!(
            ui.info[0].ends_with(" · AGENTS.md"),
            "the notice says where the session's instructions came from: {:?}",
            ui.info
        );
        std::env::set_current_dir(back).unwrap();
        std::fs::remove_dir_all(&workspace).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn resume_switches_session_and_replays_its_history() {
        // The key resolution here has to be the test's own: a key for the other
        // provider, left in a HOME of its own by another test, would bind the
        // resumed session's provider, which is the opposite of what is asserted.
        let (guard, home) = own_home();
        let dir = tmpdir("resume");
        let mut agent = agent_in(&dir, "m");
        let mut other = Session::create(
            &dir,
            SessionMeta {
                provider: Some("zai-coding-cn".to_owned()),
                model: "second-model".to_owned(),
                reasoning_effort: None,
                instructions: None,
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
            !ui.replayed.is_empty(),
            "the resumed history was replayed: {}",
            ui.replayed.len()
        );
        // The model is reported by the provider the resumed session names, and
        // the error says which key is missing to run it (the test has none).
        assert_eq!(ui.model.as_deref(), Some("deepseek/second-model"));
        assert!(
            ui.errors[0].contains("/login zai-coding-cn"),
            "{:?}",
            ui.errors
        );
        assert_eq!(ui.resets, 1);
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn resuming_a_session_of_another_provider_binds_that_provider() {
        let (guard, home) = own_home();
        crate::config::store_key("zai-coding-cn", "zai-test").unwrap();
        let dir = tmpdir("resume-provider");
        let mut agent = agent_in(&dir, "deepseek-flash");
        let other = Session::create(
            &dir,
            SessionMeta {
                provider: Some("zai-coding-cn".to_owned()),
                model: "glm-5.3".to_owned(),
                reasoning_effort: Some("low".into()),
                instructions: None,
            },
        )
        .unwrap();
        let id = other.id.clone();
        drop(other);
        session::wait_until_released(&dir.join(format!("{id}.jsonl")));

        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, &format!("/resume {id}")).await;
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert_eq!(
            agent.provider().id,
            "zai-coding-cn",
            "the session's own endpoint"
        );
        assert_eq!(ui.model.as_deref(), Some("zai-coding-cn/glm-5.3"));
        assert_eq!(
            ui.effort.as_deref(),
            Some("low"),
            "the status line takes the resumed session's tier"
        );
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
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
                .any(|m| m.text().as_deref() == Some("hello there")),
            "the user line was persisted before the attempt"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file whose bytes are a PNG's: what `/image` reads.
    fn png(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13],
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn image_asks_about_a_picture_in_one_turn() {
        let dir = tmpdir("image");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        let path = png(&dir, "shot.png");
        assert_eq!(
            submit(
                &mut agent,
                &mut ui,
                &dir,
                &format!("/image {} what is this?", path.display())
            )
            .await,
            Outcome::Continue
        );
        // The picture went into the session as a message of its own: the text
        // asked about it, then the image itself.
        let sent = &agent.session.messages[0];
        assert_eq!(sent.role, crate::types::Role::User);
        assert_eq!(sent.text().as_deref(), Some("what is this?"));
        let images = sent.content.as_ref().unwrap().images();
        assert_eq!(images.len(), 1);
        assert!(images[0].starts_with("data:image/png;base64,"));
        // The line is shown the way a resume shows it: the command is not part of
        // the session, and the message is.
        assert_eq!(ui.replayed.len(), 1);
        assert_eq!(ui.replayed[0], *sent);
        // The turn ran: the backend is unreachable in this test, and that failure
        // is reported rather than fatal.
        assert_eq!(ui.errors.len(), 1, "{:?}", ui.errors);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn image_without_a_path_says_how_to_use_it_and_sends_nothing() {
        let dir = tmpdir("image-empty");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/image").await;
        assert_eq!(ui.info, vec!["usage: /image <path> [text]"]);
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert!(agent.session.messages.is_empty(), "no message was made");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn image_that_cannot_be_read_is_an_error_and_not_a_turn() {
        let dir = tmpdir("image-missing");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        let missing = dir.join("nope.png");
        submit(
            &mut agent,
            &mut ui,
            &dir,
            &format!("/image {}", missing.display()),
        )
        .await;
        assert_eq!(ui.errors.len(), 1, "{:?}", ui.errors);
        assert!(ui.errors[0].contains("nope.png"), "{:?}", ui.errors);
        assert!(agent.session.messages.is_empty(), "nothing was sent");
        assert!(ui.replayed.is_empty(), "and nothing was shown");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn image_takes_a_quoted_path_with_spaces_in_it() {
        let dir = tmpdir("image-quoted");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        let path = png(&dir, "my shot.png");
        submit(
            &mut agent,
            &mut ui,
            &dir,
            &format!("/image \"{}\" look", path.display()),
        )
        .await;
        assert_eq!(agent.session.messages.len(), 1, "{:?}", ui.errors);
        let sent = &agent.session.messages[0];
        assert_eq!(sent.text().as_deref(), Some("look"));
        assert_eq!(
            sent.content.as_ref().unwrap().images().len(),
            1,
            "the quoted path was read whole, spaces and all"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn image_with_an_unterminated_quote_is_refused_before_anything_is_read() {
        let dir = tmpdir("image-unterminated");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/image \"oops/shot.png").await;
        assert_eq!(ui.errors.len(), 0, "{:?}", ui.errors);
        assert!(ui.info[0].contains("unterminated quote"), "{:?}", ui.info);
        assert!(agent.session.messages.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn image_argument_splits_the_path_from_what_was_asked() {
        assert_eq!(
            image_argument("/tmp/shot.png").unwrap(),
            (std::path::PathBuf::from("/tmp/shot.png"), String::new())
        );
        assert_eq!(
            image_argument("/tmp/shot.png  what is this? ").unwrap(),
            (
                std::path::PathBuf::from("/tmp/shot.png"),
                "what is this?".to_owned()
            )
        );
        // A path with spaces is read up to its closing quote, either kind.
        assert_eq!(
            image_argument("\"/tmp/my shot.png\" and this?").unwrap(),
            (
                std::path::PathBuf::from("/tmp/my shot.png"),
                "and this?".to_owned()
            )
        );
        assert_eq!(
            image_argument("'/tmp/my shot.png'").unwrap(),
            (std::path::PathBuf::from("/tmp/my shot.png"), String::new())
        );
        assert!(image_argument("   ").unwrap_err().contains("usage"));
        assert!(image_argument("\"unclosed").unwrap_err().contains("quote"));
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
            "--provider deepseek|zai-coding-cn|minimax",
            "--effort low|high|max",
            "-p \"prompt\"",
            "--image <path>",
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

    /// A HOME of the test's own, with the config lock held, so that what `/login`
    /// writes and what key resolution finds under it are the test's own doing.
    ///
    /// The guard is held across the awaits in these tests on purpose: the
    /// environment has to stay put for as long as the command under test is
    /// reading it, and a test that let go of it halfway would be reading another
    /// test's HOME.
    #[allow(clippy::await_holding_lock)]
    fn own_home() -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
        let guard = crate::config::env_lock();
        let home = crate::config::scratch_home();
        (guard, home)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn login_with_no_provider_prints_the_menu_instead_of_a_question() {
        // What the menu says about a provider is whether a key is stored for it, and
        // a key is read from HOME: a home of its own, under the lock that serializes
        // the tests that read or write one. Without it the menu is a function of
        // whatever home the test process happens to have, and of whichever test last
        // moved it.
        let (guard, home) = own_home();
        let dir = tmpdir("login-menu");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/login").await;
        assert_eq!(ui.info, vec![login_menu()]);
        assert!(ui.prompts.is_empty(), "nothing was asked for yet");
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn login_stores_the_key_where_it_outlives_the_session() {
        let (guard, home) = own_home();
        let dir = tmpdir("login-key");
        let mut agent = agent_in_session_of(&dir, "zai-coding-cn", "glm-5.3-flash");
        let mut ui = Recording {
            secret: Some("  sk-glm-123\n".into()),
            ..Default::default()
        };
        submit(&mut agent, &mut ui, &dir, "/login zai-coding-cn").await;
        // The key is in the file, trimmed of what a paste tends to bring along.
        assert_eq!(
            crate::config::stored_key("zai-coding-cn")
                .unwrap()
                .as_deref(),
            Some("sk-glm-123")
        );
        let path = crate::config::settings_file().unwrap();
        assert!(
            ui.info[0].contains(&path.display().to_string()),
            "{:?}",
            ui.info
        );
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        // The question names the provider by its name; the answer never comes
        // back as text, which is the whole point of asking for it as a secret.
        assert!(ui.prompts[0].contains("Z.AI Coding CN"), "{:?}", ui.prompts);
        for said in ui.info.iter().chain(&ui.errors) {
            assert!(!said.contains("sk-glm-123"), "the key was echoed: {said}");
        }
        assert!(
            !std::fs::read_to_string(&agent.session.path)
                .unwrap()
                .contains("sk-glm-123"),
            "the key reached the session log"
        );
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_cancelled_login_stores_nothing() {
        let (guard, home) = own_home();
        let dir = tmpdir("login-cancel");
        let mut agent = agent_in(&dir, "m");
        for answer in [None, Some(String::new()), Some("   ".into())] {
            let mut ui = Recording {
                secret: answer,
                ..Default::default()
            };
            submit(&mut agent, &mut ui, &dir, "/login deepseek").await;
            assert!(ui.errors.is_empty(), "{:?}", ui.errors);
            assert!(ui.info[0].contains("nothing was stored"), "{:?}", ui.info);
        }
        assert_eq!(crate::config::stored_key("deepseek").unwrap(), None);
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn login_for_a_provider_that_does_not_exist_is_an_error() {
        let dir = tmpdir("login-bogus");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/login nope").await;
        assert!(ui.errors[0].contains("unknown provider"), "{:?}", ui.errors);
        assert!(ui.prompts.is_empty(), "it never got as far as asking");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn model_with_no_argument_prints_the_menu_instead_of_switching() {
        // The menu marks the default model of every provider that has a key, so it is
        // a function of HOME -- see the login menu above.
        let (guard, home) = own_home();
        let dir = tmpdir("model-menu");
        let mut agent = agent_in(&dir, "deepseek-v4-flash");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/model").await;
        assert_eq!(ui.info, vec![model_menu("deepseek/deepseek-v4-flash")]);
        assert!(
            ui.info[0].contains("deepseek/deepseek-flash"),
            "{:?}",
            ui.info
        );
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn model_switches_the_session_and_reports_it_by_provider() {
        let (guard, home) = own_home();
        crate::config::store_key("deepseek", "sk-test").unwrap();
        let dir = tmpdir("model-switch");
        let mut agent = agent_in(&dir, "deepseek-flash");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/model deepseek-v4-pro").await;
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert_eq!(agent.session.meta.model, "deepseek-v4-pro");
        assert_eq!(agent.session.meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(ui.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(ui.resets, 1, "the cache figures belonged to the old model");
        // The log is the state: a resume has to find the model there.
        let log = std::fs::read_to_string(&agent.session.path).unwrap();
        assert!(log.contains("deepseek-v4-pro"), "{log}");
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_qualified_model_moves_the_session_to_that_provider() {
        let (guard, home) = own_home();
        crate::config::store_key("zai-coding-cn", "zai-test").unwrap();
        let dir = tmpdir("model-provider");
        let mut agent = agent_in(&dir, "deepseek-flash");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/model zai-coding-cn/glm-5.3").await;
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert_eq!(
            agent.provider().id,
            "zai-coding-cn",
            "the endpoint moved with it"
        );
        assert_eq!(
            agent.session.meta.provider.as_deref(),
            Some("zai-coding-cn")
        );
        assert_eq!(agent.session.meta.model, "glm-5.3");
        assert_eq!(ui.model.as_deref(), Some("zai-coding-cn/glm-5.3"));
        assert_eq!(
            ui.effort.as_deref(),
            Some("max"),
            "the status line follows the tier in effect"
        );
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_model_with_no_key_behind_it_is_refused_and_changes_nothing() {
        let (guard, home) = own_home();
        let dir = tmpdir("model-nokey");
        let mut agent = agent_in(&dir, "deepseek-flash");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/model zai-coding-cn/glm-5.3").await;
        let err = &ui.errors[0];
        assert!(
            err.contains("/login zai-coding-cn"),
            "it says what to do: {err}"
        );
        assert_eq!(agent.provider().id, "deepseek", "nothing moved");
        assert_eq!(agent.session.meta.model, "deepseek-flash");
        assert_eq!(ui.model, None, "and the status line was left alone");
        assert_eq!(ui.resets, 0);
        drop(guard);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn a_model_for_an_unknown_provider_is_an_error() {
        let dir = tmpdir("model-bogus");
        let mut agent = agent_in(&dir, "deepseek-flash");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/model nope/whatever").await;
        assert!(ui.errors[0].contains("unknown provider"), "{:?}", ui.errors);
        assert_eq!(ui.resets, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn effort_with_no_argument_prints_the_menu_instead_of_switching() {
        let dir = tmpdir("effort-menu");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/effort").await;
        assert_eq!(
            ui.info,
            vec![effort_menu(&agent.provider(), agent.effort_label())]
        );
        assert_eq!(
            ui.effort.as_deref(),
            None,
            "the menu is a question, not a switch"
        );
        // Every tier the provider offers, and the one in effect marked.
        for tier in agent.provider().efforts {
            assert!(ui.info[0].contains(tier), "{:?}", ui.info);
        }
        assert!(ui.info[0].contains("current"), "{:?}", ui.info);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn effort_switches_the_session_and_is_stored_in_the_log() {
        let dir = tmpdir("effort-switch");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/effort low").await;
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert_eq!(agent.session.meta.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(ui.info, vec!["effort low"]);
        assert_eq!(ui.effort.as_deref(), Some("low"), "the status line took it");
        // The log is the state: a resume has to find the tier there.
        let log = std::fs::read_to_string(&agent.session.path).unwrap();
        assert!(log.contains("\"reasoning_effort\":\"low\""), "{log}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_effort_the_provider_does_not_offer_is_refused_and_changes_nothing() {
        let dir = tmpdir("effort-bogus");
        let mut agent = agent_in(&dir, "m");
        let mut ui = Recording::default();
        submit(&mut agent, &mut ui, &dir, "/effort bogus").await;
        assert!(ui.info.is_empty(), "{:?}", ui.info);
        assert_eq!(ui.effort, None, "the status line was left alone");
        assert!(
            ui.errors[0].contains("low | high | max"),
            "it says what is available: {:?}",
            ui.errors
        );
        assert!(
            ui.errors[0].contains("DeepSeek"),
            "and which provider refused: {:?}",
            ui.errors
        );
        assert_eq!(
            agent.session.meta.reasoning_effort, None,
            "nothing was stored"
        );
        // Nothing was appended either: the header is still the whole file.
        let log = std::fs::read_to_string(&agent.session.path).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_menus_are_built_from_the_same_tables_the_pickers_read() {
        // Structure, not text: a provider that cannot be logged into, or a model
        // that cannot be chosen, is the drift this catches. A home of its own under
        // the lock, since the details beside a name are read from HOME and a menu
        // built beside a key search is a menu built from two homes.
        let _guard = crate::config::env_lock();
        let menu = login_menu();
        for row in crate::config::provider_choices() {
            assert!(menu.contains(&row.label), "{row:?} has no name on the menu");
            // The plain prompt is typed at, so it has to show what to type: the
            // name is for reading, the id is what `/login` takes.
            assert!(
                menu.contains(&row.argument),
                "{row:?} has no id on the menu"
            );
        }
        let menu = model_menu("deepseek/deepseek-flash");
        for row in crate::config::model_menu("deepseek/deepseek-flash") {
            assert!(
                menu.contains(&row.label),
                "{row:?} is not on the model menu"
            );
        }
        let menu = effort_menu(&provider::DEEPSEEK, "high");
        for row in crate::config::effort_menu(&provider::DEEPSEEK, "high") {
            assert!(
                menu.contains(&row.label),
                "{row:?} is not on the effort menu"
            );
        }
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
