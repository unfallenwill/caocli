# caocli — agent 维护手册

本项目由 AI agent 开发和维护。面向 agent 的约定，非人类读者向。

## 命令

- 构建: `cargo build`
- 测试: `cargo test`
- 端到端冒烟: `DEEPSEEK_API_KEY=... cargo run -- -p "1+1 等于几" --no-think`
- 会话列表: `cargo run -- --list`

## 结构（一文件一职责，勿合并）

| 文件 | 职责 |
|---|---|
| `src/types.rs` | API wire 类型（字段名=API 字段名）+ `TurnAccumulator` 流式聚合 |
| `src/config.rs` | `DEEPSEEK_API_KEY`、`~/.caocli` 路径、默认常量 |
| `src/tools.rs` | `run_shell` 工具定义与执行（永不返回 Err，错误作为 tool 结果文本回传） |
| `src/session.rs` | JSONL append-only 会话存储 |
| `src/api.rs` | HTTP + SSE 解析（`parse_sse_line`/`take_line` 纯函数可测） |
| `src/ui.rs` | 终端渲染（思维链 DIM 灰色，工具调用黄色） |
| `src/agent.rs` | 核心循环：请求→渲染→tool_calls→run_shell→继续 |
| `src/cli.rs` | clap 参数 |
| `src/main.rs` | REPL / `-p` 单次模式 / 会话解析 |

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

- 工具执行不 y/N 确认（本机信任模型），但必须回显命令
- 工具输出截断 10KB（stdout/stderr 各自），截断须落在 UTF-8 字符边界
- `-p` 模式是 agent 自测主通道：改完代码先 `cargo test`，再跑一次 `-p` 冒烟
