//! The provider table: what a backend is, as data.
//!
//! Deliberately a static table instead of a trait or dynamic registration:
//! adding a provider = adding one const line, which keeps the "one backend, one
//! loop" minimalism. This module is pure — no IO, no environment — so everything
//! in it is testable without touching HOME or the filesystem. The things a
//! provider needs from the outside world (its API key, the menus that show key
//! state) live in `config`, which reads this table but is not read by it.

use anyhow::{Context, Result, bail};

/// Backend provider presets. Deliberately a static table instead of a trait or
/// dynamic registration: adding a provider = adding one const line, which keeps
/// the "one backend, one loop" minimalism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// Id a provider is addressed by: `--provider`, `/login <id>`, the
    /// `<id>/<modelid>` a model is named by, and the session meta. Never shown
    /// where a person is reading and choosing.
    pub id: &'static str,
    /// Name a person reads, and the only thing `/login` shows. English, like
    /// everything else here.
    pub name: &'static str,
    /// Full chat/completions endpoint.
    pub url: &'static str,
    /// Model ids this provider is known to serve, the first being the default
    /// when no model is given. This is the menu `/model` offers, not a
    /// whitelist: any id may be named explicitly, and an id the backend does not
    /// know is the backend's to reject.
    pub models: &'static [&'static str],
    /// Largest answer the backend will produce, sent as `max_tokens`. Omitting
    /// it leaves the backend's own default cap in force, which is far below what
    /// these models can emit — a long `Write` would be cut off mid-file and the
    /// next `Edit` would then fail to match. The cap is a ceiling, not a
    /// reservation: a short answer costs nothing extra.
    ///
    /// This bounds a single completion, not the conversation: both backends take
    /// 1M tokens of context and history is replayed whole (never trimmed), so
    /// there is nothing else here for a context window to do.
    pub max_tokens: u32,
    /// Whether the request carries the DeepSeek-style `thinking` switch. Both
    /// backends today take `{"type":"enabled"}`; a backend that rejects fields
    /// it does not know sets this to false, and nothing else changes.
    pub send_thinking: bool,
    /// Valid `reasoning_effort` tiers this backend accepts. The tiers are the
    /// backend's answer, not the program's, so they live in the preset: a
    /// provider with other tiers declares them here and nothing else changes.
    pub efforts: &'static [&'static str],
    /// Tier sent when no effort is stored. The backends' own defaults differ
    /// (DeepSeek high, GLM max), so the preset pins one explicitly — the only
    /// way to make them behave the same.
    pub default_effort: &'static str,
}

impl Provider {
    /// Model used when no model is named: `--model`, `/model`, or the session's
    /// own meta all override it.
    pub fn default_model(&self) -> &'static str {
        self.models[0]
    }

    /// Reject an effort tier this provider does not offer, before it is ever
    /// sent. DeepSeek returns 400 for an out-of-range value while GLM silently
    /// accepts it and degrades to its default tier — rejecting locally is the
    /// only way to keep the two consistent. Serves both `--effort` at startup
    /// and `/effort` mid-session, so the message names neither flag.
    pub fn validate_effort(&self, effort: &str) -> Result<()> {
        if self.efforts.contains(&effort) {
            return Ok(());
        }
        bail!(
            "invalid effort {effort:?} for {}; available: {}",
            self.name,
            self.efforts.join(" | ")
        )
    }
}

pub const DEEPSEEK: Provider = Provider {
    id: "deepseek",
    name: "DeepSeek",
    url: "https://api.deepseek.com/chat/completions",
    // What GET /models returns for this endpoint.
    models: &["deepseek-flash", "deepseek-v4-pro"],
    max_tokens: 384_000,
    send_thinking: true,
    // DeepSeek's low still emits reasoning_content, unlike GLM's.
    efforts: &["low", "high", "max"],
    default_effort: "max",
};

/// Z.AI's coding endpoint for mainland China: OpenAI-compatible plus
/// DeepSeek-style `thinking` / `reasoning_content`, so it reuses the same request
/// and streaming parsing.
pub const ZAI_CODING_CN: Provider = Provider {
    id: "zai-coding-cn",
    name: "Z.AI Coding CN",
    url: "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
    // The two models the coding plan serves on this endpoint, as the service
    // spells them.
    models: &["glm-5.3-flash", "glm-5.3"],
    max_tokens: 128_000,
    send_thinking: true,
    // GLM's low answers without emitting reasoning_content.
    efforts: &["low", "high", "max"],
    default_effort: "max",
};

pub const PROVIDERS: &[Provider] = &[DEEPSEEK, ZAI_CODING_CN];
/// Provider used when `--provider` is not given.
pub const DEFAULT_PROVIDER: &str = "deepseek";

/// Look up a provider by id.
pub fn provider(id: &str) -> Result<Provider> {
    PROVIDERS
        .iter()
        .copied()
        .find(|p| p.id == id)
        .with_context(|| {
            let ids = PROVIDERS
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown provider {id:?}; available: {ids}")
        })
}

