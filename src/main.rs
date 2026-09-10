mod agent;
mod api;
mod cli;
mod config;
mod history;
mod machine;
mod repl;
mod session;
mod tools;
mod types;
mod ui;

use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use clap::Parser;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};

use crate::agent::{Agent, Sigint, StdinApproval};
use crate::api::Client;
use crate::cli::Cli;
use crate::session::{Session, SessionMeta};
use crate::ui::tui;
use crate::ui::{Front, Renderer};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

/// Meta for a new session: the model comes from `--model`, otherwise the
/// provider's default model is used.
fn fresh_meta(cli: &Cli, provider: config::Provider) -> SessionMeta {
    SessionMeta {
        model: cli
            .model
            .clone()
            .unwrap_or_else(|| provider.default_model.to_string()),
        reasoning_effort: cli
            .effort
            .clone()
            .or_else(|| Some(config::DEFAULT_EFFORT.to_string())),
    }
}

/// When resuming a session: parameters given explicitly on the command line
/// override the meta, while those not given keep the value stored in the session.
/// Returning true means the meta changed (a meta line must be appended).
fn apply_overrides(meta: &mut SessionMeta, cli: &Cli, provider: config::Provider) -> bool {
    let mut changed = false;
    // An explicit provider switch without a model: follow that provider's default
    // model, so the old model name is not sent to the new backend
    if cli.provider.is_some() && cli.model.is_none() && meta.model != provider.default_model {
        meta.model = provider.default_model.to_string();
        changed = true;
    }
    if let Some(m) = &cli.model
        && meta.model != *m
    {
        meta.model = m.clone();
        changed = true;
    }
    if let Some(e) = &cli.effort
        && meta.reasoning_effort.as_deref() != Some(e.as_str())
    {
        meta.reasoning_effort = Some(e.clone());
        changed = true;
    }
    changed
}

/// Ctrl-J inserts a newline instead of submitting, for multi-line input; Enter
/// still submits the whole thing.
/// rustyline binds both Ctrl-J and Enter to AcceptOrInsertLine by default, so
/// Ctrl-J is overridden here.
fn enable_multiline(rl: &mut rustyline::DefaultEditor) {
    let _ = rl.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
}

