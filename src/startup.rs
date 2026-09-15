//! Startup: the resolution pipeline that turns a parsed [`Cli`] into everything
//! `main::run` needs to drive the agent.
//!
//! Kept apart from `main` because the resolution is the part that grows: a new
//! flag, a new session source, a new front end all add a line here, not in
//! `run`. Reading `run` after that change shows only the dispatch, which is
//! what a reader wants to see.
//!
//! [`resolve_startup`] is the single entry point. It is sync (it does no IO
//! beyond reading the sessions directory and `~/.caocli/settings.json`); the
//! MCP hub is connected afterwards, because that part is async and the
//! tools it offers are part of every request, not part of the resolution.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use crate::agent::Approval;
use crate::api::Client;
use crate::cli::{Cli, Mode};
use crate::config::{self, ApiKey};
use crate::provider::{self, Provider};
use crate::session::{Session, SessionMeta, SessionSource};
use crate::ui::glyphs;
use crate::ui::terminal::{RealTerminal, Terminal};
use crate::ui::theme;

/// Everything `main::run` needs to drive the agent: the session, the provider
/// the request will go to, the client bound to that provider, and the policy
/// that decides which tool calls ask first.
///
/// `api_key` is kept alongside `client` so the banner can name what's missing
/// — `client` was built from it (with an empty string when it was absent), but
/// the banner needs the original hint to tell the user what to do about it.
pub struct Startup {
    pub session: Session,
    pub provider: Provider,
    pub client: Client,
    pub approval: Approval,
    pub api_key: ApiKey,
    pub mode: Mode,
    pub workspace: PathBuf,
    pub sessions_dir: PathBuf,
}

/// Resolve a parsed CLI into a fully-built startup state.
///
/// One-shot runs fail fast on a missing key: the one-shot prompt has nowhere
/// to ask, and the failure belongs before any work has been done. Interactive
/// runs continue with an empty key — the banner carries the hint and `/login`
/// is how it gets fixed.
pub fn resolve_startup(cli: &Cli) -> Result<Startup> {
    let mode = cli.mode()?;
    let workspace = std::env::current_dir().context("cannot determine the working directory")?;
    let sessions_dir = config::sessions_dir()?;

    // The provider this run starts from: `--model`, when it names one of its
    // own; otherwise the first provider with a key behind it — so a fresh run
    // does not start by talking to a backend it has no key for.
    //
    // A machine where no provider has a key is a machine that has not run
    // `/login` yet, and `/login` is a command of the interactive session: a run
    // that refused to start without a key would be a door locked from the
    // inside. So those modes start on the first provider, unfunded — the banner
    // says which command fixes it, and `/login` for that provider (or `/model`
    // for another) is one keystroke away. A one-shot run has nowhere to ask, and
    // fails here, before any work has been done.
    let start = match &cli.model {
        Some(spec) => provider::model_spec(spec)?.0,
        None => match first_available_provider() {
            Some(start) => start,
            None if matches!(mode, Mode::OneShot { .. }) => bail!("{}", no_key_hint()),
            None => provider::PROVIDERS[0],
        },
    };

    let source = SessionSource::from_cli(cli.resume.as_deref(), cli.cont);
    let mut session = source.resolve(&sessions_dir, || fresh_meta(cli, start))?;

    // The provider the session will run on: meta's choice for sessions that
    // recorded one, this run's choice for sessions written before they did.
    let on = match session.meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => start,
    };

    // Apply CLI overrides to a resumed session; persist the meta line only
    // when something actually changed, so `--list` is not a meta-line writer.
    if apply_overrides(&mut session.meta, cli, start)? {
        session.set_meta(session.meta.clone())?;
    }

    let api_key = ApiKey::load(&on)?;
    let client = build_client(&on, api_key.clone(), &mode)?;

    Ok(Startup {
        session,
        provider: on,
        client,
        approval: Approval::from_flag(cli.ask),
        api_key,
        mode,
        workspace,
        sessions_dir,
    })
}

/// First provider in [`provider::PROVIDERS`] that has a stored key, with its
/// `models[0]` as the model.
///
/// `None` is a machine where nothing has been logged into yet, and what a run
/// makes of that depends on whether it can be asked — see [`resolve_startup`],
/// which is the only caller.
fn first_available_provider() -> Option<provider::Provider> {
    provider::PROVIDERS
        .iter()
        .find(|p| config::has_key(p))
        .copied()
}

