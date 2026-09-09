# caocli

终端 AI agent，Rust 实现。后端 DeepSeek `/chat/completions`，思考模式（`reasoning_content` 思维链流式灰色渲染）+ `run_shell` 工具循环，会话以 JSONL 追加日志持久化于 `~/.caocli/sessions/`，支持恢复与 KVCache 前缀缓存友好的逐字节历史回放。

```bash
export DEEPSEEK_API_KEY=sk-...

cargo run --                      # REPL（/help 查看 /new /sessions /resume /exit）
cargo run -- -c -p "查看磁盘占用"  # 接最近会话单次执行
cargo run -- --no-think -p "1+1"  # 关闭思考模式
cargo run -- --effort max --model deepseek-v4-pro -p "..."
cargo run -- --list               # 列出会话
```

## 构建 / 测试

```bash
cargo build
cargo test
```

## 设计要点

- 单一后端、单一工具，刻意保持最小
- 带 `tools` 的请求历史必须回传 `reasoning_content`（DeepSeek 硬约束），因此会话文件完整保存
- 只追加不重写的会话日志：崩溃只损失尾部半行
- `SYSTEM_PROMPT` 为编译期常量、历史逐字节回放——保住 DeepSeek 硬盘缓存命中
- [`AGENTS.md`](AGENTS.md) 是给 AI agent 的维护手册（API 硬约束、结构、自测通道）

## License

[MIT](LICENSE)