/// A model choice: either the `<provider>/<modelid>` form the picker and the
/// status line use, or a bare id, which belongs to `current` — the provider in
/// effect — so `--model deepseek-v4-pro` keeps meaning what it always did.
pub fn model_spec(spec: &str, current: &str) -> Result<(Provider, String)> {
    let (id, model) = match spec.split_once('/') {
        Some((id, model)) => (id.trim(), model.trim()),
        None => (current, spec.trim()),
    };
    if model.is_empty() {
        bail!(
            "no model id in {spec:?}; a model is named <provider>/<modelid>, as in {}",
            example_model()
        );
    }
    Ok((provider(id)?, model.to_string()))
}

/// An example of the `<provider>/<modelid>` form, for error messages.
fn example_model() -> String {
    let p = provider(DEFAULT_PROVIDER).expect("the default provider is in the table");
    format!("{}/{}", p.id, p.default_model())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_lookup_and_presets() {
        assert_eq!(provider("deepseek").unwrap(), DEEPSEEK);
        assert_eq!(provider("zai-coding-cn").unwrap(), ZAI_CODING_CN);
        assert_eq!(DEEPSEEK.name, "DeepSeek");
        assert_eq!(ZAI_CODING_CN.name, "Z.AI Coding CN");
        assert!(provider("nope").unwrap_err().to_string().contains("zai"));
        assert_eq!(DEFAULT_PROVIDER, "deepseek");
        assert_eq!(DEEPSEEK.default_model(), "deepseek-flash");
        assert_eq!(ZAI_CODING_CN.default_model(), "glm-5.3-flash");
        // Every preset offers something to pick, and the first of them is what
        // `default_model` reads: an empty list would panic there.
        for p in PROVIDERS {
            assert!(!p.models.is_empty(), "{} offers no model", p.id);
            assert!(!p.id.contains('/'), "a provider id may not carry a slash");
        }
        // The two backends cap a single answer differently; both are ceilings
        // well above the defaults they replace.
        assert_eq!(DEEPSEEK.max_tokens, 384_000);
        assert_eq!(ZAI_CODING_CN.max_tokens, 128_000);
        assert!(ZAI_CODING_CN.url.contains("open.bigmodel.cn"));
    }

    #[test]
    fn every_preset_declares_a_usable_effort_profile() {
        for p in PROVIDERS {
            assert!(!p.efforts.is_empty(), "{} offers no effort tier", p.id);
            // The default must be one of the offered tiers, or a session that
            // never names an effort sends a value the backend rejects (or worse,
            // silently degrades from).
            assert!(
                p.efforts.contains(&p.default_effort),
                "{} defaults to a tier it does not offer",
                p.id
            );
        }
    }

    #[test]
    fn validate_effort_names_the_provider_and_its_tiers() {
        for ok in ["low", "high", "max"] {
            assert!(DEEPSEEK.validate_effort(ok).is_ok(), "{ok}");
        }
        for bad in ["none", "medium", "HIGH", "bogus", ""] {
            let err = DEEPSEEK.validate_effort(bad).unwrap_err().to_string();
            assert!(err.contains("low | high | max"), "{bad}: {err}");
            assert!(err.contains("DeepSeek"), "{bad}: {err}");
        }
    }

    #[test]
    fn model_spec_takes_a_qualified_id_and_its_own_provider() {
        let (p, m) = model_spec("zai-coding-cn/glm-5.3", "deepseek").unwrap();
        assert_eq!(p, ZAI_CODING_CN);
        assert_eq!(m, "glm-5.3");
        // Surrounding space is the shell's, not the model's.
        let (p, m) = model_spec(" deepseek/deepseek-v4-pro ", "zai-coding-cn").unwrap();
        assert_eq!(p, DEEPSEEK);
        assert_eq!(m, "deepseek-v4-pro");
    }

    #[test]
    fn model_spec_takes_a_bare_id_as_the_current_provider() {
        let (p, m) = model_spec("deepseek-v4-pro", "deepseek").unwrap();
        assert_eq!(p, DEEPSEEK);
        assert_eq!(m, "deepseek-v4-pro");
        // A bare id is whatever the current provider serves, even an id its own
        // menu does not list: the table is a menu, not a whitelist.
        let (p, m) = model_spec("something-new", "zai-coding-cn").unwrap();
        assert_eq!(p, ZAI_CODING_CN);
        assert_eq!(m, "something-new");
    }

    #[test]
    fn model_spec_rejects_an_empty_id_or_an_unknown_provider() {
        for bad in ["", "  ", "zai-coding-cn/", "zai-coding-cn/   "] {
            let err = model_spec(bad, "deepseek").unwrap_err().to_string();
            assert!(err.contains("deepseek/deepseek-flash"), "{bad}: {err}");
        }
        let err = model_spec("nope/whatever", "deepseek")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider"), "{err}");
    }
}
