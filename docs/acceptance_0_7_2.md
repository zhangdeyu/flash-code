# 0.7.2 SWE-bench Verified Smoke 验收记录

## 目标

把 0.7.1 的 SWE-bench Verified harness 从 1 个固定 task 扩大到 10 个固定 task,并形成可以和 Terminal-Bench smoke 并列查看的 resolved/unresolved 报告。

0.7.2 仍然保持离线可复现:固定 task list 使用 SWE-bench Verified 的核心元数据形状,真实外部数据集下载、Docker runner、公开提交文件兼容留到后续阶段。这个边界能让当前迭代专注验证 Flash Code 自身的 Agent runtime、tool、session、patch、grader 和 report 链路。

## 交付

- `flash eval swe-bench --subset verified --limit 10`。
- 10 个固定 verified smoke task。
- run 级 resolved/unresolved/evaluated/environment_failures 统计。
- task 级 session id、events path、patch path、failure kind、failure reason。
- environment failure 不进入 `evaluated` 分母。

## 验收标准映射

| 标准 | 验收方式 |
|---|---|
| 10 个固定 task 能顺序运行 | `run_swe_bench_verified_should_run_fixed_10_task_subset` 和 CLI `--limit 10` |
| 每个 task 都能映射到 session id 和 patch | 测试断言所有 result 都有 `session_id` 和存在的 `patch_path` |
| report 记录 resolved/unresolved、耗时、命令数、token usage、失败原因 | run 级 `report.md` 和 `result.json` 包含汇总与 task 明细 |
| 环境失败不会计为 agent resolved/unresolved | `swe_bench_summary_should_exclude_environment_failures_from_evaluated_count` |
| 不存在 SWE-bench 专用 runtime 或权限绕过 | runner 继续复用 `AgentRuntime<SmokeProvider>`、`flash_tools::builtin_registry()`、`PermissionPolicy::Yolo` |

## 验收命令

```bash
cargo test -p flash-eval
cargo test -p flash-cli
cargo run -p flash-cli -- eval swe-bench --subset verified --limit 10
scripts/check.sh
```

实际结果:

- `cargo test -p flash-eval`: 11 passed。
- `cargo test -p flash-cli`: passed。
- `cargo run -p flash-cli -- eval swe-bench --subset verified --limit 10`: `resolved: 10/10`, `unresolved: 0`, `environment_failures: 0`。
- run 级产物检查:10 个 `model.patch`,10 个 `events.jsonl`。
- `scripts/check.sh`: nextest 96 passed。

## 退出条件

- SWE-bench smoke 可以和 Terminal-Bench smoke 并列用于能力分析。
- 下一步进入 0.7.x 评估门:只有当单 Agent loop、基础 event 状态和 approval 已经不足时,才引入 SubAgent / Task / AskUser。
