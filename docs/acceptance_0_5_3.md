# Flash Code 0.5.3 验收记录

版本:0.5.3 Agent 与 TUI 测试加固

## 1. 验收命令

```bash
cargo test -p flash-agent mock_provider_scenarios_should_cover_loop_outcomes
cargo test -p flash-agent rust_fixture_smoke_should_fix_failing_test_and_keep_diff
cargo test -p flash-tui render_to_string_should_match_key_state_snapshot
scripts/check.sh
```

`scripts/check.sh` 执行:

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
```

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| mock provider 覆盖成功、unknown tool、tool failure、max_turns、cancel | `flash-agent::mock_provider_scenarios_should_cover_loop_outcomes` 用 deterministic provider/tool 场景覆盖五类 outcome |
| 小型 Rust fixture 修复测试稳定通过 | `flash-agent::rust_fixture_smoke_should_fix_failing_test_and_keep_diff` 创建本地 Rust crate,用同一 `AgentRuntime` + `SmokeProvider` + 真实 tool registry 修复 `41 -> 42`,再跑 `cargo test` 和 `git diff` 验证 |
| TUI snapshot 覆盖空状态、streaming、approval、tool output、error | `flash-tui::render_to_string_should_match_key_state_snapshot` 对比 `crates/tui/tests/golden/key_state_snapshot.txt`,覆盖 session 空列表、streaming answer、approval panel、tool output、error |
| CI 不要求真实 DeepSeek API key | `scripts/check.sh` 只运行 fmt/test/clippy/nextest;所有新增测试使用 mock provider 或本地 fixture,不读取 `DEEPSEEK_API_KEY` |
| 一条本地命令能跑完整内部测试门槛 | `scripts/check.sh` 是 0.5 内部测试的一键入口 |

## 3. 新增测试与脚本

- `flash-agent::mock_provider_scenarios_should_cover_loop_outcomes`
- `flash-agent::rust_fixture_smoke_should_fix_failing_test_and_keep_diff`
- `flash-tui::render_to_string_should_match_key_state_snapshot`
- `crates/tui/tests/golden/key_state_snapshot.txt`
- `scripts/check.sh`

## 4. 结论

- 0.5.3 已把 Agent loop、TUI key state snapshot、小型 fixture smoke 和本地 CI 入口纳入自动化测试。
- 0.5 内部自动化测试体系完成,下一阶段进入 0.6 Terminal-Bench Smoke。
