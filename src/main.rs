mod agent;
mod api;
mod cli;
mod config;
mod history;
mod machine;
mod provider;
mod repl;
mod session;
mod tools;
mod types;
mod ui;

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::Parser;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};

use crate::agent::{Agent, Approval};
use crate::api::Client;
use crate::cli::Cli;
use crate::session::{Session, SessionMeta};
use crate::ui::tui;
use crate::ui::{Front, Renderer};
use crate::ui::{Sigint, StdinApproval};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

/// Meta for a new session: the provider is the one this run selected, and the
/// model comes from `--model` — which may name a provider of its own as
/// `<provider>/<modelid>` — otherwise it is that provider's default.
fn fresh_meta(cli: &Cli, start: provider::Provider) -> Result<SessionMeta> {
    let (provider, model) = match &cli.model {
        Some(spec) => provider::model_spec(spec, start.id)?,
        None => (start, start.default_model().to_string()),
    };
    // Checked against the provider the session will run on — which `--model`
    // may name, independently of the run's own choice.
    if let Some(e) = &cli.effort {
        provider.validate_effort(e)?;
    }
    Ok(SessionMeta {
        provider: Some(provider.id.to_string()),
        model,
        reasoning_effort: cli
            .effort
            .clone()
            .or_else(|| Some(provider.default_effort.to_string())),
    })
}

/// When resuming a session: parameters given explicitly on the command line
/// override the meta, while those not given keep the value stored in the session.
/// Returning true means the meta changed (a meta line must be appended).
fn apply_overrides(
    meta: &mut SessionMeta,
    cli: &Cli,
    fallback: provider::Provider,
) -> Result<bool> {
    let mut changed = false;
    // The provider a bare model id belongs to: the session's own, if it names
    // one, and this run's choice otherwise.
    let current = match meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => fallback,
    };
    match (&cli.provider, &cli.model) {
        // An explicit model settles the provider too:
        // `--model zai-coding-cn/glm-5.3` needs no `--provider` beside it, and a
        // bare id stays where it was.
        (_, Some(spec)) => {
            let id = cli.provider.as_deref().unwrap_or(current.id);
            let (chosen, model) = provider::model_spec(spec, id)?;
            if meta.provider.as_deref() != Some(chosen.id) {
                meta.provider = Some(chosen.id.to_string());
                changed = true;
            }
            if meta.model != model {
                meta.model = model;
                changed = true;
            }
        }
        // An explicit provider switch without a model: follow that provider's
        // default model, so the old model name is not sent to the new backend.
        (Some(id), None) => {
            let chosen = provider::provider(id)?;
            if meta.provider.as_deref() != Some(chosen.id) || meta.model != chosen.default_model() {
                meta.provider = Some(chosen.id.to_string());
                meta.model = chosen.default_model().to_string();
                changed = true;
            }
        }
        (None, None) => {}
    }
    // The effort, when given, is checked against the provider the session ends
    // up on — the one `--model`/`--provider` chose above, or the session's own
    // when neither was given.
    if let Some(e) = &cli.effort {
        let on = match meta.provider.as_deref() {
            Some(id) => provider::provider(id)?,
            None => current,
        };
        on.validate_effort(e)?;
    }
    if let Some(e) = &cli.effort
        && meta.reasoning_effort.as_deref() != Some(e.as_str())
    {
        meta.reasoning_effort = Some(e.clone());
        changed = true;
    }
    Ok(changed)
}

/// Ctrl-J inserts a newline instead of submitting, for multi-line input; Enter
/// still submits the whole thing.
/// rustyline binds both Ctrl-J and Enter to AcceptOrInsertLine by default, so
/// Ctrl-J is overridden here.
fn enable_multiline(rl: &mut rustyline::DefaultEditor) {
    let _ = rl.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
}

