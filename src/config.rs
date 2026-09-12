use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::provider::{PROVIDERS, Provider};

/// One row of a menu: what it shows, what choosing it means, and what it says
/// about itself. The two front ends render these their own way — a picker row, a
/// line of text — but neither invents its own list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What the row shows: a provider's name, or `<provider id>/<modelid>`.
    pub label: String,
    /// What choosing it submits, as the argument of the command the menu belongs
    /// to: a provider's id, or a model's `<provider id>/<modelid>`. The same
    /// string as the label where the row *is* what is being chosen; a provider is
    /// listed by name and addressed by id.
    pub argument: String,
    /// The dim second column: where the row stands, in a word or two.
    pub detail: String,
}

impl Choice {
    fn new(
        label: impl Into<String>,
        argument: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            argument: argument.into(),
            detail: detail.into(),
        }
    }
}

/// The providers `/login` can store a key for, each named and each with the
/// state of its key.
pub fn provider_choices() -> Vec<Choice> {
    PROVIDERS
        .iter()
        .map(|p| {
            let detail = if has_key(p) { "key stored" } else { "no key" };
            Choice::new(p.name, p.id, detail)
        })
        .collect()
}

/// Every model the preset table offers, named `<provider id>/<modelid>` — which
/// is what the id is for — with the current one and any that has no key to send
/// marked.
pub fn model_menu(current: &str) -> Vec<Choice> {
    let mut rows = Vec::new();
    for p in PROVIDERS {
        for model in p.models {
            let id = format!("{}/{}", p.id, model);
            let detail = match (id == current, has_key(p)) {
                (true, _) => "current".to_string(),
                (false, false) => format!("no key: /login {}", p.id),
                (false, true) if *model == p.default_model() => "default".to_string(),
                _ => String::new(),
            };
            rows.push(Choice::new(id.clone(), id, detail));
        }
    }
    rows
}

/// The reasoning effort tiers the provider in use accepts, the one in effect
/// marked. Built from the provider's own list, so a provider that offers other
/// tiers offers those, and nothing here needs a second copy of them.
pub fn effort_menu(provider: &Provider, current: &str) -> Vec<Choice> {
    provider
        .efforts
        .iter()
        .map(|tier| {
            // The tier in effect wins the column when it is also the default,
            // exactly as the model menu's current model does.
            let detail = if *tier == current {
                "current"
            } else if *tier == provider.default_effort {
                "default"
            } else {
                ""
            };
            Choice::new(*tier, *tier, detail)
        })
        .collect()
}

/// Whether a provider has a key behind it, for the rows of a menu. A settings
/// file that cannot be read counts as none: it is reported where a key is
/// actually asked for, and a menu is not the place to fail.
pub fn has_key(provider: &Provider) -> bool {
    matches!(stored_key(provider.id), Ok(Some(_)))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("cannot determine HOME directory")
}

/// `~/.caocli`, where the settings, the history and the sessions live.
///
/// The path rather than the directory: reading a setting is not a reason to make
/// one, and a read that made it would leave a `~/.caocli` behind on a machine
/// whose user has never stored anything -- and a directory appearing under
/// whichever `HOME` happened to be current is a write nothing asked for. What
/// writes there makes it: [`store_key`], the history when it is saved, and
/// [`sessions_dir`].
pub fn caocli_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".caocli"))
}

/// `~/.caocli/sessions`, made here rather than by each writer: the sessions are
/// what this directory is for, and a session is written into it on the way in.
pub fn sessions_dir() -> Result<PathBuf> {
    let dir = caocli_dir()?.join("sessions");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create directory: {}", dir.display()))?;
    Ok(dir)
}

pub fn history_file() -> Result<PathBuf> {
    Ok(caocli_dir()?.join("history"))
}

// ------------------------------------------------------------- settings ---

/// `~/.caocli/settings.json`: what the user settled with a command rather than
/// with a flag. Today that is the API key of each provider `/login` was run
/// for; the file is theirs to edit as well, and `/login` rewrites it whole
/// rather than in place, so a hand-written setting it does not know about
/// survives.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    providers: BTreeMap<String, ProviderSettings>,
    /// Whatever else the file holds, kept exactly as it was read: `/login`
    /// rewrites this file whole, and a setting it does not know about is not
    /// its to drop.
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProviderSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

