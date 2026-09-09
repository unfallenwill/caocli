mod agent;
mod api;
mod cli;
mod config;
mod session;
mod tools;
mod types;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};

use crate::agent::Agent;
use crate::api::Client;
use crate::cli::Cli;
use crate::session::{Session, SessionMeta};
use crate::ui::Renderer;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

/// 新会话的 meta：全部来自 CLI 参数或默认值。
fn fresh_meta(cli: &Cli) -> SessionMeta {
    SessionMeta {
        model: cli
            .model
            .clone()
            .unwrap_or_else(|| config::DEFAULT_MODEL.to_string()),
        reasoning_effort: cli.effort.clone(),
    }
}

/// 恢复会话时：CLI 显式给出的参数覆盖 meta，未给出的沿用会话内保存值。
/// 返回 true 表示 meta 发生变化（需要追加 meta 行）。
fn apply_overrides(meta: &mut SessionMeta, cli: &Cli) -> bool {
    let mut changed = false;
    if let Some(m) = &cli.model
        && meta.model != *m
    {
        meta.model = m.clone();
        changed = true;
    }
    if let Some(e) = &cli.effort
        && meta.reasoning_effort.as_deref() != Some(e.as_str())
    {
        meta.reasoning_effort = Some(e.clone());
        changed = true;
    }
    changed
}

/// Ctrl-J 插入换行而不是提交，用于多行输入；Enter 仍然提交整段。
/// rustyline 默认把 Ctrl-J 和 Enter 都绑到 AcceptOrInsertLine，这里覆盖 Ctrl-J。
fn enable_multiline(rl: &mut rustyline::DefaultEditor) {
    let _ = rl.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
}

