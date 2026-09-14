//! The provider table: what a backend is, as data.
//!
//! Deliberately a static table instead of a trait or dynamic registration:
//! adding a provider = adding one const line, which keeps the "one backend, one
//! loop" minimalism. This module is pure — no IO, no environment — so everything
//! in it is testable without touching HOME or the filesystem. The things a
//! provider needs from the outside world (its API key, the menus that show key
//! state) live in `config`, which reads this table but is not read by it.

use anyhow::{Context, Result, bail};

/// The wire protocol a provider speaks. Three shapes are served today: the
/// OpenAI chat-completions body and SSE chunk stream, parsed here, the
/// Anthropic Messages body and event stream, spoken by the `anthropic` crate,
/// and the OpenAI Responses body and event stream, also spoken by the
/// `openai` crate — the same types the crate calls them by, since a Responses
/// answer is a list of items rather than a list of deltas. The internal
/// history is provider-agnostic; the request builder and the stream each branch
/// on this, and all three wires arrive at the same deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    OpenAi,
    Anthropic,
    Responses,
}

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
    /// Full endpoint of the resource the wire is spoken at — `/chat/completions`
    /// or `/responses` on the OpenAI shapes, `/messages` on the Anthropic one.
    /// The client sends to it verbatim, and the trace records it verbatim.
    pub url: &'static str,
    /// The wire protocol the endpoint speaks (see [`Wire`]).
    pub wire: Wire,
    /// Model ids this provider is known to serve, the first being the default
    /// when no model is given. This is the menu `/model` offers, not a
    /// whitelist: any id may be named explicitly, and an id the backend does not
    /// know is the backend's to reject.
    pub models: &'static [&'static str],
    /// Largest answer the backend will produce, sent as `max_tokens` on the
    /// OpenAI chat wire and `max_output_tokens` on the Responses one — on the
    /// latter, a cap that includes the reasoning tokens. Omitting it leaves the
    /// backend's own default cap in force, which is far below what these models
    /// can emit — a long `Write` would be cut off mid-file and the next `Edit`
    /// would then fail to match. The cap is a ceiling, not a reservation: a
    /// short answer costs nothing extra.
    ///
    /// This bounds a single completion, not the conversation: the backends take
    /// 1M tokens of context and history is replayed whole (never trimmed), so
    /// there is nothing else here for a context window to do.
    pub max_tokens: u32,
    /// Whether the request carries the DeepSeek-style `thinking` switch (an
    /// OpenAi chat-wire field; the other two wires drive thinking through their
    /// effort tiers instead, see `default_effort`). Both chat-wire backends
    /// take `{"type":"enabled"}`; a backend that rejects fields it does not
    /// know sets this to false, and nothing else changes.
    pub send_thinking: bool,
    /// Valid `reasoning_effort` tiers this backend accepts. The tiers are the
    /// backend's answer, not the program's, so they live in the preset: a
    /// provider with other tiers declares them here and nothing else changes.
    /// On the Responses wire the tier is sent as `reasoning.effort`, which is
    /// where that wire keeps the same knob.
    pub efforts: &'static [&'static str],
    /// Tier sent when no effort is stored. The backends' own defaults differ
    /// (DeepSeek high, GLM max, MiniMax thinking on), so the preset pins one
    /// explicitly — the only way to make them behave the same. On the
    /// Anthropic wire the tier is a thinking switch (see [`AnthropicOptions`])
    /// unless the preset says the endpoint serves the standard effort field.
    pub default_effort: &'static str,
    /// Which of the standard Anthropic optional fields this preset's endpoint
    /// serves. Meaningless on the OpenAI wire, where they do not exist.
    pub anthropic: AnthropicOptions,
}

/// The standard optional fields of the Anthropic wire, as the endpoint's
/// answers rather than the program's.
///
/// Each is a field the spec describes and a spec-conformant endpoint serves;
/// none is required, and one a backend rejects is a 400 on *every* request.
/// That is the same bargain `send_thinking` struck on the OpenAI wire: the
/// preset carries the answer, the request builder obeys, and nothing else
/// changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicOptions {
    /// Read this preset's effort tiers as [`output_config.effort`] — the
    /// standard field for how hard the model works — instead of as a thinking
    /// switch. A tier the spec has no word for is left unsent rather than
    /// guessed at, so a preset that serves its own tiers keeps thinking on and
    /// sends no effort.
    ///
    /// [`output_config.effort`]: https://platform.claude.com/en/api/messages
    pub effort: bool,
    /// Ask for the reasoning text itself (`thinking.display: "summarized"`).
    /// Worth setting on every endpoint that takes it: the field defaults to
    /// `omitted` on the newest models, where the answer carries a signature and
    /// no words, and a front end that shows reasoning then has nothing to show.
    pub display: bool,
    /// Turn the prompt cache on with the automatic breakpoint. One field, and
    /// the server keeps the breakpoint at the end of the cacheable prefix and
    /// moves it forward as the conversation grows.
    pub cache_control: bool,
}

