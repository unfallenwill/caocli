# caocli — agent 维护手册

本项目由 AI agent 开发和维护。面向 agent 的约定，非人类读者向。

## 命令

- 格式化: `cargo fmt`（CI 有 `--check` 门禁，提交前必须格式化）
- Lint: `cargo clippy --all-targets -- -D warnings`（CI 门禁）
- 测试: `cargo test`
- 覆盖率: `cargo llvm-cov --fail-under-lines 90`（CI 门禁，行覆盖 ≥90%；本地需 `cargo install cargo-llvm-cov` + `rustup component add llvm-tools-preview`）
- 依赖安全审计: CI 每次推送跑 `rustsec/audit-check`；本地可 `cargo install cargo-audit && cargo audit`
- 端到端冒烟: `DEEPSEEK_API_KEY=... cargo run -- -p "1+1 等于几" --no-think`
- 会话列表: `cargo run -- --list`
- 提交即触发 CI（.github/workflows/ci.yml）：fmt + clippy + test + 覆盖门禁 + audit，任一红不许合入

## 结构（一文件一职责，勿合并）

| 文件 | 职责 |
|---|---|
| `src/types.rs` | API wire 类型（字段名=API 字段名）+ `TurnAccumulator` 流式聚合 |
| `src/config.rs` | `DEEPSEEK_API_KEY`、`~/.caocli` 路径、默认常量 |
| `src/tools/mod.rs` | 工具定义汇总、按名字分派、截断/参数解析公共件 |
| `src/tools/shell.rs` | `Bash`（120s 超时，永不返回 Err） |
| `src/tools/fs.rs` | `Read`/`Edit`/`Write`（UTF-8 文本，Edit 要求 old_string 唯一匹配，写回 tmp+rename 原子替换） |
| `src/session.rs` | JSONL append-only 会话存储 |
| `src/api.rs` | HTTP + SSE 解析（`parse_sse_line`/`take_line` 纯函数可测） |
| `src/ui.rs` | 终端渲染（思维链 DIM 灰色，工具调用黄色，块间空行分隔）+ `replay()` 历史回放 + 底部状态栏（`StatusBar` 占最后一行，滚动区域 1..rows-1） |
| `src/agent.rs` | 核心循环：请求→渲染→tool_calls→执行工具→继续 |
| `src/cli.rs` | clap 参数 |
| `src/main.rs` | REPL / `-p` 单次模式 / 会话解析 |

## 工具

- `Bash`: bash -c 执行命令，120s 超时，stdout/stderr 各截断 10KB
- `Read`: 读 UTF-8 文本文件，输出截断 10KB
- `Edit`: `file_path` + `old_string` + `new_string` 精确替换；old_string 必须唯一，否则报错让模型补上下文；tmp+rename 原子写
- `Write`: 新建/整文件覆盖，自动建父目录
- 工具输出上限 10KB/项，单文件读写上限 10MB
- `definitions()` 顺序固定（Bash, Read, Edit, Write）：**改变工具集或顺序会改变请求前缀，导致 KVCache 全量 miss（预期行为，但要有意识）**
- 工具执行结果永远是文本，永不 Err：错误也回传给模型让它自己纠偏

## DeepSeek API 硬约束（改代码前必读）

文档: <https://api-docs.deepseek.com/zh-cn/guides/thinking_mode> `~/.firecrawl/` 下有快照。

1. 端点 `POST https://api.deepseek.com/chat/completions`，OpenAI 兼容 + DeepSeek 扩展
2. **请求带 `tools` 时，历史 assistant 消息的 `reasoning_content` 必须原样回传，缺失 → 400**；不带 tools 时会被忽略
3. `thinking: {"type": "enabled"|"disabled"}`，默认 enabled；`reasoning_effort: low|high|max`（medium/xhigh 映射为 high）；disabled 时 effort 不传
4. 思考模式下 `temperature`/`top_p` 无效（不报错）；`presence_penalty`/`frequency_penalty` 已废弃
5. 流式：SSE；`delta.reasoning_content` 先于 `delta.content`；`data: [DONE]` 结束；usage 附在**最后一个内容块**（无独立 usage 块）
6. 流式 `tool_calls` 按 `index` 分片：id/name 首片给出，arguments 逐片追加 → `TurnAccumulator` 聚合
7. `finish_reason`: `stop|length|content_filter|tool_calls|insufficient_system_resource`
8. 模型: `deepseek-v4-flash`（默认）/ `deepseek-v4-pro`

