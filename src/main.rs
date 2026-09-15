mod agent;
mod agents_md;
mod api;
mod canonical;
mod cli;
mod config;
mod front;
mod history;
mod image;
mod machine;
mod migrate;
mod provider;
mod repl;
mod session;
mod startup;
mod tools;
mod types;
mod ui;

use anyhow::Result;
use clap::Parser;

use crate::agent::Agent;
use crate::cli::{Cli, Mode};
use crate::front::FrontEnd;
use crate::ui::{Front, Renderer};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    // Before anything is written: how this run paints and how it draws. Both are
    // process-wide and both are read by every front end, so they are settled once
    // here, from the flags and the settings file, while the terminal still has
    // nothing on it to redo.
    startup::install_visuals(&cli)?;
    // Migrate: a CLI verb that runs before any agent or MCP work is
    // started. It only needs the sessions directory, which the cli
    // path resolution can answer on its own.
    if matches!(cli.mode()?, cli::Mode::Migrate { .. }) {
        let sessions_dir = config::sessions_dir()?;
        return cli::run_migrate(&cli, &sessions_dir);
    }
    let mut ui = Renderer::new();
    let startup = startup::resolve_startup(&cli)?;

    // List sessions: a CLI verb that exits before the agent is built.
    if matches!(startup.mode, Mode::ListSessions) {
        return front::print_sessions(&startup.sessions_dir, cli.list_all);
    }

    // The MCP servers this workspace and the user's settings name, connected
    // before any turn is run: their tools are part of every request, and a
    // session that offered some of them would offer a different prefix than
    // the one it will send next. The guard shuts every connection down on
    // drop, so the three front ends all clean up the same way.
    //
    // `mcpServers` is read by the binary: it is the binary that owns the
    // settings file, and the crate that knows the protocol is not the one
    // that knows where the file lives. A missing or unreadable settings
    // file is not a hub's problem; the hub gets a `None` and carries on.
    let user_mcp = config::setting("mcpServers").ok().flatten();
    let mcp = caocli_mcp::McpGuard::new(caocli_mcp::Hub::spawn(
        &startup.workspace,
        user_mcp.as_ref(),
    ))
    .await;
    let mcp_notes = mcp.notes().to_vec();

    let mut agent = Agent::new(startup.client, startup.session, startup.provider);
    agent.approval = startup.approval;
    agent.mcp = mcp.hub();
    ui.set_model(&agent.model_label());
    ui.set_effort(agent.effort_label());

    FrontEnd::from_mode(
        &startup.mode,
        &agent,
        startup.sessions_dir,
        mcp_notes,
        &startup.api_key,
        &cli,
    )
    .run(&mut agent, &mut ui)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("caocli").chain(args.iter().copied()))
    }

    #[test]
    fn model_flag_parses() {
        // `--model` is the one provider-switching flag; it carries its own
        // `<provider>/<modelid>` shape.
        assert_eq!(
            cli(&["--model", "zai-coding-cn/glm-5.3"]).model.as_deref(),
            Some("zai-coding-cn/glm-5.3")
        );
        assert!(cli(&[]).model.is_none());
    }

    #[test]
    fn no_provider_flag_exists() {
        // The provider is part of the model spec; there is no separate flag.
        // Passing one is rejected by clap so the user finds out at startup.
        Cli::try_parse_from(["caocli", "--provider", "zai-coding-cn"]).unwrap_err();
    }

    #[test]
    fn no_status_bar_flag_parses() {
        assert!(cli(&["--no-status-bar"]).no_status_bar);
        assert!(!cli(&[]).no_status_bar);
    }

    #[test]
    fn ask_flag_defaults_off_and_parses() {
        assert!(
            !cli(&[]).ask,
            "execution is trusted by default, so do not ask"
        );
        assert!(cli(&["--ask"]).ask);
    }
}
