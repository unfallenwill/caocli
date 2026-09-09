use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub const BASE_URL: &str = "https://api.deepseek.com/chat/completions";
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

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