pub fn settings_file() -> Result<PathBuf> {
    Ok(caocli_dir()?.join("settings.json"))
}

/// The settings as they are on disk. A file that is not there is the empty
/// settings; a file that cannot be understood is an error, not an empty one --
/// otherwise a key the user stored would silently stop being used.
fn load_settings() -> Result<Settings> {
    let path = settings_file()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// The key `/login` stored for a provider, if there is one.
pub fn stored_key(provider_id: &str) -> Result<Option<String>> {
    Ok(load_settings()?
        .providers
        .get(provider_id)
        .and_then(|p| p.api_key.clone())
        .filter(|k| !k.trim().is_empty()))
}

/// One top-level setting as the file writes it, for the parts of the
/// application whose settings are their own.
///
/// The settings file is read in one place and parsed in one place; what a
/// section of it means is the reader's business, and `mcpServers` is one whose
/// reader is the MCP client.
pub fn setting(name: &str) -> Result<Option<serde_json::Value>> {
    Ok(load_settings()?.rest.get(name).cloned())
}

/// Remember a provider's key, keeping every other setting in the file.
pub fn store_key(provider_id: &str, key: &str) -> Result<PathBuf> {
    let mut settings = load_settings()?;
    settings
        .providers
        .entry(provider_id.to_string())
        .or_default()
        .api_key = Some(key.to_string());
    let path = settings_file()?;
    // The directory is made here, by the write that needs it, and not by the read
    // above: a key is the first thing many users store, and the file it goes in
    // may be the first thing in a home that has no `~/.caocli` yet.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create directory: {}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&settings)? + "\n";
    // Written beside the file and renamed over it, so a crash leaves either the
    // old key or the new one and never half of either.
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, text.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// A file only its owner can read: it holds an API key.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Resolve the API key for this provider: the one `/login` stored in
/// `settings.json`. There is no second place for one to come from — a key in an
/// environment variable was one, and a key that two mechanisms hold is a key
/// whose state nobody can see.
pub fn api_key(provider: &Provider) -> Result<String> {
    match stored_key(provider.id)? {
        Some(key) => Ok(key),
        None => bail!(
            "no API key for {}: run /login {}",
            provider.name,
            provider.id
        ),
    }
}

/// Serializes the tests that read or write the process environment or HOME.
/// There is one environment for the whole test binary, so there is one lock for
/// the whole crate rather than one per test module.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take it, ignoring poisoning: a test that fails while holding it must not turn
/// the ones behind it into failures of their own.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A HOME of its own, so that a key a test finds under it is one the test itself
/// put there. The caller holds [`env_lock`] while it uses it.
#[cfg(test)]
pub(crate) fn scratch_home() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let home = std::env::temp_dir().join(format!(
        "caocli-test-home-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    // The config directory with it, because a test that writes a settings file
    // writes it where the program would look for one, and what reads there no
    // longer makes the directory on its way past.
    std::fs::create_dir_all(home.join(".caocli")).unwrap();
    unsafe { std::env::set_var("HOME", &home) };
    home
}

/// Move the process into a working directory of its own, for the tests that
/// read where the process stands (the workspace a fresh session reads its
/// instructions from is the cwd). The cwd is one directory for the whole test
/// binary, like the environment, so the caller holds [`env_lock`] while it
/// uses it and moves the process back before it returns.
#[cfg(test)]
pub(crate) fn scratch_cwd() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "caocli-test-cwd-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_current_dir(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{DEEPSEEK, ZAI_CODING_CN};

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

    /// A scratch HOME with every key variable cleared. See [`scratch_home`].
    fn bare_home() -> PathBuf {
        scratch_home()
    }

    #[test]
    fn model_menu_names_every_model_by_its_provider() {
        let _g = env_lock();
        let home = bare_home();
        store_key("deepseek", "sk-test").unwrap();
        let rows = model_menu("deepseek/deepseek-v4-pro");
        let names: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "deepseek/deepseek-flash",
                "deepseek/deepseek-v4-pro",
                "zai-coding-cn/glm-5.3-flash",
                "zai-coding-cn/glm-5.3",
                "minimax/MiniMax-M3",
            ]
        );
        // The current one says so, a provider's default says so, and the
        // models with no key behind them say where to get one.
        assert_eq!(rows[1].detail, "current");
        assert_eq!(rows[0].detail, "default");
        assert_eq!(rows[2].detail, "no key: /login zai-coding-cn");
        assert_eq!(rows[3].detail, "no key: /login zai-coding-cn");
        assert_eq!(rows[4].detail, "no key: /login minimax");
        // A model is chosen by the name it is shown by.
        assert_eq!(rows[1].argument, rows[1].label);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn effort_menu_offers_the_providers_tiers_and_marks_the_current_one() {
        let rows = effort_menu(&DEEPSEEK, "high");
        let tiers: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(tiers, vec!["low", "high", "max"], "the provider's own list");
        assert_eq!(rows[1].detail, "current");
        assert_eq!(rows[2].detail, "default");
        assert_eq!(rows[0].detail, "", "a plain tier says nothing extra");
        // A tier is chosen by the name it is shown by.
        assert_eq!(rows[0].argument, rows[0].label);

        // With the default in effect, the default row is the current one, as it
        // is in the model menu.
        let rows = effort_menu(&DEEPSEEK, DEEPSEEK.default_effort);
        assert_eq!(rows[2].detail, "current");
        // The menu is the provider's: another provider's tiers are its own list.
        let rows = effort_menu(&ZAI_CODING_CN, "low");
        assert_eq!(rows[0].detail, "current");
        assert_eq!(rows.len(), ZAI_CODING_CN.efforts.len());
    }

    #[test]
    fn provider_choices_show_the_name_and_submit_the_id() {
        let _g = env_lock();
        let home = bare_home();
        let rows = provider_choices();
        assert_eq!(rows[0].label, "DeepSeek");
        assert_eq!(rows[0].argument, "deepseek", "listed by name, chosen by id");
        assert_eq!(rows[0].detail, "no key");
        assert_eq!(rows[1].label, "Z.AI Coding CN");
        assert_eq!(rows[1].argument, "zai-coding-cn");
        assert_eq!(rows[1].detail, "no key");

        store_key("deepseek", "sk-stored").unwrap();
        let rows = provider_choices();
        assert_eq!(rows[0].detail, "key stored");
        assert_eq!(rows[1].detail, "no key", "the other one is untouched");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn model_menu_marks_a_model_with_no_key_behind_it() {
        let _g = env_lock();
        let home = bare_home();
        store_key("deepseek", "sk-test").unwrap();
        let rows = model_menu("deepseek/deepseek-flash");
        // The keyless provider's rows say how to get a key, not merely that
        // there is none; the funded one does not mention keys at all.
        assert_eq!(rows[2].detail, "no key: /login zai-coding-cn");
        assert_eq!(rows[3].detail, "no key: /login zai-coding-cn");
        let funded: Vec<&str> = rows
            .iter()
            .filter(|row| row.label.starts_with("deepseek/"))
            .map(|row| row.detail.as_str())
            .collect();
        assert!(!funded.iter().any(|d| d.contains("no key")), "{funded:?}");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn api_key_comes_from_the_file_and_nowhere_else() {
        let _g = env_lock();
        let home = bare_home();
        // The file `/login` writes is the one place a key lives. A bare HOME has
        // none, and no provider pretends otherwise: each says what to run.
        for provider in PROVIDERS {
            let err = api_key(provider).unwrap_err().to_string();
            assert!(err.contains("/login"), "err: {err}");
        }
        assert!(!has_key(&DEEPSEEK));

        store_key("deepseek", "sk-stored").unwrap();
        assert_eq!(api_key(&DEEPSEEK).unwrap(), "sk-stored");
        assert!(has_key(&DEEPSEEK));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn api_key_missing_says_what_to_do_about_it() {
        let _g = env_lock();
        let home = bare_home();
        let err = api_key(&ZAI_CODING_CN).unwrap_err().to_string();
        assert!(err.contains("/login zai-coding-cn"), "err: {err}");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_whitespace_only_key_is_no_key() {
        let _g = env_lock();
        let home = bare_home();
        std::fs::create_dir_all(home.join(".caocli")).unwrap();
        std::fs::write(
            settings_file().unwrap(),
            r#"{"providers":{"deepseek":{"api_key":"   \n\t "}}}"#,
        )
        .unwrap();
        assert_eq!(stored_key("deepseek").unwrap(), None);
        assert!(api_key(&DEEPSEEK).is_err());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_stored_key_is_the_one_that_is_used() {
        let _g = env_lock();
        let home = bare_home();
        let path = store_key("deepseek", "from-file").unwrap();
        assert_eq!(path, home.join(".caocli").join("settings.json"));
        assert_eq!(
            stored_key("deepseek").unwrap().as_deref(),
            Some("from-file")
        );
        assert_eq!(api_key(&DEEPSEEK).unwrap(), "from-file");
        // The other provider is untouched by it.
        assert_eq!(stored_key("zai-coding-cn").unwrap(), None);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn storing_a_key_keeps_the_rest_of_the_file() {
        let _g = env_lock();
        let home = bare_home();
        store_key("deepseek", "first").unwrap();
        // Something the user wrote by hand, which `/login` does not know about.
        let path = settings_file().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["future_setting"] = serde_json::json!({"kept": true});
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        store_key("zai-coding-cn", "second").unwrap();
        assert_eq!(api_key(&ZAI_CODING_CN).unwrap(), "second");
        assert_eq!(api_key(&DEEPSEEK).unwrap(), "first", "not overwritten");
        let text = std::fs::read_to_string(settings_file().unwrap()).unwrap();
        assert!(text.contains("future_setting"), "{text}");
        assert!(text.ends_with('\n'), "a text file ends with a newline");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn the_settings_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let _g = env_lock();
        let home = bare_home();
        let path = store_key("deepseek", "secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        // The file it was renamed from is gone: a leftover would be a stray
        // second copy of the key.
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_settings_file_that_cannot_be_parsed_is_an_error() {
        let _g = env_lock();
        let home = bare_home();
        std::fs::create_dir_all(home.join(".caocli")).unwrap();
        std::fs::write(settings_file().unwrap(), "{ not json").unwrap();
        // Reported rather than skipped: a key the user stored would otherwise
        // quietly stop being used.
        let err = api_key(&DEEPSEEK).unwrap_err().to_string();
        assert!(err.contains("settings.json"), "err: {err}");
        assert!(store_key("deepseek", "x").is_err());
        assert!(!has_key(&DEEPSEEK));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_missing_settings_file_is_the_empty_settings() {
        let _g = env_lock();
        let home = bare_home();
        assert_eq!(stored_key("deepseek").unwrap(), None);
        assert!(!has_key(&ZAI_CODING_CN));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn dirs_created_under_home() {
        let _g = env_lock();
        let home = temp_home();
        unsafe { std::env::set_var("HOME", &home) };

        // Asking where the settings are is not a reason to make a directory: a
        // read that made one would leave a `~/.caocli` behind for a user who has
        // never stored anything, and would create it under whichever HOME the
        // process happened to have.
        let d = caocli_dir().unwrap();
        assert_eq!(d, home.join(".caocli"));
        assert!(!d.is_dir(), "the path is not the directory");
        assert_eq!(
            history_file().unwrap(),
            home.join(".caocli").join("history")
        );
        assert_eq!(
            settings_file().unwrap(),
            home.join(".caocli").join("settings.json")
        );
        assert!(!d.is_dir(), "and neither of those made it either");

        // What writes there makes it. The settings file is the case this exists
        // for: a key is the first thing many users store.
        let path = store_key("deepseek", "sk-test")
            .expect("no settings file to read is the empty settings");
        assert_eq!(path, home.join(".caocli").join("settings.json"));
        assert!(d.is_dir());

        let s = sessions_dir().unwrap();
        assert_eq!(s, home.join(".caocli").join("sessions"));
        assert!(s.is_dir());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn missing_home_is_error() {
        let _g = env_lock();
        unsafe { std::env::remove_var("HOME") };
        let err = home_dir().unwrap_err().to_string();
        assert!(err.contains("HOME"), "err: {err}");
    }
}
