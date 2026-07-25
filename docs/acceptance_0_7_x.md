# 0.7.x SubAgent / Task / AskUser 评估点验收记录

## 目标

评估是否需要在 v1 前引入 `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList`、`AskUserQuestion`、只读 `Agent` 等复杂交互工具。

本阶段不是默认实现迭代。只有当单 Agent loop、基础 event 状态和 approval 已经不足时,才进入候选实现。当前结论是继续不实现这些工具,保持 v1 Agent 可见工具协议克制。

## 触发条件评估

| 触发条件 | 当前证据 | 结论 |
|---|---|---|
| 大仓库代码定位明显拖慢主 loop | 当前 0.6/0.7 smoke 都是小型固定任务,尚无大仓库定位瓶颈证据 | 未触发 |
| 评测失败主要来自上下文收集不足 | Terminal-Bench smoke 和 SWE-bench Verified 10-task smoke 均通过 | 未触发 |
| 需要把分析任务和执行任务隔离 | 当前 runtime 没有出现分析污染执行的失败样本 | 未触发 |
| TUI 中长期任务需要结构化 todo,单纯 events 难以表达进度 | 当前 TUI 已能展示 transcript、reasoning、tool output、approval、error,暂无长任务 todo 需求证据 | 未触发 |
| 模型频繁需要向用户询问歧义,approval 不能表达问题类型 | 当前 smoke 和 fixture 不依赖模型主动询问用户 | 未触发 |

## 候选工具结论

| 候选 | 决策 | 原因 |
|---|---|---|
| `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList` | 暂不实现 | 会引入 task state event、replay 重建和 TUI task panel,当前没有验收失败证明必要 |
| `AskUserQuestion` | 暂不实现 | 需要同时定义 TUI、CLI、headless eval 语义,当前 approval 和失败状态足够 |
| 只读 `Agent` | 暂不实现 | 会引入 child context、只读 registry、parent event attribution 和 timeout,当前公开 smoke 未证明需要 |

## 验收标准映射

因为本阶段结论是“不实现候选工具”,适用的验收标准是边界验收:

| 标准 | 验收方式 |
|---|---|
| 不新增 SubAgent 写权限风险 | 工具协议仍只暴露 `Read/Edit/Write/Glob/Grep/ListFiles/Bash` |
| 不新增不能 replay 的 task state | 未新增 `Task*` event 或新必需存储文件 |
| headless eval 不会因 AskUserQuestion 挂起 | 未新增 `AskUserQuestion`;现有 eval 仍走同步 Agent runtime |
| 所有工具仍受 permission policy 和 workspace guard 约束 | 现有工具、permission、workspace guard 测试继续通过 |
| 退出条件满足时继续不实现复杂工具 | 当前单 Agent loop 支撑 Terminal-Bench smoke 和 SWE-bench smoke |

## 验收命令

```bash
rg -n 'TaskCreate|TaskUpdate|TaskGet|TaskList|AskUserQuestion|\"Agent\"' crates docs/tool_protocol.md docs/iteration_plan.md
cargo test -p flash-tools
cargo test -p flash-agent
cargo test -p flash-eval
scripts/check.sh
```

实际结果:

- 边界搜索:候选工具名只出现在 `docs/tool_protocol.md` 和 `docs/iteration_plan.md` 的后置说明中,未出现在 `crates/` 实现代码中。
- `cargo test -p flash-tools`: 27 passed。
- `cargo test -p flash-agent`: 16 passed。
- `cargo test -p flash-eval`: 11 passed。
- `scripts/check.sh`: nextest 96 passed。

## 退出条件

- 0.7.x 评估门关闭,继续不实现 `Task*`、`AskUserQuestion`、只读 `Agent`。
- 下一阶段进入 0.8 回归评测与报告。
