# Flash Code 0.4.1 验收记录

版本:0.4.1 TUI Shell 与事件渲染

## 1. 验收命令

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build -p flash-cli
FLASH_TUI_ONCE=1 cargo run -p flash-cli --
FLASH_TUI_ONCE=1 cargo run -p flash-cli -- tui
cargo run -p flash-cli -- --help
cargo run -p flash-cli -- run "list files"
cargo run -p flash-cli -- tui
```

PTY 验收:

- `cargo run -p flash-cli -- tui` 输出 alternate screen enter 序列 `\x1b[?1049h`。
- 输入 `q` 后进程退出,并输出 cursor/alternate screen restore 序列 `\x1b[?25h\x1b[?1049l`。

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| `flash --help`、`flash run`、`flash replay`、`flash doctor` 仍保持 CLI 行为 | `cargo run -p flash-cli -- --help` 输出 CLI help;`cargo run -p flash-cli -- run "list files"` 成功并输出 session/outcome |
| 无参数 `flash` 和 `flash tui` 都进入同一 TUI | `FLASH_TUI_ONCE=1 cargo run -p flash-cli --` 与 `FLASH_TUI_ONCE=1 cargo run -p flash-cli -- tui` 都渲染 Flash Code TUI |
| 退出后 terminal raw mode 和 cursor 状态恢复 | PTY 中输入 `q` 后进程退出,输出 restore escape sequence |
| 使用 0.3.x 产生的 `events.jsonl` 可以重建 transcript | TUI 渲染显示 reasoning、assistant、tool output、session finished |
| snapshot 覆盖空状态、普通输出、reasoning、tool output、error | `flash-tui` unit tests 覆盖对应 render state |
| 窄宽度终端下文本不互相覆盖 | `render_to_string_should_keep_rows_within_width` 验证每行不超过指定宽度 |
| 本阶段不直接调用 Provider 或 Tool | `flash-tui` 只依赖 `flash-core`,不依赖 `flash-agent`、`flash-provider` 或 `flash-tools` |

## 3. 结论

- 0.4.1 已完成 TUI shell、session 扫描和 event transcript 渲染。
- `flash` 默认进入 TUI,带子命令时仍是 CLI 模式。
- TUI 当前是 replay viewer 外壳;live input、approval、resume 留到 0.4.2/0.4.3。
