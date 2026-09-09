use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// 后端供应商预设。刻意用静态表而非 trait/动态注册：
/// 加一个供应商 = 加一行常量，保持“一个后端、一个循环”的极简。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// `--provider` 使用的 id。
    pub id: &'static str,
    /// 完整的 chat/completions 端点。
    pub url: &'static str,
    /// 未指定 `--model` 时的默认模型。
    pub default_model: &'static str,
    /// API key 环境变量，按顺序取第一个非空的。
    pub key_envs: &'static [&'static str],
}

pub const DEEPSEEK: Provider = Provider {
    id: "deepseek",
    url: "https://api.deepseek.com/chat/completions",
    default_model: "deepseek-v4.1-flash-expires-on-0910",
    key_envs: &["DEEPSEEK_API_KEY"],
};

/// 智谱 BigModel（GLM）coding 端点：OpenAI 兼容 + DeepSeek 风格的
/// `thinking` / `reasoning_content`，因此复用同一套请求与流式解析。
pub const GLM: Provider = Provider {
    id: "glm",
    url: "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
    default_model: "GLM-5.3-Flash",
    key_envs: &["ZAI_API_KEY", "GLM_API_KEY"],
};

pub const PROVIDERS: &[Provider] = &[DEEPSEEK, GLM];
/// 未指定 `--provider` 时的供应商。
pub const DEFAULT_PROVIDER: &str = "deepseek";
/// 通用 API key 覆盖，优先级高于供应商专用变量。
const UNIVERSAL_KEY_ENV: &str = "CAOCLI_API_KEY";

/// reasoning_effort 的合法档位。两家后端都只支持这三档：
/// GLM 的 low 不产出 reasoning_content，DeepSeek 的 low 仍会思考。
/// 非法值必须本地拒绝——DeepSeek 会 400，GLM 会静默按默认档处理。
pub const EFFORTS: &[&str] = &["low", "high", "max"];

/// 按 id 查供应商。
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
            format!("未知供应商 {id:?}；可用: {ids}")
        })
}

/// 取该供应商的 API key：先看通用变量，再按供应商专用变量顺序。
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
        "未设置供应商 {} 的 API key：请 export {}=<key>（或用 {} 覆盖）",
        provider.id,
        provider.key_envs[0],
        UNIVERSAL_KEY_ENV
    )
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("无法确定 HOME 目录")
}

pub fn caocli_dir() -> Result<PathBuf> {
    let dir = home_dir()?.join(".caocli");
    std::fs::create_dir_all(&dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    Ok(dir)
}

pub fn sessions_dir() -> Result<PathBuf> {
    let dir = caocli_dir()?.join("sessions");
    std::fs::create_dir_all(&dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    Ok(dir)
}

pub fn history_file() -> Result<PathBuf> {
    Ok(caocli_dir()?.join("history"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// env 是进程级全局，测试并行跑会互相踩；串行化所有读写 env 的用例。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 返回一个已创建的空临时 HOME。
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

    /// 清掉所有与 key 解析相关的环境变量，避免用例间互相污染。
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
        assert_eq!(GLM.default_model, "GLM-5.3-Flash");
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
        assert!(caocli_dir().unwrap().is_dir()); // 幂等
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