/// What a run that cannot ask says when no provider has a key: every provider,
/// and the command that would give it one.
fn no_key_hint() -> String {
    let names = provider::PROVIDERS
        .iter()
        .map(|p| format!("{} (/login {})", p.name, p.id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "no provider has a stored key; start caocli and run /login <provider id> for one of: {names}"
    )
}

/// Build the client the session will use, with one-shot runs failing on a
/// missing key. The key itself is moved into the client on the present path,
/// and `api_key` is captured separately in `Startup` for the banner.
fn build_client(provider: &Provider, key: ApiKey, mode: &Mode) -> Result<Client> {
    if matches!(mode, Mode::OneShot { .. }) {
        return Client::for_provider(provider, key.require()?);
    }
    match key {
        ApiKey::Present(k) => Client::for_provider(provider, k),
        ApiKey::Missing { .. } => Client::for_provider(provider, String::new()),
    }
}

/// Meta for a new session: the model comes from `--model`, which names a
/// provider of its own as `<provider>/<modelid>`. When no `--model` is given,
/// the run's `start` provider — the first one with a stored key — is used, with
/// its `models[0]` as the model. The workspace's project instructions (its
/// AGENTS.md files) are read here, once, and frozen into the session:
/// everything the session later sends is what it stored, never what the files
/// say by then.
fn fresh_meta(cli: &Cli, start: Provider) -> Result<SessionMeta> {
    let (provider, model) = match &cli.model {
        Some(spec) => provider::model_spec(spec)?,
        None => (start, start.models[0].to_string()),
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
        instructions: crate::agents_md::load(),
    })
}

/// When resuming a session: parameters given explicitly on the command line
/// override the meta, while those not given keep the value stored in the session.
/// Returning true means the meta changed (a meta line must be appended).
fn apply_overrides(meta: &mut SessionMeta, cli: &Cli, fallback: Provider) -> Result<bool> {
    let mut changed = false;
    if let Some(spec) = &cli.model {
        let (chosen, model) = provider::model_spec(spec)?;
        if meta.provider.as_deref() != Some(chosen.id) {
            meta.provider = Some(chosen.id.to_string());
            changed = true;
        }
        if meta.model != model {
            meta.model = model;
            changed = true;
        }
    }
    // The provider the session ends up on: the one `--model` chose above, the
    // session's own meta when neither was given, or the run's `start` for the
    // pre-providers sessions whose meta did not record a provider.
    let on = match meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => fallback,
    };
    // The effort, when given, is checked against that provider.
    if let Some(e) = &cli.effort {
        on.validate_effort(e)?;
    }
    if let Some(e) = &cli.effort
        && meta.reasoning_effort.as_deref() != Some(e.as_str())
    {
        meta.reasoning_effort = Some(e.clone());
        changed = true;
    }
    // A tier the provider does not offer is not carried into it: the session
    // would send a value this backend may reject, and show a tier it is not
    // running on. See `Provider::fit_effort`.
    let fitted = on.fit_effort(meta.reasoning_effort.as_deref());
    if fitted != meta.reasoning_effort {
        meta.reasoning_effort = fitted;
        changed = true;
    }
    Ok(changed)
}

/// Settle how this run paints and how it draws, and install both.
///
/// Called by `main` before anything is written to the terminal, because both are
/// process-wide values that every front end reads
/// ([`crate::ui::theme`], [`crate::ui::glyphs`]) and neither can be changed once
/// the first line has been painted.
///
/// The two settings have the same shape -- a flag, then a key in
/// `~/.caocli/settings.json`, then a default -- and they differ in what the
/// default is allowed to consult. The theme's `auto` asks the environment, which
/// is the only way a light terminal gets the light palette without being told;
/// the glyph set has no such question to ask, so its default is simply the
/// designed one.
///
/// Everything here is fallible on purpose: a name nothing answers to is a typo in
/// a file the user edits by hand, and silently painting the default is how they
/// never find out. The error names what would have worked.
pub fn install_visuals(cli: &Cli) -> Result<()> {
    install_glyphs(cli)?;
    install_theme(cli)
}

/// The glyph set: `--glyphs`, then the `glyphs` setting, then the designed one.
fn install_glyphs(cli: &Cli) -> Result<()> {
    let named = match &cli.glyphs {
        Some(value) => Some(value.clone()),
        None => setting_string("glyphs")?,
    };
    let set = match named.as_deref().map(str::trim) {
        None | Some("") | Some("unicode") => glyphs::UNICODE,
        Some("ascii") => glyphs::ASCII,
        Some(other) => bail!("unknown glyph set `{other}`: expected unicode or ascii"),
    };
    glyphs::install(set);
    Ok(())
}

