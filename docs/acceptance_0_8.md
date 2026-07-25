# 0.8 回归评测与报告验收记录

## 目标

形成小而稳定的防退化机制,用于 release 前检查。0.8 不新增评测 runtime,只编排现有内部 fixture、Terminal-Bench smoke 和 SWE-bench Verified fixed subset,并生成趋势 JSON 与 Markdown 汇总报告。

## 交付

- `flash eval regression`。
- 固定内部 regression task:`fix-rust`。
- 固定 Terminal-Bench subset:`terminal-bench_smoke_2026-07-25`。
- 固定 SWE-bench Verified subset:`swe-bench-verified-smoke-10-2026-07-25`。
- `regression_result.json`。
- `trend.json`。
- `report.md`。
- replay links。

## 验收标准映射

| 标准 | 验收方式 |
|---|---|
| 一条命令能跑 smoke regression | `flash eval regression` |
| 报告能对比本次与上次 pass rate | 连续两次运行 regression,第二次 `previous_pass_rate_bps` 不为空 |
| 报告能列出新增失败任务 | `new_failures` 写入 `regression_result.json` 和 `trend.json`,报告中有 `New Failures` |
| 报告能链接到对应 replay 文件 | `report.md` 中有 `Replay Links`,子评测 result 记录 `events.jsonl` |
| 报告区分 agent、环境、benchmark 失败 | `agent_failures`、`environment_failures`、`benchmark_failures` 分别写入 JSON 和 Markdown |
| release 前必须有一次 smoke regression 结果 | 当前迭代实际运行 `flash eval regression` |
| fixed subset 版本固定,公开基准更新时不能静默改变历史对比口径 | 报告记录每个 benchmark 的 `lock_version` |

## 验收命令

```bash
cargo test -p flash-eval
cargo test -p flash-cli
cargo run -p flash-cli -- eval regression
cargo run -p flash-cli -- eval regression
scripts/check.sh
```

实际结果:

- `cargo test -p flash-eval`: 13 passed。
- `cargo test -p flash-cli`: passed。
- 第一次 `cargo run -p flash-cli -- eval regression`: `passed: 12/12`, `previous_pass_rate_bps: none`。
- 第二次 `cargo run -p flash-cli -- eval regression`: `passed: 12/12`, `previous_pass_rate_bps: 10000`, `new_failures: 0`。
- 第二次 regression 产物:生成 `report.md`、`regression_result.json`、`trend.json`。
- `report.md`:包含 `Replay Links`、`New Failures`、fixed subset lock version、agent/environment/benchmark failure 计数。
- `scripts/check.sh`: nextest 98 passed。

## 退出条件

- 每次 release 都能通过同一套 smoke regression 做横向对比。
- 公开 benchmark 用于发现能力短板,而不是只追总分。