impl AnthropicOptions {
    /// None of them: the shape the spec requires and nothing beyond it.
    ///
    /// The starting point for an endpoint nobody has asked yet — a request that
    /// carries an unknown field is a request some gateways refuse, and every
    /// one of these can be turned on the moment a request says it is taken.
    pub const NONE: Self = Self {
        effort: false,
        display: false,
        cache_control: false,
    };
}

impl Provider {
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

    /// Whether this provider offers `effort`.
    pub fn offers_effort(&self, effort: &str) -> bool {
        self.efforts.contains(&effort)
    }

    /// The tier to carry into this provider: the one the session had when this
    /// provider offers it, this provider's own default when it does not.
    ///
    /// A switch can move a session to another provider, whose tiers are its own
    /// — MiniMax's thinking switch against DeepSeek's low/high/max, say. Sent,
    /// the old tier is one backend's 400 and another's silent misreading;
    /// shown, it names a tier the session is not running on. A session that
    /// stored no tier keeps storing none, so the provider's default stays the
    /// fallback it always was.
    pub fn fit_effort(&self, effort: Option<&str>) -> Option<String> {
        match effort {
            Some(tier) if !self.offers_effort(tier) => Some(self.default_effort.to_string()),
            other => other.map(str::to_owned),
        }
    }
}

pub const DEEPSEEK: Provider = Provider {
    id: "deepseek",
    name: "DeepSeek",
    url: "https://api.deepseek.com/chat/completions",
    wire: Wire::OpenAi,
    // What GET /models returns for this endpoint.
    models: &["deepseek-flash", "deepseek-v4-pro"],
    max_tokens: 384_000,
    send_thinking: true,
    // DeepSeek's low still emits reasoning_content, unlike GLM's.
    efforts: &["low", "high", "max"],
    default_effort: "max",
    // The OpenAI wire has none of these fields.
    anthropic: AnthropicOptions::NONE,
};

/// Z.AI's coding endpoint for mainland China: OpenAI-compatible plus
/// DeepSeek-style `thinking` / `reasoning_content`, so it reuses the same request
/// and streaming parsing.
pub const ZAI_CODING_CN: Provider = Provider {
    id: "zai-coding-cn",
    name: "Z.AI Coding CN",
    url: "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
    wire: Wire::OpenAi,
    // The two models the coding plan serves on this endpoint, as the service
    // spells them.
    models: &["glm-5.3-flash", "glm-5.3"],
    max_tokens: 128_000,
    send_thinking: true,
    // GLM's low answers without emitting reasoning_content.
    efforts: &["low", "high", "max"],
    default_effort: "max",
    // The OpenAI wire has none of these fields.
    anthropic: AnthropicOptions::NONE,
};

/// MiniMax's Anthropic-compatible Messages endpoint (mainland-China host, the
/// platform at platform.minimaxi.com). `MiniMax-M3` is an agentic coding
/// model with a 1M-token context; it has no effort tiers, only a thinking
/// switch (`adaptive` / `disabled`), so the effort slots carry `on` / `off`
/// and the Anthropic request builder maps them onto that switch.
pub const MINIMAX: Provider = Provider {
    id: "minimax",
    name: "MiniMax",
    url: "https://api.minimax.cn/anthropic/v1/messages",
    wire: Wire::Anthropic,
    models: &["MiniMax-M3"],
    // The recommended value for M3 (the ceiling the endpoint allows is
    // 512 Ki tokens); a ceiling, not a reservation.
    max_tokens: 131_072,
    // The thinking switch on this wire is the effort tier's job.
    send_thinking: false,
    efforts: &["on", "off"],
    default_effort: "on",
    // Verified against this endpoint with `scripts/anthropic_probe.py`: it takes
    // all three, and the two that are on are on because their *effect* was
    // measured, not because the request was accepted — a repeated long prefix
    // came back with the second request's prompt read from the cache, and the
    // reasoning text streamed as deltas.
    anthropic: AnthropicOptions {
        // Off, and not because the endpoint refuses it: this preset's tiers are
        // a thinking switch (`on`/`off`), not the standard levels, so there is
        // no effort to send. Saying `true` here would turn `/effort off` from
        // "do not think" into "think", which is the opposite of what the tier
        // means.
        effort: false,
        display: true,
        cache_control: true,
    },
};