/// The palette: `--theme`, then the `theme` setting, then `auto`.
///
/// `auto` is resolved from the cheapest source that has an answer: `COLORFGBG`,
/// which the terminal set when it started, and only then a query to the terminal
/// itself. The query is skipped entirely when one of the other two has already
/// decided, because it is a round trip in the middle of startup and a reader who
/// passed `--theme paper` is not a reader who wants to wait for a question whose
/// answer will be discarded.
fn install_theme(cli: &Cli) -> Result<()> {
    let named = match &cli.theme {
        Some(value) => Some(value.clone()),
        None => setting_string("theme")?,
    };
    let choice = match named.as_deref() {
        None => theme::Choice::Auto,
        Some(value) => theme::Choice::parse(value).ok_or_else(|| {
            anyhow::anyhow!("unknown theme `{value}`: expected {}", theme::theme_names())
        })?,
    };

    let term = RealTerminal;
    // The terminal owns the background, so the palette has to be calibrated to
    // it rather than to us: `COLORFGBG` is what it already told us, and the query
    // is asking it the same thing directly.
    let colorfgbg = std::env::var("COLORFGBG").ok();
    let queried = match (&choice, &colorfgbg) {
        (theme::Choice::Auto, None) => term.background().map(theme::theme_from_background),
        _ => None,
    };
    let name = theme::resolve(choice, colorfgbg.as_deref(), queried);
    let tier = theme::tier_from_env(|key| std::env::var(key).ok());
    theme::install(theme::Theme::new(name.palette(), tier, term.wants_color()));
    Ok(())
}

