# Flash Code 0.1 验收记录

版本:0.1 协议、存储与 CLI 骨架

## 1. 验收命令

已执行:

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo run -p flash-cli -- --version
zsh -ic 'cargo run -p flash-cli -- doctor'
cargo run -p flash-cli -- run "0.1 acceptance smoke"
cargo run -p flash-cli -- replay .flash/sessions/session_1784908037536971000/events.jsonl
cargo run -p flash-cli -- resume session_1784908037536971000
```

结果:全部通过。

## 2. 验收标准映射

| 标准 | 证据 |
|---|---|
| `cargo fmt --check` 通过 | 已执行通过 |
| `cargo clippy --all-targets --all-features -- -D warnings` 通过 | 已执行通过 |
| `cargo test --all-features` 通过 | 已执行通过,13 个测试通过 |
| git repo 内 `flash doctor` 能识别 workspace root | 输出 `/Users/derek/Project/github/zhangdeyu/flash-code` |
| `.flash/` 默认在 `.gitignore` 中忽略 | `.gitignore` 包含 `.flash/` |
| 配置优先级正确 | `config::tests::load_should_apply_config_precedence` |
| `DEEPSEEK_API_KEY` 缺失时不打印 secret | doctor 只打印 env name 和 present/missing |
| `flash init` 或首次运行创建 `.flash/workspace.json` | `flash doctor` / `flash run` 已创建 |
| 新建 session 后生成 `session.json` | `.flash/sessions/session_1784908037536971000/session.json` |
| user message 写入 `messages.jsonl` | `messages.jsonl` 包含 user message |
| event 写入 `events.jsonl` | replay 输出 `session_started`、`user_message_appended` |
| replay 能打印事件时间线 | replay 输出 `1: session_started`、`2: user_message_appended` |
| 跨 workspace resume 拒绝 | `storage::tests::load_session_should_reject_workspace_mismatch` |
| 同名 tool 注册失败 | `tools::tests::register_should_reject_duplicate_tool_names` |
| permission policy 输出 allow/ask/deny | `tools::tests::*permission_policy*` |

## 3. 已知边界

- 0.1 不进行真实 DeepSeek 调用。
- 0.1 无参数 `flash` 显示帮助;0.4 起默认进入 TUI。
- JSON/TOML 解析保持最小实现,只覆盖当前配置和存储格式。
