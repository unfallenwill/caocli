use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
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
/// marked. There is no "default" column: the current model is the only one the
/// menu singles out, and a model the session is not on is no different from any
/// other usable choice.
pub fn model_menu(current: &str) -> Vec<Choice> {
    let mut rows = Vec::new();
    for p in PROVIDERS {
        for model in p.models {
            let id = format!("{}/{}", p.id, model);
            let detail = match (id == current, has_key(p)) {
                (true, _) => "current".to_string(),
                (false, false) => format!("no key: /login {}", p.id),
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

/// The home directory the environment names, or the error that says it names
/// none.
///
/// `HOME` is the variable every Unix sets, and the one a user who keeps their
/// dotfiles somewhere else sets deliberately. Windows does not set it: the
/// profile is named by `USERPROFILE`, and `HOMEDRIVE` + `HOMEPATH` is the pair
/// an environment that dropped it still spells the same path with. Those two
/// come first there, because the shells that do set `HOME` on Windows -- MSYS,
/// Cygwin -- spell it `/c/Users/me`, which no native program can open: taking it
/// would put a `~/.caocli` under whatever the current drive happens to be
/// instead of in the profile. `HOME` is last rather than absent, so an
/// environment stripped of everything Windows sets still names a home.
fn home_dir() -> Result<PathBuf> {
    let windows = cfg!(windows);
    home_from(&|name| std::env::var_os(name), windows).with_context(|| {
        if windows {
            "cannot determine HOME directory: USERPROFILE, HOMEDRIVE+HOMEPATH and HOME are unset \
             or empty"
        } else {
            "cannot determine HOME directory: HOME is unset or empty"
        }
    })
}

/// [`home_dir`]'s rule, with the environment and the platform passed in rather
/// than read from the process.
///
/// The order is the part worth a test, and a test binary runs on one platform:
/// taking both as arguments is what lets the Windows order be a test on the
/// machine this is developed on, instead of only on the one it applies to.
fn home_from(get: &impl Fn(&str) -> Option<OsString>, windows: bool) -> Option<PathBuf> {
    // A variable that holds nothing names no directory. Taking it would turn
    // `~/.caocli` into a `.caocli` beside whatever the program was run from.
    let set = |name: &str| get(name).filter(|value| !value.is_empty());
    if windows {
        if let Some(profile) = set("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (set("HOMEDRIVE"), set("HOMEPATH")) {
            let mut joined = drive;
            joined.push(path);
            return Some(PathBuf::from(joined));
        }
    }
    set("HOME").map(PathBuf::from)
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

/// The current working directory as the layout wants to read it: a
/// PathBuf for an absolute path, `None` when the cwd could not be
/// determined. Falls back to the `_no-working-directory` sentinel
/// rather than erroring out: a session whose cwd was not recorded
/// (older runs, broken environments) still needs to land somewhere.
fn current_cwd() -> Option<PathBuf> {
    std::env::current_dir().ok().filter(|p| p.is_absolute())
}

/// `~/.caocli/sessions/<escaped cwd>/`, made here rather than by each
/// writer: the sessions are what this directory is for, and a session
/// is written into it on the way in. Sessions now live under a
/// per-working-directory subdirectory so `--continue` means "the most
/// recent session of *this* workspace", and `--list --all` can show
/// every workspace without reading one workspace's history into
/// another's listing.
pub fn sessions_dir() -> Result<PathBuf> {
    let dir = caocli_dir()?.join("sessions");
    let cwd = current_cwd();
    let dir = dir.join(crate::session::escape_working_directory(cwd.as_deref())?);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create directory: {}", dir.display()))?;
    Ok(dir)
}

/// Walk every layout subdirectory under `~/.caocli/sessions/` and
/// return `(path, cwd-display)` for each. The cwd-display is the
/// unescaped name (the original absolute path) when the name was a
/// known escape, or the escaped name itself when it was the
/// `_no-working-directory` sentinel. `~/_no-working-directory` is
/// reported with a fixed label rather than its (unreadable) path.
pub fn all_sessions_dirs() -> Result<Vec<(PathBuf, String)>> {
    let base = caocli_dir()?.join("sessions");
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&base)
        .with_context(|| format!("failed to read directory: {}", base.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let label = match crate::session::unescape_working_directory(&name) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => name,
        };
        out.push((path, label));
    }
    Ok(out)
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

/// The API key for a provider, or the absence of one with the message that says so.
///
/// Missing is data, not error: a one-shot run fails on missing, an interactive
/// session prints it in the banner so the user knows `/login` is the next step.
/// Keeping the absence in the type (rather than `Result<String, _>`) is what
/// makes the two cases read differently at the call site — the `require`
/// method is the one that fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKey {
    Present(String),
    Missing { hint: String },
}

impl ApiKey {
    /// Read the key the user stored under this provider's id.
    pub fn load(provider: &Provider) -> Result<Self> {
        match stored_key(provider.id)? {
            Some(key) => Ok(ApiKey::Present(key)),
            None => Ok(ApiKey::Missing {
                hint: format!(
                    "no API key for {}: run /login {}",
                    provider.name, provider.id
                ),
            }),
        }
    }

    /// The key, or an error carrying the missing hint. One-shot runs use this:
    /// they have nowhere to ask, so they have to fail now rather than at the
    /// first request.
    pub fn require(self) -> Result<String> {
        match self {
            ApiKey::Present(key) => Ok(key),
            ApiKey::Missing { hint } => bail!("{hint}"),
        }
    }

    /// The hint to show in the banner, if the key is missing.
    pub fn missing_note(&self) -> Option<&str> {
        match self {
            ApiKey::Present(_) => None,
            ApiKey::Missing { hint } => Some(hint),
        }
    }
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
/// the ones behind it into failures of their own. Tests that mutate HOME or
/// PWD (any test that touches `sessions_dir`'s inputs) must hold this
/// for their entire read+write window. Tests across modules that both
/// touch the env share this single lock.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counter used to give env-driven tests unique temp dirs. Public so
/// other modules' tests can mint unique paths without colliding.
#[cfg(test)]
pub(crate) static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn next_counter() -> usize {
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}
/// A HOME of its own, so that a key a test finds under it is one the test itself
/// put there. The caller holds [`env_lock`] while it uses it.
#[cfg(test)]
pub(crate) fn scratch_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "caocli-test-home-{}-{}",
        std::process::id(),
        next_counter()
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
    let dir = std::env::temp_dir().join(format!(
        "caocli-test-cwd-{}-{}",
        std::process::id(),
        next_counter()
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
        let d = std::env::temp_dir().join(format!(
            "caocli-config-test-{}-{}",
            std::process::id(),
            next_counter()
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
                "mimo/mimo-v2.5-pro",
                "mimo/mimo-v2.5",
            ]
        );
        // The current one says so; a provider with no key behind it says where
        // to get one; every other funded model is just listed.
        assert_eq!(rows[1].detail, "current");
        assert_eq!(rows[0].detail, "", "no key column, no default column");
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

        // sessions_dir now puts a per-cwd layer under ~/.caocli/sessions/.
        // The exact cwd is the test process's cwd at the moment of the
        // call; we only check the directory exists and is inside the
        // sessions root.
        let s = sessions_dir().unwrap();
        assert!(s.starts_with(home.join(".caocli").join("sessions")));
        assert!(s.is_dir());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn missing_home_is_error() {
        let _g = env_lock();
        // Every name the rule reads, so the answer does not depend on which
        // platform the tests run on: this test is about the error, not about
        // which variable names a home here. Each one goes back afterwards, so
        // the next test to read one of them reads what it was given.
        let names = ["HOME", "USERPROFILE", "HOMEDRIVE", "HOMEPATH"];
        let saved: Vec<Option<OsString>> = names.iter().map(std::env::var_os).collect();
        for name in names {
            // SAFETY: env_lock held, and every name is put back below.
            unsafe { std::env::remove_var(name) };
        }
        let err = home_dir().unwrap_err().to_string();
        assert!(err.contains("HOME"), "err: {err}");
        for (name, value) in names.iter().zip(saved) {
            match value {
                // SAFETY: env_lock held.
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    /// An environment as [`home_from`] reads it: these names, these values, and
    /// nothing else.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: BTreeMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), OsString::from(value)))
            .collect();
        move |name| map.get(name).cloned()
    }

    /// The order, asked of both platforms from whichever one is running. Unix has
    /// one answer and Windows has three, and which one is taken is the whole of
    /// the rule -- `home_from` takes the platform so this can be a test
    /// anywhere, and the one place the platform is read is `home_dir`.
    #[test]
    fn unix_home_is_home_and_windows_home_is_the_profile() {
        let unix = env_of(&[("HOME", "/home/u"), ("USERPROFILE", r"C:\Users\u")]);
        assert_eq!(home_from(&unix, false), Some(PathBuf::from("/home/u")));

        // Windows: the profile Windows itself hands a program wins even when
        // HOME is set, because what an MSYS shell sets HOME to -- `/c/Users/u`
        // -- is a path this program cannot open.
        let both = env_of(&[("HOME", "/c/Users/u"), ("USERPROFILE", r"C:\Users\u")]);
        assert_eq!(home_from(&both, true), Some(PathBuf::from(r"C:\Users\u")));

        // A profile with no USERPROFILE is still spelled by the two halves, in
        // one piece and with no separator to add.
        let halves = env_of(&[("HOMEDRIVE", "C:"), ("HOMEPATH", r"\Users\u")]);
        assert_eq!(home_from(&halves, true), Some(PathBuf::from(r"C:\Users\u")));

        // And HOME is what is left when Windows' own names are gone.
        let only_home = env_of(&[("HOME", r"C:\tools\home")]);
        assert_eq!(
            home_from(&only_home, true),
            Some(PathBuf::from(r"C:\tools\home"))
        );
    }

    #[test]
    fn a_variable_that_names_nothing_is_no_home() {
        // An empty variable is not a directory, and the path built from one is
        // relative: `.caocli` would land beside whatever the program was run
        // from.
        let empty = env_of(&[("HOME", ""), ("USERPROFILE", ""), ("HOMEDRIVE", "")]);
        assert_eq!(home_from(&empty, false), None);
        assert_eq!(home_from(&empty, true), None);

        // Half of the pair is not a home either.
        let half = env_of(&[("HOMEDRIVE", "C:"), ("HOME", "")]);
        assert_eq!(home_from(&half, true), None);
    }

    #[test]
    fn api_key_load_returns_present_when_stored() {
        let _g = env_lock();
        let home = bare_home();
        store_key("deepseek", "sk-stored").unwrap();
        assert_eq!(
            ApiKey::load(&DEEPSEEK).unwrap(),
            ApiKey::Present("sk-stored".into())
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn api_key_load_returns_missing_with_hint_when_absent() {
        let _g = env_lock();
        let home = bare_home();
        match ApiKey::load(&ZAI_CODING_CN).unwrap() {
            ApiKey::Missing { hint } => {
                assert!(hint.contains("Z.AI"), "hint names the provider: {hint}");
                assert!(
                    hint.contains("/login zai-coding-cn"),
                    "hint names the command: {hint}"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn api_key_require_passes_present_through() {
        let key = ApiKey::Present("sk-test".into());
        assert_eq!(key.require().unwrap(), "sk-test");
    }

    #[test]
    fn api_key_require_fails_with_the_hint() {
        let key = ApiKey::Missing {
            hint: "no API key for X".into(),
        };
        let err = key.require().unwrap_err().to_string();
        assert!(err.contains("no API key for X"), "{err}");
    }

    #[test]
    fn api_key_missing_note_is_none_when_present() {
        let key = ApiKey::Present("sk-test".into());
        assert_eq!(key.missing_note(), None);
    }

    #[test]
    fn api_key_missing_note_carries_the_hint() {
        let key = ApiKey::Missing {
            hint: "the hint".into(),
        };
        assert_eq!(key.missing_note(), Some("the hint"));
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    // Reuse the crate-level `env_lock` / `next_counter` so the env
    // is serialised with `config::tests` and the unique-id counter
    // is shared with every other env-touching test in the crate.

    #[test]
    fn sessions_dir_lives_under_an_escaped_cwd_subdirectory() {
        // Hold the env lock for the whole read of HOME + write of
        // sessions_dir + read of HOME again, so a concurrent test
        // cannot change HOME in between.
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!(
            "caocli-config-layout2-{}-{}",
            std::process::id(),
            next_counter()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env_lock held.
        unsafe {
            std::env::set_var("HOME", &dir);
        }
        let s = sessions_dir().unwrap();
        let expected_root = caocli_dir().unwrap().join("sessions");
        assert!(
            s.starts_with(&expected_root),
            "sessions_dir sits under the sessions root: {} under {}",
            s.display(),
            expected_root.display()
        );
        assert!(s.is_dir(), "the directory is created on first access");
        let _ = std::fs::remove_dir_all(&expected_root);
    }

    #[test]
    fn sessions_dir_under_a_no_cwd_runtime_uses_the_sentinel() {
        // We cannot reliably make `current_dir()` fail, but we can verify
        // the helper that calls it: when the cwd resolves to a relative
        // path, the helper falls back to the sentinel. The escape helper
        // itself is unit-tested in session.rs; here we assert the path the
        // layout builds when cwd is None.
        let encoded = crate::session::escape_working_directory(None).unwrap();
        assert_eq!(encoded, "_no-working-directory");
    }

    #[test]
    fn all_sessions_dirs_walks_under_caocli_sessions() {
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!(
            "caocli-config-layout3-{}-{}",
            std::process::id(),
            next_counter()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env_lock held.
        unsafe {
            std::env::set_var("HOME", &dir);
        }
        let base = caocli_dir().unwrap().join("sessions");
        let a = base.join("aaaaaaaa-aaaaaaaa");
        let b = base.join("bbbbbbbb-bbbbbbbb");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let all = all_sessions_dirs().unwrap();
        let paths: Vec<_> = all.iter().map(|(p, _)| p.clone()).collect();
        assert!(paths.contains(&a), "first workspace is walked");
        assert!(paths.contains(&b), "second workspace is walked");

        let _ = std::fs::remove_dir_all(&base);
    }
}
