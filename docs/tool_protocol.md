# Flash Code Agent 工具协议

本文档定义暴露给 Agent 的 v1 工具协议。目标是少、稳定、语义清楚,避免把内部 helper 全部暴露给模型。

## 1. v1 Agent 可见工具

| 工具 | 作用 | 默认风险 |
|---|---|---|
| `Read` | 读取指定文件的完整或部分内容 | Read |
| `Edit` | 对现有文件进行精准、有针对性的修改 | Write |
| `Write` | 创建或完全覆写文件内容 | Write |
| `Glob` | 基于模式匹配快速查找文件和目录路径 | Read |
| `Grep` | 在文件内容中通过正则或关键字搜索代码逻辑 | Read |
| `ListFiles` | 列出特定路径下的文件和目录结构 | Read |
| `Bash` | 在隔离或本地环境中执行 shell 命令行操作 | Execute / Destructive |

## 2. 工具语义

### Read

用于读取已知路径的文件内容。后续可支持 offset/limit,但 v1 不要求复杂分页协议。

### Edit

用于修改已有文件的一小段内容。优先使用 find/replace 或 patch 风格输入,避免整文件重写。

### Write

用于创建新文件或完全覆写文件。风险高于 `Edit`,因为它更容易破坏已有内容。

### Glob

用于按路径模式找文件或目录,例如 `**/*.rs`、`crates/*/Cargo.toml`。

### Grep

用于按关键字或正则搜索文件内容,例如函数名、错误信息、测试名、配置项。

### ListFiles

用于查看某个目录下的文件和目录结构。它和 `Glob` 的区别是:  
`ListFiles` 用于理解局部目录结构,`Glob` 用于根据模式找路径。

### Bash

用于执行命令,例如 `cargo test`、`git diff`、`rg`、`cargo fmt`。  
模型应优先使用结构化工具完成查找和编辑,只有需要执行命令时才使用 `Bash`。

## 3. 内部实现映射

实现层允许拆小工具,但 Agent 和 Provider tool spec 应优先使用 v1 工具名。

| 当前实现 | 目标 Agent 工具 |
|---|---|
| `read_file` | `Read` |
| `apply_patch` | `Edit` |
| `write_file` | `Write` |
| `search` | `Glob` / `Grep` / `ListFiles` |
| `shell` | `Bash` |
| `run_tests` | `Bash` 的受控场景或内部 helper |
| `git_diff` | `Bash` / `Read` 的受控场景或内部 helper |

兼容要求:

- runtime 可以在过渡期继续接受旧工具名。
- replay 中的历史旧工具名不需要迁移。
- 新 provider tool spec 应逐步切到 v1 工具名。

## 4. 后置工具

这些工具不进入 v1 默认暴露:

| 工具 | 后置原因 |
|---|---|
| `TaskCreate` / `TaskUpdate` / `TaskGet` / `TaskList` | 需要任务状态模型、TUI 展示和 resume 语义 |
| `Agent` | 属于 SubAgent,会引入独立上下文、权限继承和事件归属 |
| `AskUserQuestion` | 需要同时定义 TUI、CLI、headless eval 下的交互行为 |

## 5. 权限约束

- `Read`、`Glob`、`Grep`、`ListFiles` 是 Read 风险。
- `Edit`、`Write` 是 Write 风险,必须受 workspace path guard 保护。
- `Bash` 根据命令内容动态判断 Execute 或 Destructive。
- `Destructive` 永远需要显式确认。
- `human` 模式下任何工具都不真实执行,只写 rejected tool result。
