use anyhow::{Context, Result, bail};
use std::path::PathBuf;

pub const BASE_URL: &str = "https://api.deepseek.com/chat/completions";
pub const DEFAULT_MODEL: &str = "deepseek-v4.1-flash-expires-on-0910";

pub fn api_key() -> Result<String> {
    let key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        bail!("环境变量 DEEPSEEK_API_KEY 未设置。请先: export DEEPSEEK_API_KEY=<your-key>");
    }
    Ok(key)
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

    #[test]
    fn api_key_reads_env_var() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-test-123") };
        assert_eq!(api_key().unwrap(), "sk-test-123");
    }

    #[test]
    fn api_key_missing_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        let err = api_key().unwrap_err().to_string();
        assert!(err.contains("DEEPSEEK_API_KEY"), "err: {err}");
    }

    #[test]
    fn api_key_whitespace_only_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "   \n\t ") };
        assert!(api_key().is_err());
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
