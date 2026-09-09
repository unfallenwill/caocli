use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "caocli",
    version,
    about = "终端 agent · DeepSeek 后端 · 思维链 + Bash"
)]
pub struct Cli {
    /// 单次执行模式：执行该 prompt（含工具循环）后退出
    #[arg(short = 'p')]
    pub prompt: Option<String>,

    /// 模型 id（默认 deepseek-v4.1-flash-expires-on-0910）
    #[arg(long)]
    pub model: Option<String>,

    /// 关闭思考模式
    #[arg(long)]
    pub no_think: bool,

    /// 思考强度: low|high|max
    #[arg(long)]
    pub effort: Option<String>,

    /// 继续最近一次会话
    #[arg(short = 'c', long)]
    pub cont: bool,

    /// 按 id 恢复指定会话
    #[arg(long)]
    pub resume: Option<String>,

    /// 列出会话后退出
    #[arg(long)]
    pub list: bool,

    /// 关闭 REPL 底部状态栏（缓存命中率）
    #[arg(long)]
    pub no_status_bar: bool,
}
