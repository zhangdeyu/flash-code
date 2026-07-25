# Flash Code 0.3.x 验收记录

版本:0.3.x 工具协议收敛

## 1. 验收命令

已执行:

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build -p flash-cli

cargo run -p flash-cli -- run "list files"
cargo run -p flash-cli -- replay .flash/sessions/session_1784945125007018000/events.jsonl

# fixture: /private/tmp/flash-code-0-3x-fixture
cargo test
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash run "fix failing tests"
cargo test
git diff -- src/lib.rs
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash replay .flash/sessions/session_1784945163500206000/events.jsonl
rg -n '"name":"(Read|Edit|Bash|ListFiles)"' .flash/sessions/session_1784945163500206000/events.jsonl
```

结果:全部通过。

## 2. 验收标准映射

| 标准 | 证据 |
|---|---|
| `flash run "list files"` 触发 `ListFiles` | session `1784945125007018000` 的 events 包含 `name":"ListFiles"` |
| `flash run "fix failing tests"` replay 出现 `Read`、`Edit`、`Bash` | session `1784945163500206000` 的 events 包含 `Read`、`Edit`、两次 `Bash` |
| 旧工具名仍能被 runtime 解析 | `builtin_registry_should_expose_new_protocol_names_and_legacy_aliases` |
| `Glob`、`Grep`、`ListFiles` 都是 Read 风险 | 对应 `Tool::risk` 返回 `ToolRisk::Read` |
| `Edit`、`Write` 受 workspace path guard 保护 | 复用 `existing_workspace_path` / `writable_workspace_path`; tests 覆盖越界 |
| `Bash` 对 destructive 命令返回 Destructive 风险 | `BashTool` 复用 `command_risk` |
| 验收通过后提交并 push | 本记录随 0.3.x commit 提交并 push |

## 3. 结果摘要

- Agent 可见工具已收敛为 `Read`、`Edit`、`Write`、`Glob`、`Grep`、`ListFiles`、`Bash`。
- 旧工具名 `read_file`、`apply_patch`、`write_file`、`search`、`shell`、`run_tests`、`git_diff` 继续注册为兼容别名。
- `Search` 语义被拆分为 `Glob`、`Grep`、`ListFiles`。
