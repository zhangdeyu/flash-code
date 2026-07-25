# Flash Code 0.6.x Skill 机制评估记录

版本:0.6.x Skill 机制评估点

## 1. 评估结论

不实现动态 Skill。

当前证据:

- 0.6.2 Terminal-Bench smoke adapter 通过,没有失败分析证明缺少可复用策略层。
- 当前代码中没有动态 Skill runtime、Skill loader、Skill selector 或 Skill 权限分支。
- `scripts/check.sh` 全部通过,nextest 为 90 passed。

## 2. 验收命令

```bash
rg -n "struct .*Skill|enum .*Skill|skill_source|selected skill|SkillSource|Skill" crates docs/acceptance_0_6_2.md docs/architecture.md docs/iteration_plan.md
sed -n '1,120p' docs/acceptance_0_6_2.md
scripts/check.sh
```

## 3. 验收标准映射

| 验收标准 | 结果 |
|---|---|
| Skill 只能影响 prompt projection 或可用工具选择 | 未实现动态 Skill,因此没有新增 prompt projection 或 tool selection 分支 |
| Skill 不能绕过 permission policy | 未实现动态 Skill,现有 runtime 仍只通过 `PermissionPolicy` 决策 |
| Skill 不能直接写 `messages.jsonl` / `events.jsonl` | 未实现动态 Skill,没有 Skill 代码写 session 文件 |
| Skill 文件中不得保存 secret 明文 | 未新增 Skill 文件或 Skill 存储目录 |
| Skill 行为必须能从 session replay 中解释 | 未启用 Skill,session replay 行为保持 0.6.2 现状 |

## 4. 退出条件

- 本次评估没有明确收益,继续不实现动态 Skill。
- 后续如 SWE-bench/Terminal-Bench 失败分析证明需要可复用策略注入,再进入 `0.6.x.1 最小 Skill 方案候选`。
