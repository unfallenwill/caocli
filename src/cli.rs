use anyhow::{Result, bail};
use clap::Parser;
use std::path::PathBuf;

/// What this invocation of caocli is asked to do.
///
/// Three modes, mutually exclusive and exhaustive: a session lists itself, runs
/// once, or stays open for turns. Which front end renders the interactive case
/// (the full-screen TUI or the plain prompt) is settled at run time, not here —
/// the TUI is what the front end attempts first, and the plain prompt is the
/// fallback when it declines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    ListSessions,
    OneShot {
        prompt: String,
        images: Vec<PathBuf>,
    },
    Interactive {
        no_status_bar: bool,
    },
}

#[derive(Debug, Parser)]
#[command(
    name = "caocli",
    version,
    about = "Terminal coding agent · DeepSeek / Z.AI / MiniMax backends · thinking + Bash"
)]
pub struct Cli {
    /// One-shot mode: run this prompt (including the tool loop), then exit
    #[arg(short = 'p')]
    pub prompt: Option<String>,

    /// Attach an image to the one-shot prompt (PNG, JPEG, WebP or GIF; repeat
    /// for more than one)
    #[arg(long, value_name = "PATH")]
    pub image: Vec<std::path::PathBuf>,

    /// Model id as `<provider>/<modelid>` — the provider comes from the name,
    /// not from a separate flag. When `--model` is not given, the first
    /// provider with a stored key is used.
    #[arg(long)]
    pub model: Option<String>,

    /// Thinking effort tier: low|high|max (or on|off on the Anthropic wire;
    /// provider defaults otherwise)
    #[arg(long)]
    pub effort: Option<String>,

    /// Continue the most recent session
    #[arg(short = 'c', long)]
    pub cont: bool,

    /// Resume a specific session by id
    #[arg(long)]
    pub resume: Option<String>,

    /// List sessions and exit
    #[arg(long)]
    pub list: bool,

    /// Disable the REPL status bar (cache hit rate)
    #[arg(long)]
    pub no_status_bar: bool,

    /// Keep the plain prompt instead of the full-screen front end. The plain
    /// prompt is used anyway when stdout is not a terminal, or when the terminal
    /// will not take raw mode.
    #[arg(long)]
    pub no_tui: bool,

    /// Approval gate: ask y/N before Bash/Edit/Write executes (a call that changes
    /// nothing on disk -- Read, Glob, TodoWrite -- is always allowed)
    #[arg(long)]
    pub ask: bool,
}

impl Cli {
    /// The mode this run is in, derived from the flags. Each flag combination
    /// collapses to exactly one variant: `--list` is `ListSessions`, `--prompt`
    /// is `OneShot`, anything else is `Interactive` (including a bare
    /// invocation).
    ///
    /// `--image` without `--prompt` is rejected here: an attached image has
    /// nowhere to go except the one-shot prompt, and the interactive front end
    /// takes images through `/image`, not through the command line.
    pub(crate) fn mode(&self) -> Result<Mode> {
        if self.list {
            return Ok(Mode::ListSessions);
        }
        if let Some(prompt) = &self.prompt {
            return Ok(Mode::OneShot {
                prompt: prompt.clone(),
                images: self.image.clone(),
            });
        }
        if !self.image.is_empty() {
            bail!(
                "--image attaches an image to the one-shot prompt (-p); \
                 in the interactive front end, use /image <path> [text]"
            );
        }
        Ok(Mode::Interactive {
            no_status_bar: self.no_status_bar,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("caocli").chain(args.iter().copied()))
    }

    #[test]
    fn list_flag_is_list_sessions() {
        assert_eq!(cli(&["--list"]).mode().unwrap(), Mode::ListSessions);
    }

    #[test]
    fn prompt_with_images_is_one_shot() {
        assert_eq!(
            cli(&["-p", "hi", "--image", "a.png"]).mode().unwrap(),
            Mode::OneShot {
                prompt: "hi".into(),
                images: vec![PathBuf::from("a.png")],
            }
        );
    }

    #[test]
    fn bare_invocation_is_interactive() {
        assert_eq!(
            cli(&[]).mode().unwrap(),
            Mode::Interactive {
                no_status_bar: false
            }
        );
    }

    #[test]
    fn no_status_bar_carries_through_to_interactive() {
        assert_eq!(
            cli(&["--no-status-bar"]).mode().unwrap(),
            Mode::Interactive {
                no_status_bar: true
            }
        );
    }

    #[test]
    fn image_without_prompt_is_rejected() {
        let err = cli(&["--image", "a.png"]).mode().unwrap_err().to_string();
        assert!(err.contains("--image"), "err: {err}");
        assert!(err.contains("-p"), "err: {err}");
    }

    #[test]
    fn list_wins_over_other_flags_when_present() {
        // `--list` and `-p` together: list is the verb, prompt is dropped.
        assert_eq!(
            cli(&["--list", "-p", "ignored"]).mode().unwrap(),
            Mode::ListSessions
        );
    }
}
