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

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::agent::Approval;
use crate::api::Client;
use crate::cli::{Cli, Mode};
use crate::config::{self, ApiKey};
use crate::provider::{self, Provider};
use crate::session::{Session, SessionMeta, SessionSource};

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

    // The provider this run starts from: --provider, or the default.
    let start = provider::provider(
        cli.provider
            .as_deref()
            .unwrap_or(provider::DEFAULT_PROVIDER),
    )?;

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

/// Meta for a new session: the provider is the one this run selected, and the
/// model comes from `--model` — which may name a provider of its own as
/// `<provider>/<modelid>` — otherwise it is that provider's default. The
/// workspace's project instructions (its AGENTS.md files) are read here, once,
/// and frozen into the session: everything the session later sends is what it
/// stored, never what the files say by then.
fn fresh_meta(cli: &Cli, start: Provider) -> Result<SessionMeta> {
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
        instructions: crate::agents_md::load(),
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
    // The provider the session ends up on: the one `--model`/`--provider` chose
    // above, or the session's own when neither was given.
    let on = match meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => current,
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