/// `--effort` accepts only `low|high|max`. DeepSeek returns 400 for an
/// out-of-range value while GLM silently accepts it and degrades to its default
/// tier — so reject it locally first, which is the only way to keep the two
/// consistent.
fn validate_effort(cli: &Cli) -> Result<()> {
    if let Some(e) = &cli.effort
        && !config::EFFORTS.contains(&e.as_str())
    {
        bail!(
            "invalid --effort {e:?}; available: {}",
            config::EFFORTS.join(" | ")
        );
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    validate_effort(&cli)?;
    let mut ui = Renderer::new();
    let sdir = config::sessions_dir()?;

    if cli.list {
        for s in session::list(&sdir)? {
            println!(
                "{}\t{} messages\t{}\t{}",
                s.id,
                s.message_count,
                s.preview,
                s.path.display()
            );
        }
        return Ok(());
    }

    // Provider and authentication are settled before the session: --provider
    // determines the endpoint, the default model and the key environment variable
    let provider = config::provider(cli.provider.as_deref().unwrap_or(config::DEFAULT_PROVIDER))?;
    let api = Client::new(config::api_key(&provider)?, provider.url.to_string())?;

    // Session resolution priority: --resume > --continue > create new
    let mut session = if let Some(id) = &cli.resume {
        let path = sdir.join(format!("{id}.jsonl"));
        Session::load(&path).with_context(|| format!("failed to resume session {id}"))?
    } else if cli.cont {
        match session::latest(sdir.clone())? {
            Some(path) => Session::load(&path)?,
            None => Session::create(&sdir, fresh_meta(&cli, provider))?,
        }
    } else {
        Session::create(&sdir, fresh_meta(&cli, provider))?
    };

    if apply_overrides(&mut session.meta, &cli, provider) {
        session.set_meta(session.meta.clone())?;
    }

    let mut agent = Agent::new(api, session);
    agent.confirm_tools = cli.ask;
    ui.set_model(&agent.session.meta.model);

    // One-shot mode (the agent's primary self-test channel)
    if let Some(prompt) = &cli.prompt {
        ui.info(&format!(
            "session {} · {}{}",
            agent.session.id,
            agent.session.meta.model,
            agent
                .session
                .meta
                .reasoning_effort
                .as_deref()
                .map(|e| format!(" · effort {e}"))
                .unwrap_or_default()
        ));
        // One interrupt listener per turn, subscribed before the turn's first
        // await point (see Agent::turn).
        let mut interrupt = Sigint::new()?;
        let mut approve = StdinApproval;
        if let Err(e) = agent
            .turn(prompt, &mut ui, &mut interrupt, &mut approve)
            .await
        {
            ui.error(&format!("{e:#}"));
            std::process::exit(1);
        }
        return Ok(());
    }

    // After --continue / --resume, show the source file; the path has diagnostic
    // value
    let banner = format!(
        "caocli · session {} ({} messages, {}) · {} · /help for commands",
        agent.session.id,
        agent.session.messages.len(),
        agent.session.path.display(),
        agent.session.meta.model
    );

    // Interactive front end: it owns the terminal, so nothing may have been
    // printed before it and nothing may be printed after it while it runs.
    // It declines rather than fails when the terminal cannot host it -- an
    // inline viewport has to ask where the cursor is, and a terminal that does
    // not answer leaves no way to place it.
    if !cli.no_tui && std::io::stdout().is_terminal() {
        // Cloned because the front end borrows the agent mutably for the whole
        // session; this is once, at startup.
        let history = agent.session.messages.clone();
        if tui::run(&mut agent, &sdir, &banner, &history).await? {
            return Ok(());
        }
    }

    // REPL
    let mut rl = rustyline::DefaultEditor::new()?;
    enable_multiline(&mut rl);
    let hist_path = config::history_file()?;
    let _ = rl.load_history(&hist_path);

    // Bottom status bar: only enabled in the REPL on a TTY
    if !cli.no_status_bar {
        ui.refresh_status_bar();
    }

    ui.info(&banner);
    // A resumed session replays its history to the screen, otherwise only the
    // banner is visible and there is no context
    if !agent.session.messages.is_empty() {
        ui.replay(&agent.session.messages);
    }

    loop {
        // Sync the status bar before each input (which also handles window resizes)
        if !cli.no_status_bar {
            ui.refresh_status_bar();
        }
        match rl.readline("› ") {
            Ok(raw) => {
                let line = raw.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                // One interrupt listener per turn, subscribed before the turn's
                // first await point (see Agent::turn).
                let mut interrupt = Sigint::new()?;
                let mut approve = StdinApproval;
                let outcome = repl::handle(
                    &mut agent,
                    &mut ui,
                    &sdir,
                    line,
                    &mut interrupt,
                    &mut approve,
                )
                .await?;
                if outcome == repl::Outcome::Exit {
                    break;
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue, // Ctrl-C clears the line
            Err(rustyline::error::ReadlineError::Eof) => break,            // Ctrl-D exits
            Err(e) => {
                ui.error(&format!("readline error: {e}"));
                break;
            }
        }
    }
    let _ = rl.save_history(&hist_path);
    ui.teardown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("caocli").chain(args.iter().copied()))
    }

    #[test]
    fn no_cli_args_keeps_session_meta_untouched() {
        // Without --effort, the default tier max must not be written into an
        // existing session; the value stored in the session has to be kept.
        let mut meta = SessionMeta {
            model: "deepseek-v4-pro".into(),
            reasoning_effort: Some("low".into()),
        };
        assert!(!apply_overrides(&mut meta, &cli(&[]), config::DEEPSEEK));
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn explicit_model_overrides_only_model() {
        let mut meta = SessionMeta {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: Some("high".into()),
        };
        assert!(apply_overrides(
            &mut meta,
            &cli(&["--model", "deepseek-v4-pro"]),
            config::DEEPSEEK
        ));
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn switching_provider_without_model_follows_default_model() {
        let mut meta = SessionMeta {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: None,
        };
        assert!(apply_overrides(
            &mut meta,
            &cli(&["--provider", "glm"]),
            config::GLM
        ));
        assert_eq!(meta.model, "GLM-5.3-Flash");

        // an explicit --model wins over the provider default
        let mut meta2 = SessionMeta {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: None,
        };
        assert!(apply_overrides(
            &mut meta2,
            &cli(&["--provider", "glm", "--model", "glm-4.6"]),
            config::GLM
        ));
        assert_eq!(meta2.model, "glm-4.6");
    }

    #[test]
    fn effort_overrides_stored_value() {
        let mut meta = SessionMeta {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: Some("high".into()),
        };
        assert!(apply_overrides(
            &mut meta,
            &cli(&["--effort", "low"]),
            config::DEEPSEEK
        ));
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));

        // an identical value does not trigger a write
        assert!(!apply_overrides(
            &mut meta,
            &cli(&["--effort", "low"]),
            config::DEEPSEEK
        ));
    }

    #[test]
    fn fresh_meta_defaults() {
        let meta = fresh_meta(&cli(&[]), config::DEEPSEEK);
        assert_eq!(meta.model, config::DEEPSEEK.default_model);
        assert_eq!(
            meta.reasoning_effort.as_deref(),
            Some(config::DEFAULT_EFFORT)
        );
    }

    #[test]
    fn fresh_meta_uses_provider_default_model() {
        let meta = fresh_meta(&cli(&[]), config::GLM);
        assert_eq!(meta.model, "GLM-5.3-Flash");
    }

    #[test]
    fn fresh_meta_carries_model_and_effort() {
        let meta = fresh_meta(
            &cli(&["--model", "deepseek-v4-pro", "--effort", "max"]),
            config::DEEPSEEK,
        );
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn provider_flag_parses() {
        assert_eq!(cli(&["--provider", "glm"]).provider.as_deref(), Some("glm"));
        assert!(cli(&[]).provider.is_none());
    }

    #[test]
    fn validate_effort_accepts_known_tiers_and_rejects_others() {
        for ok in ["low", "high", "max"] {
            assert!(validate_effort(&cli(&["--effort", ok])).is_ok(), "{ok}");
        }
        assert!(validate_effort(&cli(&[])).is_ok()); // not passed = the backend default tier
        for bad in ["none", "minimal", "medium", "xhigh", "HIGH", "bogus", ""] {
            let err = validate_effort(&cli(&["--effort", bad]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("low | high | max"), "{bad}: {err}");
        }
    }

    #[test]
    fn no_status_bar_flag_parses() {
        assert!(cli(&["--no-status-bar"]).no_status_bar);
        assert!(!cli(&[]).no_status_bar);
    }

    #[test]
    fn ask_flag_defaults_off_and_parses() {
        assert!(
            !cli(&[]).ask,
            "execution is trusted by default, so do not ask"
        );
        assert!(cli(&["--ask"]).ask);
    }

    #[test]
    fn ctrl_j_is_bound_to_newline() {
        let mut rl = rustyline::DefaultEditor::new().unwrap();
        enable_multiline(&mut rl);
        // Binding an already-bound key again returns the previous handler: this
        // proves Ctrl-J is really taken, otherwise it would still go to the default
        // AcceptOrInsertLine (Enter semantics, no newline).
        let prev = rl.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
        assert!(prev.is_some());
    }
}
