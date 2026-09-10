use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// Backend provider presets. Deliberately a static table instead of a trait or
/// dynamic registration: adding a provider = adding one const line, which keeps
/// the "one backend, one loop" minimalism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// Id used by `--provider`.
    pub id: &'static str,
    /// Full chat/completions endpoint.
    pub url: &'static str,
    /// Default model when `--model` is not given.
    pub default_model: &'static str,
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
    /// API key environment variables, first non-empty one wins, in order.
    pub key_envs: &'static [&'static str],
}

pub const DEEPSEEK: Provider = Provider {
    id: "deepseek",
    url: "https://api.deepseek.com/chat/completions",
    default_model: "deepseek-flash",
    max_tokens: 384_000,
    key_envs: &["DEEPSEEK_API_KEY"],
};

/// Zhipu BigModel (GLM) coding endpoint: OpenAI-compatible plus DeepSeek-style
/// `thinking` / `reasoning_content`, so it reuses the same request and streaming
/// parsing.
pub const GLM: Provider = Provider {
    id: "glm",
    url: "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
    default_model: "GLM-5.3-Flash",
    max_tokens: 128_000,
    key_envs: &["ZAI_API_KEY", "GLM_API_KEY"],
};

pub const PROVIDERS: &[Provider] = &[DEEPSEEK, GLM];
/// Provider used when `--provider` is not given.
pub const DEFAULT_PROVIDER: &str = "deepseek";
/// Universal API key override, takes precedence over provider-specific variables.
const UNIVERSAL_KEY_ENV: &str = "CAOCLI_API_KEY";

/// Valid tiers for reasoning_effort. Both backends support only these three:
/// GLM's low produces no reasoning_content, DeepSeek's low still thinks.
/// Invalid values must be rejected locally — DeepSeek returns 400 and GLM
/// silently falls back to its default tier.
pub const EFFORTS: &[&str] = &["low", "high", "max"];

/// Tier used when `--effort` is not given. The backends' own defaults differ
/// (DeepSeek high, GLM max), so pinning it explicitly is the only way to make the
/// two behave the same.
pub const DEFAULT_EFFORT: &str = "max";

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

/// Resolve the API key for this provider: the universal variable first, then the
/// provider-specific variables in order.
pub fn api_key(provider: &Provider) -> Result<String> {
    let mut envs = vec![UNIVERSAL_KEY_ENV];
    envs.extend_from_slice(provider.key_envs);
    for env in envs {
        if let Ok(k) = std::env::var(env)
            && !k.trim().is_empty()
        {
            return Ok(k);
        }
    }
    bail!(
        "no API key set for provider {}: export {}=<key> (or set {} to override)",
        provider.id,
        provider.key_envs[0],
        UNIVERSAL_KEY_ENV
    )
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("cannot determine HOME directory")
}

pub fn caocli_dir() -> Result<PathBuf> {
    let dir = home_dir()?.join(".caocli");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create directory: {}", dir.display()))?;
    Ok(dir)
}

pub fn sessions_dir() -> Result<PathBuf> {
    let dir = caocli_dir()?.join("sessions");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create directory: {}", dir.display()))?;
    Ok(dir)
}

pub fn history_file() -> Result<PathBuf> {
    Ok(caocli_dir()?.join("history"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The environment is process-global, so parallel tests stomp on each other;
    /// serialize every case that reads or writes env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Returns a fresh, already-created temporary HOME.
    fn temp_home() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "caocli-config-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Clear every env var involved in key resolution so cases do not pollute
    /// each other.
    fn clear_key_envs() {
        for e in [
            UNIVERSAL_KEY_ENV,
            "DEEPSEEK_API_KEY",
            "ZAI_API_KEY",
            "GLM_API_KEY",
        ] {
            unsafe { std::env::remove_var(e) };
        }
    }

    #[test]
    fn provider_lookup_and_presets() {
        assert_eq!(provider("deepseek").unwrap(), DEEPSEEK);
        assert_eq!(provider("glm").unwrap(), GLM);
        assert!(provider("nope").unwrap_err().to_string().contains("glm"));
        assert_eq!(DEFAULT_PROVIDER, "deepseek");
        assert_eq!(DEEPSEEK.default_model, "deepseek-flash");
        assert_eq!(GLM.default_model, "GLM-5.3-Flash");
        // The two backends cap a single answer differently; both are ceilings
        // well above the defaults they replace.
        assert_eq!(DEEPSEEK.max_tokens, 384_000);
        assert_eq!(GLM.max_tokens, 128_000);
        assert!(GLM.url.contains("open.bigmodel.cn"));
    }

    #[test]
    fn api_key_reads_provider_env_var() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_key_envs();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-test-123") };
        assert_eq!(api_key(&DEEPSEEK).unwrap(), "sk-test-123");
        unsafe { std::env::set_var("ZAI_API_KEY", "zhipu-456") };
        assert_eq!(api_key(&GLM).unwrap(), "zhipu-456");
    }

    #[test]
    fn api_key_universal_env_wins() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_key_envs();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "specific") };
        unsafe { std::env::set_var(UNIVERSAL_KEY_ENV, "universal") };
        assert_eq!(api_key(&DEEPSEEK).unwrap(), "universal");
    }

    #[test]
    fn api_key_glm_falls_back_to_secondary_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_key_envs();
        unsafe { std::env::set_var("GLM_API_KEY", "glm-fallback") };
        assert_eq!(api_key(&GLM).unwrap(), "glm-fallback");
    }

    #[test]
    fn api_key_missing_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_key_envs();
        let err = api_key(&GLM).unwrap_err().to_string();
        assert!(err.contains("ZAI_API_KEY"), "err: {err}");
        assert!(err.contains("CAOCLI_API_KEY"), "err: {err}");
    }

    #[test]
    fn api_key_whitespace_only_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_key_envs();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "   \n\t ") };
        assert!(api_key(&DEEPSEEK).is_err());
    }

    #[test]
    fn dirs_created_under_home() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = temp_home();
        unsafe { std::env::set_var("HOME", &home) };

        let d = caocli_dir().unwrap();
        assert_eq!(d, home.join(".caocli"));
        assert!(d.is_dir());

        let s = sessions_dir().unwrap();
        assert_eq!(s, home.join(".caocli").join("sessions"));
        assert!(s.is_dir());

        assert_eq!(
            history_file().unwrap(),
            home.join(".caocli").join("history")
        );
        assert!(caocli_dir().unwrap().is_dir()); // idempotent
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn missing_home_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("HOME") };
        let err = home_dir().unwrap_err().to_string();
        assert!(err.contains("HOME"), "err: {err}");
    }
}