async fn run(cli: Cli) -> Result<()> {
    let mut ui = Renderer::new();
    let sdir = config::sessions_dir()?;

    if cli.list {
        for s in session::list(&sdir)? {
            println!(
                "{}\t{}条消息\t{}\t{}",
                s.id,
                s.message_count,
                s.preview,
                s.path.display()
            );
        }
        return Ok(());
    }

    let api = Client::new(config::api_key()?, config::BASE_URL.to_string())?;

    // 会话解析优先级: --resume > --continue > 新建
    let mut session = if let Some(id) = &cli.resume {
        let path = sdir.join(format!("{id}.jsonl"));
        Session::load(&path).with_context(|| format!("恢复会话 {id} 失败"))?
    } else if cli.cont {
        match session::latest(sdir.clone())? {
            Some(path) => Session::load(&path)?,
            None => Session::create(&sdir, fresh_meta(&cli))?,
        }
    } else {
        Session::create(&sdir, fresh_meta(&cli))?
    };

    if apply_overrides(&mut session.meta, &cli) {
        session.set_meta(session.meta.clone())?;
    }

    let mut agent = Agent::new(api, session);
    ui.set_model(&agent.session.meta.model);

    // 单次执行模式（agent 自测的主通道）
    if let Some(prompt) = &cli.prompt {
        ui.info(&format!(
            "会话 {} · {}{}",
            agent.session.id,
            agent.session.meta.model,
            agent
                .session
                .meta
                .reasoning_effort
                .as_deref()
                .map(|e| format!("({e})"))
                .unwrap_or_default()
        ));
        if let Err(e) = agent.turn(prompt, &mut ui).await {
            ui.error(&format!("{e:#}"));
            std::process::exit(1);
        }
        return Ok(());
    }

    // REPL
    let mut rl = rustyline::DefaultEditor::new()?;
    enable_multiline(&mut rl);
    let hist_path = config::history_file()?;
    let _ = rl.load_history(&hist_path);

    // 底部状态栏：只在 REPL + TTY 下启用
    if !cli.no_status_bar {
        ui.refresh_status_bar();
    }

    // --continue / --resume 恢复后提示来源文件，path 有诊断价值
    ui.info(&format!(
        "caocli · 会话 {}（{} 条历史，{}）· {} · /help 查看命令",
        agent.session.id,
        agent.session.messages.len(),
        agent.session.path.display(),
        agent.session.meta.model
    ));
    // 恢复的会话把历史回放到屏幕，否则只有提示行、看不到上下文
    if !agent.session.messages.is_empty() {
        ui.replay(&agent.session.messages);
    }

    loop {
        // 每轮输入前同步状态栏（顺带处理窗口缩放）
        if !cli.no_status_bar {
            ui.refresh_status_bar();
        }
        match rl.readline("› ") {
            Ok(raw) => {
                let line = raw.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                match line {
                    "/exit" | "/quit" | "/q" => break,
                    "/help" => print_help(),
                    "/sessions" => {
                        for s in session::list(&sdir)? {
                            ui.info(&format!("{}\t{}条\t{}", s.id, s.message_count, s.preview));
                        }
                    }
                    "/new" => match Session::create(&sdir, agent.session.meta.clone()) {
                        Ok(s) => {
                            ui.info(&format!("新会话 {}", s.id));
                            agent.session = s;
                            ui.reset_stats();
                            ui.set_model(&agent.session.meta.model);
                        }
                        Err(e) => ui.error(&format!("{e:#}")),
                    },
                    _ if line.starts_with("/resume ") => {
                        let id = line.trim_start_matches("/resume ").trim();
                        let path = sdir.join(format!("{id}.jsonl"));
                        match Session::load(&path) {
                            Ok(s) => {
                                ui.info(&format!(
                                    "已切换到会话 {}（{} 条历史）",
                                    s.id,
                                    s.messages.len()
                                ));
                                ui.replay(&s.messages);
                                agent.session = s;
                                ui.reset_stats();
                                ui.set_model(&agent.session.meta.model);
                            }
                            Err(e) => ui.error(&format!("{e:#}")),
                        }
                    }
                    _ if line.starts_with('/') => ui.info("未知命令，/help 查看可用命令"),
                    _ => {
                        if let Err(e) = agent.turn(line, &mut ui).await {
                            ui.error(&format!("{e:#}"));
                        }
                    }
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue, // Ctrl-C 清行
            Err(rustyline::error::ReadlineError::Eof) => break,            // Ctrl-D 退出
            Err(e) => {
                ui.error(&format!("readline 错误: {e}"));
                break;
            }
        }
    }
    let _ = rl.save_history(&hist_path);
    ui.teardown();
    Ok(())
}

fn print_help() {
    println!("{}", help_text());
}

fn help_text() -> String {
    "命令:\n  /exit /quit /q   退出\n  /new             开新会话\n  /sessions        列出会话\n  /resume <id>     切换到指定会话\n输入:\n  Enter            提交\n  Ctrl-J           换行（多行输入）\n启动参数:\n  -c / --continue  继续最近会话\n  --resume <id>    恢复指定会话\n  --effort low|high|max --model <id>\n  -p \"prompt\"     单次执行后退出"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("caocli").chain(args.iter().copied()))
    }

    #[test]
    fn no_cli_args_keeps_session_meta_untouched() {
        let mut meta = SessionMeta {
            model: "deepseek-v4-pro".into(),
            reasoning_effort: None,
        };
        assert!(!apply_overrides(&mut meta, &cli(&[])));
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort, None);
    }

    #[test]
    fn explicit_model_overrides_only_model() {
        let mut meta = SessionMeta {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: Some("high".into()),
        };
        assert!(apply_overrides(
            &mut meta,
            &cli(&["--model", "deepseek-v4-pro"])
        ));
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn effort_overrides_stored_value() {
        let mut meta = SessionMeta {
            model: "deepseek-v4-flash".into(),
            reasoning_effort: Some("high".into()),
        };
        assert!(apply_overrides(&mut meta, &cli(&["--effort", "low"])));
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));

        // 相同值不触发写盘
        assert!(!apply_overrides(&mut meta, &cli(&["--effort", "low"])));
    }

    #[test]
    fn fresh_meta_defaults() {
        let meta = fresh_meta(&cli(&[]));
        assert_eq!(meta.model, config::DEFAULT_MODEL);
        assert_eq!(meta.reasoning_effort, None);
    }

    #[test]
    fn fresh_meta_carries_model_and_effort() {
        let meta = fresh_meta(&cli(&["--model", "deepseek-v4-pro", "--effort", "max"]));
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn no_status_bar_flag_parses() {
        assert!(cli(&["--no-status-bar"]).no_status_bar);
        assert!(!cli(&[]).no_status_bar);
    }

    #[test]
    fn help_text_lists_slash_commands_and_flags() {
        let t = help_text();
        for expected in ["/resume <id>", "-c / --continue", "-p \"prompt\"", "Ctrl-J"] {
            assert!(t.contains(expected), "缺少 {expected:?}\n{t}");
        }
    }

    #[test]
    fn ctrl_j_is_bound_to_newline() {
        let mut rl = rustyline::DefaultEditor::new().unwrap();
        enable_multiline(&mut rl);
        // 已绑定的键再绑一次会返回旧处理器：证明 Ctrl-J 确实被占用，
        // 否则它仍走默认的 AcceptOrInsertLine（Enter 语义，无法换行）。
        let prev = rl.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
        assert!(prev.is_some());
    }
}