/// Xiaomi's Responses endpoint. `mimo-v2.5-pro` is the flagship reasoning
/// model and `mimo-v2.5` the omni-modal one; both take a 1M-token context and
/// emit a full chain of thought, which this wire streams as reasoning items
/// and hands back on the next request (see `agent::request::responses_request`).
pub const MIMO: Provider = Provider {
    id: "mimo",
    name: "MiMo",
    url: "https://api.xiaomimimo.com/v1/responses",
    wire: Wire::Responses,
    models: &["mimo-v2.5-pro", "mimo-v2.5"],
    // Both models are documented at a 128 Ki-token maximum output, on a shared
    // range of [1, 131_072] — the same ceiling the Responses wire's
    // `max_output_tokens` takes.
    max_tokens: 131_072,
    // The thinking switch on this wire is the `reasoning.effort` tier's job:
    // `none` turns thinking off, and the tiers above it turn it on.
    send_thinking: false,
    // Every value the endpoint accepts. It documents the enabled tiers as
    // indistinguishable today, so they are offered as the backend spells them
    // rather than collapsed; `none` is the one that means anything different.
    efforts: &["none", "low", "medium", "high"],
    // Thinking on, which is what the models do when the field is absent — the
    // same choice every other preset makes for its own tiers.
    default_effort: "high",
    // The Anthropic wire has none of these fields.
    anthropic: AnthropicOptions::NONE,
};

pub const PROVIDERS: &[Provider] = &[DEEPSEEK, ZAI_CODING_CN, MINIMAX, MIMO];
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

