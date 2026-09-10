use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "caocli",
    version,
    about = "Terminal coding agent · DeepSeek / GLM backends · thinking + Bash"
)]
pub struct Cli {
    /// One-shot mode: run this prompt (including the tool loop), then exit
    #[arg(short = 'p')]
    pub prompt: Option<String>,

    /// Provider: deepseek (default) | glm
    #[arg(long)]
    pub provider: Option<String>,

    /// Model id (defaults to the provider's default model)
    #[arg(long)]
    pub model: Option<String>,

    /// Thinking effort: low|high|max (default max)
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

    /// Approval gate: ask y/N before Bash/Edit/Write executes (Read is always allowed)
    #[arg(long)]
    pub ask: bool,
}