/// One top-level setting, when it is a string. A key that holds something else
/// is not a value this can read, and saying so would be worse than the error the
/// user gets from the thing they wrote: it is reported as no setting at all, and
/// the flag or the default answers.
fn setting_string(key: &str) -> Result<Option<String>> {
    Ok(config::setting(key)?.and_then(|value| value.as_str().map(str::to_owned)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("caocli").chain(args.iter().copied()))
    }

    /// A HOME of the test's own, with the config lock held, so that `/login`
    /// writes and key resolution reads under it do not race with another test.
    /// Returned second is the directory the test should remove when it is done.
    fn own_home() -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
        let guard = crate::config::env_lock();
        let home = crate::config::scratch_home();
        (guard, home)
    }

    /// A meta as a session file holds it, with the provider recorded.
    fn meta_of(provider: &str, model: &str) -> SessionMeta {
        SessionMeta {
            provider: Some(provider.into()),
            model: model.into(),
            reasoning_effort: None,
            instructions: None,
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
        // A resumed session runs where it ran before: the meta is what the
        // session is.
        let mut meta = meta_of("zai-coding-cn", "glm-5.3");
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::DEEPSEEK).unwrap());
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3");
    }

    #[test]
    fn explicit_model_in_the_same_provider_only_swaps_the_model() {
        let mut meta = meta_of("deepseek", "deepseek-flash");
        meta.reasoning_effort = Some("high".into());
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "deepseek/deepseek-v4-pro"]),
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
        // `--model zai-coding-cn/glm-5.3` moves the session from wherever it
        // was to the provider the name names.
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
    fn a_bare_model_id_is_rejected() {
        // A bare id is no longer accepted by `--model` or `/model`: without a
        // `--provider` to pin the implicit one, the user has to write the
        // provider into the spec.
        let mut meta = meta_of("deepseek", "deepseek-flash");
        let err = apply_overrides(
            &mut meta,
            &cli(&["--model", "deepseek-v4-pro"]),
            provider::DEEPSEEK,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("deepseek/deepseek-flash"), "{err}");
    }

    #[test]
    fn a_switch_brings_only_a_tier_the_new_provider_serves() {
        // A resumed session carries its tier into the provider it moves to only
        // where that provider serves it. MiniMax's only control is a thinking
        // switch, so DeepSeek's `max` is replaced by MiniMax's own default
        // rather than sent as a tier this backend answers with a 400.
        let mut meta = meta_of("deepseek", "deepseek-flash");
        meta.reasoning_effort = Some("max".into());
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "minimax/MiniMax-M3"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("minimax"));
        assert_eq!(meta.model, "MiniMax-M3");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("on"));
        // And back: `off` is not a DeepSeek tier either.
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "deepseek/deepseek-v4-pro"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.reasoning_effort.as_deref(), Some("max"));
        // A tier both providers serve is the session's to keep.
        let mut meta = meta_of("deepseek", "deepseek-flash");
        meta.reasoning_effort = Some("low".into());
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "zai-coding-cn/glm-5.3"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn a_session_from_before_providers_were_recorded_takes_this_runs_choice() {
        // No provider in the meta: the run's provider is what the model is sent
        // to, which is how every session behaved before the field existed.
        let mut meta = SessionMeta {
            provider: None,
            model: "deepseek-v4-pro".into(),
            reasoning_effort: None,
            instructions: None,
        };
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::ZAI_CODING_CN).unwrap());
        assert_eq!(meta.provider, None, "nothing is invented for it");
        assert_eq!(meta.model, "deepseek-v4-pro");
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
    fn fresh_meta_uses_start_provider_when_no_model_is_given() {
        // The run's `start` (the first provider with a stored key) is what
        // a brand-new session is on when no `--model` was passed.
        let meta = fresh_meta(&cli(&[]), provider::DEEPSEEK).unwrap();
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(meta.model, provider::DEEPSEEK.models[0]);
        assert_eq!(
            meta.reasoning_effort.as_deref(),
            Some(provider::DEEPSEEK.default_effort)
        );
    }

    #[test]
    fn fresh_meta_falls_back_to_whatever_start_passes_in() {
        // `resolve_startup` picks the first provider with a key as `start`;
        // `fresh_meta` only takes that as the fallback, whatever it is.
        let meta = fresh_meta(&cli(&[]), provider::ZAI_CODING_CN).unwrap();
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3-flash");
    }

    #[test]
    fn fresh_meta_carries_model_and_effort() {
        let meta = fresh_meta(
            &cli(&["--model", "deepseek/deepseek-v4-pro", "--effort", "max"]),
            provider::DEEPSEEK,
        )
        .unwrap();
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn first_available_provider_walks_to_the_first_with_a_key() {
        // No keys at all is not an error here: it is a state `resolve_startup`
        // decides what to do about, and the decision is tested below.
        let (guard, home) = own_home();
        assert!(first_available_provider().is_none());
        let hint = no_key_hint();
        assert!(hint.contains("/login"), "{hint}");
        assert!(hint.contains("deepseek"), "{hint}");
        assert!(hint.contains("zai-coding-cn"), "{hint}");
        assert!(hint.contains("minimax"), "{hint}");
        // DeepSeek has no key: the walk steps over it.
        crate::config::store_key("zai-coding-cn", "sk-test").unwrap();
        let p = first_available_provider().unwrap();
        assert_eq!(p.id, "zai-coding-cn");
        // Add DeepSeek on top: it wins because it is earlier in PROVIDERS.
        crate::config::store_key("deepseek", "sk-test").unwrap();
        let p = first_available_provider().unwrap();
        assert_eq!(p.id, "deepseek");
        drop(guard);
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// The first run on a machine with no key at all: the session has to start,
    /// because `/login` is a command of the session. It starts on the first
    /// provider, unfunded, and the banner is what says so.
    #[test]
    fn a_run_that_can_be_asked_starts_without_a_key_on_the_first_provider() {
        let (guard, home) = own_home();
        let startup = resolve_startup(&cli(&[])).unwrap();
        assert_eq!(startup.provider.id, provider::PROVIDERS[0].id);
        assert_eq!(
            startup.session.meta.provider.as_deref(),
            Some(provider::PROVIDERS[0].id)
        );
        assert_eq!(startup.session.meta.model, provider::PROVIDERS[0].models[0]);
        // The hint the banner carries is the one that names the command.
        let note = startup.api_key.missing_note().unwrap_or_default();
        assert!(note.contains("/login deepseek"), "{note}");
        drop(guard);
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// A run that cannot be asked fails where the failure belongs: before any
    /// session is built, with the command that would fix it.
    #[test]
    fn a_one_shot_run_with_no_key_fails_before_any_work() {
        let (guard, home) = own_home();
        let err = match resolve_startup(&cli(&["-p", "hello"])) {
            Ok(_) => panic!("a one-shot run with no key must not start"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no provider has a stored key"), "{err}");
        assert!(err.contains("/login deepseek"), "{err}");
        // The refusal comes before the session is created, so nothing was
        // written: a run with no key does not leave a session behind.
        assert_eq!(
            std::fs::read_dir(config::sessions_dir().unwrap())
                .unwrap()
                .count(),
            0
        );
        drop(guard);
        std::fs::remove_dir_all(&home).unwrap();
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
}