/// Resolve a model spec into a (provider, model id) pair. The spec must be
/// `<provider>/<modelid>` — the same shape `/model`, `--model` and the status
/// line all use. A bare id is not accepted: without a `--provider` flag to pin
/// the implicit one, it would just be a guess.
pub fn model_spec(spec: &str) -> Result<(Provider, String)> {
    let (id, model) = spec.split_once('/').ok_or_else(|| {
        anyhow::anyhow!(
            "a model is named <provider>/<modelid>, as in {example}; got {spec:?}",
            example = example_model()
        )
    })?;
    let (id, model) = (id.trim(), model.trim());
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
    format!("{}/{}", p.id, p.models[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_lookup_and_presets() {
        assert_eq!(provider("deepseek").unwrap(), DEEPSEEK);
        assert_eq!(provider("zai-coding-cn").unwrap(), ZAI_CODING_CN);
        assert_eq!(provider("minimax").unwrap(), MINIMAX);
        assert_eq!(provider("mimo").unwrap(), MIMO);
        assert_eq!(DEEPSEEK.name, "DeepSeek");
        assert_eq!(ZAI_CODING_CN.name, "Z.AI Coding CN");
        assert_eq!(MINIMAX.name, "MiniMax");
        assert_eq!(MIMO.name, "MiMo");
        assert!(provider("nope").unwrap_err().to_string().contains("zai"));
        assert_eq!(DEFAULT_PROVIDER, "deepseek");
        assert_eq!(DEEPSEEK.models[0], "deepseek-flash");
        assert_eq!(ZAI_CODING_CN.models[0], "glm-5.3-flash");
        assert_eq!(MINIMAX.models[0], "MiniMax-M3");
        assert_eq!(MIMO.models[0], "mimo-v2.5-pro");
        // Every preset offers something to pick, and `models[0]` is what the
        // startup fallback reads: an empty list would panic there.
        for p in PROVIDERS {
            assert!(!p.models.is_empty(), "{} offers no model", p.id);
            assert!(!p.id.contains('/'), "a provider id may not carry a slash");
        }
        // The four backends cap a single answer differently; all are ceilings
        // well above the defaults they replace.
        assert_eq!(DEEPSEEK.max_tokens, 384_000);
        assert_eq!(ZAI_CODING_CN.max_tokens, 128_000);
        assert_eq!(MINIMAX.max_tokens, 131_072);
        assert_eq!(MIMO.max_tokens, 131_072);
        assert!(ZAI_CODING_CN.url.contains("open.bigmodel.cn"));
        // The wire split: the first two speak the OpenAI chat shape, MiniMax
        // the Anthropic one, MiMo the OpenAI Responses one — each preset naming
        // the endpoint the wire is spoken at.
        assert_eq!(DEEPSEEK.wire, Wire::OpenAi);
        assert_eq!(ZAI_CODING_CN.wire, Wire::OpenAi);
        assert_eq!(MINIMAX.wire, Wire::Anthropic);
        assert_eq!(MIMO.wire, Wire::Responses);
        assert!(MINIMAX.url.contains("/anthropic/v1/messages"));
        assert!(MIMO.url.ends_with("/v1/responses"));
        // The Anthropic wire carries no reasoning_effort: its effort slots are
        // the thinking switch, and `send_thinking` (a chat-wire field) is out of
        // service on both other wires too — on the Responses wire the tier is
        // `reasoning.effort`, which is the same knob by another name.
        assert_eq!(MINIMAX.efforts, &["on", "off"]);
        assert_eq!(MINIMAX.default_effort, "on");
        assert_eq!(MIMO.efforts, &["none", "low", "medium", "high"]);
        assert_eq!(MIMO.default_effort, "high");
        // Through the lookup, so the assertion reads a runtime value rather
        // than folding a const away.
        assert!(!provider("minimax").unwrap().send_thinking);
        assert!(!provider("mimo").unwrap().send_thinking);
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
    fn fit_effort_keeps_an_offered_tier_and_falls_back_to_the_default() {
        // A tier the provider offers is the session's to keep: a switch from
        // GLM to DeepSeek keeps `max`, which both serve.
        assert_eq!(DEEPSEEK.fit_effort(Some("max")).as_deref(), Some("max"));
        assert_eq!(
            ZAI_CODING_CN.fit_effort(Some("low")).as_deref(),
            Some("low")
        );
        assert_eq!(MINIMAX.fit_effort(Some("off")).as_deref(), Some("off"));
        // MiMo spells its own tiers, one of which is shared with DeepSeek's
        // list — `low` means the same word on both, so a switch keeps it.
        assert_eq!(MIMO.fit_effort(Some("low")).as_deref(), Some("low"));
        assert_eq!(MIMO.fit_effort(Some("none")).as_deref(), Some("none"));
        // One it does not offer is replaced by its own default: MiniMax has a
        // thinking switch, not DeepSeek's tiers, and DeepSeek would answer a
        // request carrying `on` with a 400.
        assert_eq!(MINIMAX.fit_effort(Some("max")).as_deref(), Some("on"));
        assert_eq!(DEEPSEEK.fit_effort(Some("on")).as_deref(), Some("max"));
        assert_eq!(MIMO.fit_effort(Some("max")).as_deref(), Some("high"));
        // A session that stored no tier keeps storing none: the provider's
        // default is already what the request falls back to.
        assert_eq!(DEEPSEEK.fit_effort(None), None);
        assert_eq!(MINIMAX.fit_effort(None), None);
        assert_eq!(MIMO.fit_effort(None), None);
    }

    #[test]
    fn model_spec_takes_a_qualified_id_and_its_own_provider() {
        let (p, m) = model_spec("zai-coding-cn/glm-5.3").unwrap();
        assert_eq!(p, ZAI_CODING_CN);
        assert_eq!(m, "glm-5.3");
        // Surrounding space is the shell's, not the model's.
        let (p, m) = model_spec(" deepseek/deepseek-v4-pro ").unwrap();
        assert_eq!(p, DEEPSEEK);
        assert_eq!(m, "deepseek-v4-pro");
    }

    #[test]
    fn model_spec_rejects_a_bare_id_without_a_provider() {
        // A bare id is no longer accepted: there is no implicit provider to
        // belong to, so the user has to write `<provider>/<modelid>`.
        for bad in ["deepseek-v4-pro", "glm-5.3", "MiniMax-M3"] {
            let err = model_spec(bad).unwrap_err().to_string();
            assert!(err.contains("deepseek/deepseek-flash"), "{bad}: {err}");
        }
    }

    #[test]
    fn model_spec_rejects_an_empty_id_or_an_unknown_provider() {
        for bad in ["", "  ", "zai-coding-cn/", "zai-coding-cn/   "] {
            let err = model_spec(bad).unwrap_err().to_string();
            assert!(err.contains("deepseek/deepseek-flash"), "{bad}: {err}");
        }
        let err = model_spec("nope/whatever").unwrap_err().to_string();
        assert!(err.contains("unknown provider"), "{err}");
    }
}