## KVCache 前缀缓存规则（改消息构造前必读）

文档: <https://api-docs.deepseek.com/zh-cn/guides/kv_cache>

- 前缀**完整匹配**"缓存前缀单元"才命中
- `SYSTEM_PROMPT` 是编译期常量，**禁止**注入时间/cwd/随机内容（有测试钉住）
- 历史消息**逐字节回放**：不 trim、不重排、不裁剪、不压缩
- 任何"优化历史"的改动 = 缓存全 miss = 成本和延迟上涨
- 验证: ui 渲染 `hit/miss` token（来自 `usage.prompt_cache_hit_tokens/miss`）

## 会话存储 `~/.caocli/sessions/<id>.jsonl`

- 第一行 `{"t":"header","id":...,"created_at":...,"model":...,"thinking":{...},"reasoning_effort":...}`
- 消息行 `{"t":"msg","message":{...}}`（字段名与 API 一致）
- meta 变更追加 `{"t":"meta",...}`，读取时**后行覆盖前行**
- 只追加，永不重写；读取时跳过损坏行（崩溃容错，坏行只可能出现在尾部）
- id 格式 `YYYYMMDD-HHMMSS`（同秒冲突加 `-N` 后缀）

## 行为约定

- REPL 输入：`Enter` 提交，`Ctrl-J` 插入换行（多行输入，`enable_multiline` 覆盖 rustyline 默认的 `AcceptOrInsertLine` 绑定）；`Shift-Enter` 无效（终端发同一个字节）。改键位后必须用 pty 冒烟：`( sleep 3; printf '/help\x0aX\r/exit\r' ) | script -qec "stty rows 24 cols 80; DEEPSEEK_API_KEY=x cargo run -q" /dev/null | cat -v`——看到「未知命令」说明 Ctrl-J 换行生效，看到 `/help` 帮助文本说明没生效
- 工具执行不 y/N 确认（本机信任模型），但必须回显命令
- 状态栏只在 REPL + TTY（`rows >= 3`）启用；`--no-status-bar` 关闭。退出路径必须 `ui.teardown()` 复位滚动区域，否则终端会残留滚动区域
- 状态栏统计是**进程内、会话级**累加（切换会话 `ui.reset_stats()`）；会话文件不存 hit/miss，恢复会话从 0 起算
- 恢复会话（`-c` / `--resume` / `/resume`）必须调用 `ui.replay(&messages)` 把历史回放到屏幕，否则只有提示行、看不到上下文；回放只给 tool 结果摘要（与实时渲染一致），system 消息不在会话文件里（`SYSTEM_PROMPT` 是编译期常量，请求时才拼）
- **全进程只有一个 `Renderer`**：`main` 持有，`Agent::turn(&mut ui)` 借用。禁止让 `Agent` 自建 `Renderer`——`usage()` 会把 hit/miss 记到另一个没建栏的实例上，状态栏永远停在 `cache —`（已有测试钉住 `turn` 必须把 usage 写进传入的 renderer）
- 改 `StatusBar` 的转义序列后，用 pty 冒烟验证：`printf '/exit\n' | script -qec "stty rows 24 cols 80; DEEPSEEK_API_KEY=x cargo run -q" /dev/null | cat -v`
- 工具输出截断 10KB（stdout/stderr 各自），截断须落在 UTF-8 字符边界
- `-p` 模式是 agent 自测主通道：改完代码先 `cargo test`，再跑一次 `-p` 冒烟