async fn run(cli: Cli) -> Result<()> {
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

    // The provider this run starts from: --provider, or the default. It settles
    // the endpoint of a new session and of one whose provider is not recorded;
    // `--model <provider>/<model>` and a resumed session's own meta name one too.
    let start = provider::provider(
        cli.provider
            .as_deref()
            .unwrap_or(provider::DEFAULT_PROVIDER),
    )?;

    // Session resolution priority: --resume > --continue > create new
    let mut session = if let Some(id) = &cli.resume {
        let path = sdir.join(format!("{id}.jsonl"));
        Session::load(&path).with_context(|| format!("failed to resume session {id}"))?
    } else if cli.cont {
        match session::latest(sdir.clone())? {
            Some(path) => Session::load(&path)?,
            None => Session::create(&sdir, fresh_meta(&cli, start)?)?,
        }
    } else {
        Session::create(&sdir, fresh_meta(&cli, start)?)?
    };

    if apply_overrides(&mut session.meta, &cli, start)? {
        session.set_meta(session.meta.clone())?;
    }

    // The provider this session runs on: the one its meta names, or this run's
    // choice for a session written before providers were recorded.
    let provider = match session.meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => start,
    };

    // Authentication is settled before the session. A key that is missing is not
    // fatal where it can be stored -- `/login` exists for exactly that -- but a
    // one-shot run has no way to ask, so there it fails now rather than at the
    // first request.
    let mut missing_key = None;
    let api = match config::api_key(&provider) {
        Ok(key) => Client::for_provider(&provider, key)?,
        Err(e) if cli.prompt.is_some() => return Err(e),
        Err(e) => {
            missing_key = Some(format!("{e:#}"));
            Client::for_provider(&provider, String::new())?
        }
    };

    let mut agent = Agent::new(api, session, provider);
    agent.approval = Approval::from_flag(cli.ask);
    ui.set_model(&agent.model_label());
    ui.set_effort(agent.effort_label());

    // One-shot mode (the agent's primary self-test channel)
    if let Some(prompt) = &cli.prompt {
        ui.info(&format!(
            "session {} · {} · effort {}",
            agent.session.id,
            agent.model_label(),
            agent.effort_label()
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

    // The session summary: which session this is, how much is in it, and the
    // model. Built by the front end rather than here, because how much of it fits
    // is the terminal's answer and not the application's. A key that is missing is
    // said here as well: it is the first thing the user has to fix, and `/login`
    // is how.
    let mut banner = ui::banner(
        &agent.session.id,
        agent.session.messages.len(),
        &agent.model_label(),
    );
    if let Some(note) = missing_key {
        banner = format!("{note}\n{banner}");
    }

    // Interactive front end: it owns the terminal, so nothing may have been
    // printed before it and nothing may be printed after it while it runs.
    // It declines rather than fails when the terminal cannot host it: the
    // alternate screen and raw mode are process-wide, and a terminal that cannot
    // take them leaves the plain prompt as the only one there is.
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

    /// A meta as a session file holds it, with the provider recorded.
    fn meta_of(provider: &str, model: &str) -> SessionMeta {
        SessionMeta {
            provider: Some(provider.into()),
            model: model.into(),
            reasoning_effort: None,
        }
    }

    #[test]
    fn no_cli_args_keeps_session_meta_untouched() {
        // Without --effort, the default tier max must not be written into an
        // existing session; the value stored in the session has to be kept.
        let mut meta = meta_of("deepseek", "deepseek-v4-pro");
        meta.reasoning_effort = Some("low".into());
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::DEEPSEEK).unwrap());
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn no_cli_args_keeps_the_sessions_own_provider() {
        // A resumed session runs where it ran before, whatever this run's
        // `--provider` default is: the meta is what the session is.
        let mut meta = meta_of("zai-coding-cn", "glm-5.3");
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::DEEPSEEK).unwrap());
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3");
    }

    #[test]
    fn explicit_model_overrides_only_model() {
        let mut meta = meta_of("deepseek", "deepseek-flash");
        meta.reasoning_effort = Some("high".into());
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "deepseek-v4-pro"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(meta.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn a_qualified_model_carries_its_own_provider() {
        // `--model zai-coding-cn/glm-5.3` needs no `--provider` beside it, and on
        // a session that is running elsewhere it moves the session.
        let mut meta = meta_of("deepseek", "deepseek-flash");
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "zai-coding-cn/glm-5.3"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3");
    }

    #[test]
    fn a_bare_model_stays_with_the_sessions_provider() {
        let mut meta = meta_of("zai-coding-cn", "glm-5.3-flash");
        assert!(
            apply_overrides(&mut meta, &cli(&["--model", "glm-4.6"]), provider::DEEPSEEK).unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-4.6");
    }

    #[test]
    fn a_session_from_before_providers_were_recorded_takes_this_runs_choice() {
        // No provider in the meta: the run's provider is what the model is sent
        // to, which is how every session behaved before the field existed.
        let mut meta = SessionMeta {
            provider: None,
            model: "deepseek-v4-pro".into(),
            reasoning_effort: None,
        };
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::ZAI_CODING_CN).unwrap());
        assert_eq!(meta.provider, None, "nothing is invented for it");
        assert_eq!(meta.model, "deepseek-v4-pro");
    }

    #[test]
    fn switching_provider_without_model_follows_default_model() {
        let mut meta = meta_of("deepseek", "deepseek-flash");
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--provider", "zai-coding-cn"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3-flash");

        // an explicit --model wins over the provider default
        let mut meta2 = meta_of("deepseek", "deepseek-flash");
        assert!(
            apply_overrides(
                &mut meta2,
                &cli(&["--provider", "zai-coding-cn", "--model", "glm-4.6"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta2.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta2.model, "glm-4.6");
    }

    #[test]
    fn switching_to_the_provider_the_session_already_names_changes_nothing() {
        // A meta line is a write to an append-only log, so the same choice must
        // not put another one in it.
        let mut meta = meta_of("zai-coding-cn", "glm-5.3-flash");
        assert!(
            !apply_overrides(
                &mut meta,
                &cli(&["--provider", "zai-coding-cn"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        // ... but a model that is not the provider's default does move.
        let mut meta2 = meta_of("zai-coding-cn", "glm-4.6");
        assert!(
            apply_overrides(
                &mut meta2,
                &cli(&["--provider", "zai-coding-cn"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta2.model, "glm-5.3-flash");
    }

    #[test]
    fn an_unknown_model_provider_is_rejected_before_anything_runs() {
        let mut meta = meta_of("deepseek", "deepseek-flash");
        let err = apply_overrides(&mut meta, &cli(&["--model", "nope/x"]), provider::DEEPSEEK)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn effort_overrides_stored_value() {
        let mut meta = meta_of("deepseek", "deepseek-v4-flash");
        meta.reasoning_effort = Some("high".into());
        assert!(
            apply_overrides(&mut meta, &cli(&["--effort", "low"]), provider::DEEPSEEK).unwrap()
        );
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));

        // an identical value does not trigger a write
        assert!(
            !apply_overrides(&mut meta, &cli(&["--effort", "low"]), provider::DEEPSEEK).unwrap()
        );
    }

    #[test]
    fn fresh_meta_defaults() {
        let meta = fresh_meta(&cli(&[]), provider::DEEPSEEK).unwrap();
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(meta.model, provider::DEEPSEEK.default_model());
        assert_eq!(
            meta.reasoning_effort.as_deref(),
            Some(provider::DEEPSEEK.default_effort)
        );
    }

    #[test]
    fn fresh_meta_uses_provider_default_model() {
        let meta = fresh_meta(&cli(&[]), provider::ZAI_CODING_CN).unwrap();
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3-flash");
    }

    #[test]
    fn fresh_meta_carries_model_and_effort() {
        let meta = fresh_meta(
            &cli(&["--model", "deepseek-v4-pro", "--effort", "max"]),
            provider::DEEPSEEK,
        )
        .unwrap();
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn fresh_meta_takes_the_provider_a_qualified_model_names() {
        let meta = fresh_meta(
            &cli(&["--model", "zai-coding-cn/glm-5.3"]),
            provider::DEEPSEEK,
        )
        .unwrap();
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3");
    }

    #[test]
    fn provider_flag_parses() {
        assert_eq!(
            cli(&["--provider", "zai-coding-cn"]).provider.as_deref(),
            Some("zai-coding-cn")
        );
        assert!(cli(&[]).provider.is_none());
    }

    #[test]
    fn an_effort_the_provider_does_not_offer_is_rejected_where_it_lands() {
        // A new session is checked against the provider it will run on — the one
        // `--model` names, when it names one.
        let err = fresh_meta(&cli(&["--effort", "bogus"]), provider::DEEPSEEK)
            .unwrap_err()
            .to_string();
        assert!(err.contains("low | high | max"), "{err}");
        let err = fresh_meta(
            &cli(&["--model", "zai-coding-cn/glm-5.3", "--effort", "bogus"]),
            provider::DEEPSEEK,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Z.AI Coding CN"), "{err}");
        // A resumed session is checked against the provider its meta names,
        // not against this run's choice.
        let mut meta = meta_of("zai-coding-cn", "glm-5.3");
        let err = apply_overrides(&mut meta, &cli(&["--effort", "bogus"]), provider::DEEPSEEK)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Z.AI Coding CN"), "{err}");
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
