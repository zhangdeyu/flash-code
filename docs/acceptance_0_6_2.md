# Flash Code 0.6.2 验收记录

版本:0.6.2 Terminal-Bench Adapter

## 1. 验收命令

```bash
cargo test -p flash-eval
cargo build -p flash-cli

# from /private/tmp/flash-code-0-6-2-cli
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash eval terminal-bench --subset smoke
sed -n '1,160p' .flash/evals/eval_1784950189995978000/result.json
sed -n '1,200p' .flash/evals/eval_1784950189995978000/report.md
sed -n '1,80p' .flash/evals/eval_1784950189995978000/terminal_bench_smoke.lock
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash replay .flash/evals/eval_1784950189995978000/tasks/terminal-bench-smoke_local-rust-fix/workspace/.flash/sessions/session_1784950190055417000/events.jsonl

# second run, verifies non-overwrite behavior
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash eval terminal-bench --subset smoke
find .flash/evals -maxdepth 1 -type d | sort

scripts/check.sh
```

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| 能跑固定 smoke subset | `flash eval terminal-bench --subset smoke` 成功运行,输出 `passed: 1/1` |
| 每个 task 都产生 session 和 event log | CLI 输出每个 task 的 `events:` 路径;`flash replay <events.jsonl>` 可回放完整事件时间线 |
| report 包含 task id、pass/fail、耗时、命令数、token usage、失败原因 | `report.md` 包含 `Task / Pass / Duration ms / Commands / Tokens / Failure`;`result.json` 包含 `task_id`、`passed`、`duration_ms`、`command_count`、`input_tokens`、`output_tokens`、`failure_reason` |
| fixed subset 版本固定,公开基准更新不会静默改变 smoke 口径 | `crates/eval/fixtures/terminal_bench_smoke.lock` 固定 `lock_version=terminal-bench-smoke-2026-07-25` 和唯一 task `terminal-bench-smoke/local-rust-fix` |
| 不存在 Terminal-Bench 专用 runtime 或绕过工具权限的 prompt 分支 | adapter 调用 `run_task_in_eval_run`,该函数使用同一个 `AgentRuntime<SmokeProvider>`、`flash_tools::builtin_registry()` 和 `PermissionPolicy` |

## 3. 新增交付

- `flash eval terminal-bench --subset smoke`
- `crates/eval/fixtures/terminal_bench_smoke.lock`
- Terminal-Bench smoke task loader。
- Terminal-Bench smoke runner。
- Terminal-Bench summary `result.json`。
- Terminal-Bench summary `report.md`。
- command count 和 token usage metrics extraction。

## 4. 说明

- 本阶段锁定的是 Terminal-Bench adapter smoke surface,用于验证数据流、报告、replay 和退化防护。
- 外部 Harbor/Docker 真实公开任务扩大运行留给后续阶段;当前实现不污染日常 CLI/TUI runtime。
