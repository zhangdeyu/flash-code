# 0.7.1 SWE-bench Harness 验收记录

## 目标

跑通 SWE-bench Verified 形状的数据链路:task metadata -> repo checkout/cache -> issue prompt -> Agent runtime -> patch collector -> grader -> report。

0.7.1 采用离线固定 task,元数据字段对齐 SWE-bench Verified 的核心字段,但不接入外部数据集下载、Docker runner 或云端提交。这样可以先验证 Flash Code 自己的 runtime、tool、session、patch、grader 链路,避免在第一步引入不可控环境复杂度。

参考公开资料:

- SWE-bench 数据集字段包含 `instance_id`、`repo`、`base_commit`、`problem_statement`、`patch`、`test_patch`、`FAIL_TO_PASS`、`PASS_TO_PASS`。
- SWE-bench Verified 是专家筛选的 500 个任务子集。
- `sb-cli submit swe-bench_verified <split> --predictions_path <path>` 是后续对接公开提交链路时需要兼容的形态。

## 交付

- `crates/eval/fixtures/swe_bench_verified_smoke.lock`: 固定 verified smoke task lock。
- `flash_eval::swe_bench_verified_tasks`: task metadata loader。
- `flash_eval::run_swe_bench_verified`: SWE-bench Verified harness runner。
- `flash eval swe-bench --subset verified --limit 1`: CLI 入口。
- task 级 `result.json`、`report.md`、`issue_prompt.md`、`task_metadata.json`。
- run 级 `result.json`、`report.md`、`swe_bench_verified_smoke.lock`。
- `model.patch`、`events.jsonl`、`grader.stdout.txt`、`grader.stderr.txt` 保留。

## 验收标准映射

| 标准 | 验收方式 |
|---|---|
| 对 1 个固定 verified task 完整执行 checkout、agent、patch、grader | `flash eval swe-bench --subset verified --limit 1` 输出 `resolved: 1/1` |
| 运行目录与用户当前 workspace 隔离 | 所有 task workspace 位于 `.flash/evals/<run>/tasks/<instance>/workspace` |
| patch、events、grader log 都能保留 | task result 中有 `patch_path`、`events_path`,task 目录有 `grader.stdout.txt` 和 `grader.stderr.txt` |
| task 超时边界能记录 timeout | `swe_bench_task_should_record_timeout_before_execution` 覆盖 `timeout_secs = 0` |
| 不引入 SWE-bench 专用 runtime 或权限绕过 | runner 复用 `AgentRuntime<SmokeProvider>`、`flash_tools::builtin_registry()` 和 `PermissionPolicy::Yolo` |

## 验收命令

```bash
cargo test -p flash-eval
cargo test -p flash-cli
cargo run -p flash-cli -- eval swe-bench --subset verified --limit 1
scripts/check.sh
```

实际结果:

- `cargo test -p flash-eval`: 9 passed。
- `cargo test -p flash-cli`: passed。
- `cargo run -p flash-cli -- eval swe-bench --subset verified --limit 1`: `resolved: 1/1`。
- `scripts/check.sh`: nextest 94 passed。

## 退出条件

0.7.1 退出后,可以进入 0.7.2:

- 固定 task list 从 1 个扩大到 10 个。
- 报告继续记录 resolved/unresolved、耗时、命令数、token usage、失败原因。
- 外部 SWE-bench Verified 数据和真实 repo checkout 只在 0.7.2 或之后接入。
