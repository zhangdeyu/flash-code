# Flash Code 0.6.1 验收记录

版本:0.6.1 Eval Harness 基础

## 1. 验收命令

```bash
cargo test -p flash-eval
cargo build -p flash-cli

# from /private/tmp/flash-code-0-6-1-cli
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash eval fixture --task fix-rust
find .flash/evals -maxdepth 2 -type f | sort
sed -n '1,120p' .flash/evals/eval_1784948274791912000/result.json
sed -n '1,160p' .flash/evals/eval_1784948274791912000/report.md
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash replay .flash/evals/eval_1784948274791912000/workspace/.flash/sessions/session_1784948274851406000/events.jsonl

# second run, verifies non-overwrite behavior
/Users/derek/Project/github/zhangdeyu/flash-code/target/debug/flash eval fixture --task fix-rust
find .flash/evals -maxdepth 1 -type d | sort

scripts/check.sh
```

## 2. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| eval harness 可以运行一个本地 fixture task | `flash eval fixture --task fix-rust` 成功运行,输出 `passed: true` |
| eval task 调用同一个 `AgentRuntime` | `flash-eval` 使用 `AgentRuntime<SmokeProvider>`、`flash_tools::builtin_registry()` 和正常 permission policy,不存在评测专用 runtime |
| eval run 输出目录不覆盖历史结果 | 连续两次运行生成 `.flash/evals/eval_1784948274791912000` 和 `.flash/evals/eval_1784948288487138000` |
| task stdout/stderr、events、artifacts 都能保留 | eval run 保存 `grader.stdout.txt`、`grader.stderr.txt`;workspace 内保留 `.flash/sessions/<session>/events.jsonl` 和 artifacts 目录 |
| agent failure、environment failure、grader failure 能区分 | `EvalFailureKind::{AgentFailure,EnvironmentFailure,GraderFailure}` 写入 `result.json`;`eval_failure_kind_should_have_stable_json_names` 固定 JSON 名称 |

## 3. 新增交付

- `crates/eval`
- `flash eval fixture --task fix-rust`
- `EvalTask` / `EvalRun` / `EvalResult`
- eval run 目录:`.flash/evals/eval_<timestamp>/`
- `result.json`
- `report.md`
- `grader.stdout.txt`
- `grader.stderr.txt`

## 4. 结论

- 0.6.1 已建立 provider-agnostic 的 eval harness 基础。
- 下一阶段进入 0.6.2 Terminal-Bench Adapter,在此基础上接固定 smoke subset。
