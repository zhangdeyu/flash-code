# Flash Code 本地存储方案

本文档定义 v1 的本地存储。原则是够用、清楚、可恢复,不做过度设计。

核心结论:

- Session 必须绑定 workspace。
- A 目录创建的 session 默认只能在 A 目录 resume。
- 跨目录可以 replay,但不能继续执行。
- v1 不强制 `index.json`。
- v1 不强制单独 `trajectory.jsonl`。
- v1 不强制单独 `compactions.jsonl`。

## 1. 存储边界

Flash Code 保留两类入口:

- TUI:主交互界面,0.4 起无参数 `flash` 默认进入。
- Headless CLI:自动化、评测、replay、doctor、CI、单次任务。

两者共享同一套 session 存储。

不规划:

- Web UI。
- Browser dashboard。
- HTTP server 前端协议。

## 2. Workspace 绑定

每个 session 都绑定一个 workspace root。

workspace root 规则:

- 在 git repo 内运行时,workspace root 是 git root。
- 在非 git repo 内运行时,workspace root 是启动 `flash` 时的 canonical cwd。

resume 规则:

- `flash resume <session_id>` 只能恢复当前 workspace 下的 session。
- 如果当前目录不是 session 所属 workspace,默认拒绝。
- `flash replay <path>` 可以跨目录只读查看。

这样可以避免把 A 项目的历史、git 状态、工具权限误带到 B 项目。

## 3. 目录结构

项目级默认存储:

```text
<workspace-root>/
  .flash/
    config.toml
    workspace.json
    sessions/
      session_01J4Z6K8YB7Q9N3X2R4M5A6C7D/
        session.json
        messages.jsonl
        events.jsonl
        artifacts/
```

非项目目录可以使用用户级 workspace 存储:

```text
~/.config/flash-code/
  config.toml

~/.local/share/flash-code/
  workspaces/
    <workspace_id>/
      workspace.json
      sessions/
        session_xxx/
          session.json
          messages.jsonl
          events.jsonl
          artifacts/
```

v1 不使用日期目录。时间放在 `session.json` 里。

`.flash/` 是本地运行状态目录,默认不提交到 git。团队共享配置不要直接提交 `.flash/config.toml`;等出现真实需求后再设计显式模板文件,例如 `flash.example.toml`。

## 4. 配置文件

v1 只支持两层配置:

```text
~/.config/flash-code/config.toml   # 用户级默认配置
<workspace-root>/.flash/config.toml # 当前 workspace 覆盖配置
```

优先级:

```text
default values
  -> user config
  -> workspace config
  -> environment variables
  -> CLI args
```

### 4.1 用户级配置

`~/.config/flash-code/config.toml` 保存个人默认偏好:

```toml
[provider]
default = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
default_model = "deepseek-v4-flash"
reasoning_effort = "high"

[agent]
approval_mode = "confirm"
max_turns = 50

[tools.shell]
timeout_secs = 120
max_output_bytes = 200000
```

### 4.2 Workspace 配置

`<workspace-root>/.flash/config.toml` 保存当前项目的覆盖项:

```toml
[provider]
model = "deepseek-v4-pro"

[agent]
approval_mode = "confirm"

[tools]
allow_network = false
```

workspace 配置只应该保存项目相关的非敏感偏好,例如:

- 默认模型。
- approval mode。
- shell timeout。
- tool output 限制。
- 是否允许网络。
- 项目特定测试命令,如果后续需要。

因为 `.flash/` 默认忽略,workspace 配置主要服务本机当前 checkout。需要团队共享的默认值先写进 README 或后续模板文件,不要在 v1 里引入额外配置层。

配置只保存默认策略,不保存某一次工具调用的授权结果。单次授权结果写入 `events.jsonl` 的 `approval_resolved`,这样 replay 能还原当时发生了什么,而 resume 不会把历史授权误用于新的工具调用。

### 4.3 Secret 规则

配置文件不保存 secret 值。

禁止写入:

- API key。
- access token。
- password。
- private key。
- `.env` 原文。

只保存环境变量名:

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"
```

`flash doctor` 负责检查环境变量是否存在,但不能打印 secret 值。

### 4.4 初始化与检查

`flash init` 是可选命令。v1 可以让 `flash doctor` 或首次 `flash run` 自动创建 `.flash/workspace.json` 和 `.flash/sessions/`。

最小职责:

- 确认 workspace root。
- 创建 `.flash/`。
- 创建 `.flash/workspace.json`。
- 如果 `.flash/config.toml` 不存在,可以提示用户创建,但不强制。
- 不写入 API key。

`flash doctor` 最小检查:

- 当前 workspace root。
- 用户级 config 是否可读。
- workspace config 是否可读。
- 配置合并后 provider/model 是什么。
- `api_key_env` 指向的环境变量是否存在。
- `.flash/` 是否可写。
- 不打印 secret 值。

### 4.5 为什么不拆更多配置文件

v1 不需要:

- `providers.toml`
- profile 目录
- project policy 文件
- secret store
- team managed config

等出现真实需求再拆。当前两个 TOML 文件已经能覆盖个人默认配置和项目覆盖配置。

## 5. 文件作用

v1 每个 session 只需要三个核心文件。

| 文件 | 作用 | 是否必须 |
|---|---|---|
| `session.json` | session 元数据,包括 workspace、模型、状态、时间 | 必须 |
| `messages.jsonl` | 已 commit 的对话消息,用于继续对话 | 必须 |
| `events.jsonl` | 流式事件和工具事件,用于 TUI/CLI replay 和调试 | 必须 |
| `artifacts/` | 大 stdout/stderr、patch、debug 文件 | 按需 |

### 5.1 为什么不只用一个文件

不能只用一个 `session.jsonl`,主要是因为读写路径不同:

- resume 只需要 committed messages,不能读到半截 token。
- TUI/replay 需要 token delta、stdout delta 这类细粒度事件。
- session 列表只需要很小的元数据。

所以最小拆分是:

```text
session.json    # 元数据
messages.jsonl  # 可继续对话的事实历史
events.jsonl    # 可回放的事件历史
```

这三个文件已经够 v1 使用。

### 5.2 `index.json` 的作用

`index.json` 只是一个可选缓存,用于快速列出 session,类似目录索引。

没有 `index.json` 时,TUI 或 CLI 可以直接扫描:

```text
.flash/sessions/*/session.json
```

因此 v1 不需要实现 `index.json`。

只有当单个 workspace 下 session 很多、扫描变慢时,再引入:

```text
.flash/sessions/index.json
```

如果未来引入,它也必须是派生缓存:

- 可以删除。
- 可以重建。
- 不能作为事实来源。
- 不能绕过 workspace resume 校验。

## 6. Workspace 元数据

`.flash/workspace.json`:

```json
{
  "version": "1",
  "workspace_id": "wk_6J4Z6K8YB7Q9N3X2R4M5A6",
  "root": "/Users/derek/Project/github/zhangdeyu/flash-code",
  "kind": "git",
  "git_root": "/Users/derek/Project/github/zhangdeyu/flash-code",
  "created_at": "2026-07-24T10:20:30Z",
  "updated_at": "2026-07-24T10:35:12Z"
}
```

`workspace_id`:

```text
workspace_id = base32(sha256(canonical_workspace_root))[0..26]
```

如果 `.flash/workspace.json.root` 和当前 workspace root 不一致:

- `flash resume` 拒绝。
- `flash replay` 允许只读。
- 后续可以提供显式 repair/move 命令,但 v1 不必实现。

## 7. Session 元数据

`session.json`:

```json
{
  "version": "1",
  "session_id": "session_01J4Z6K8YB7Q9N3X2R4M5A6C7D",
  "title": "fix failing tests",
  "created_at": "2026-07-24T10:20:30Z",
  "updated_at": "2026-07-24T10:35:12Z",
  "workspace": {
    "workspace_id": "wk_6J4Z6K8YB7Q9N3X2R4M5A6",
    "root": "/Users/derek/Project/github/zhangdeyu/flash-code",
    "git_root": "/Users/derek/Project/github/zhangdeyu/flash-code",
    "git_head": "abc1234",
    "branch": "main"
  },
  "provider": {
    "id": "deepseek",
    "model": "deepseek-v4-flash"
  },
  "state": {
    "status": "finished",
    "outcome": "success",
    "last_event_sequence": 182
  },
  "files": {
    "messages": "messages.jsonl",
    "events": "events.jsonl"
  }
}
```

`session.json` 用途:

- TUI 列 session 时读取标题、时间、状态。
- resume 前校验 workspace。
- replay 时展示 provider/model。
- storage check 时校验文件完整性。

## 8. Messages

`messages.jsonl` 是继续对话的事实来源。

一行一个 committed message:

```json
{
  "version": "1",
  "message": {
    "id": "msg_01J4Z6M0ABCD",
    "role": "user",
    "created_at": "2026-07-24T10:20:31Z",
    "content": [
      {
        "type": "text",
        "text": "修复当前项目失败的测试"
      }
    ]
  }
}
```

规则:

- user message 提交后写入。
- assistant message 只在模型 turn 完成后写入。
- tool result 只在工具有终态后写入。
- 流式 token 不写入 `messages.jsonl`。
- 半截 assistant 输出不写入 `messages.jsonl`。
- resume 只读取 `messages.jsonl`。

这条规则很重要:它保证 crash 或 cancel 后,不会把半截模型输出带入下一轮 prompt。

## 9. Events

`events.jsonl` 是回放和调试用事件流。

一行一个 event:

```json
{
  "version": "1",
  "sequence": 42,
  "timestamp": "2026-07-24T10:20:42Z",
  "session_id": "session_01J4Z6K8YB7Q9N3X2R4M5A6C7D",
  "event": {
    "type": "assistant_delta",
    "text": "我先运行测试。"
  }
}
```

`events.jsonl` 可以包含:

- `user_message_appended`
- `model_request_started`
- `reasoning_delta`
- `assistant_delta`
- `assistant_message_completed`
- `tool_call_requested`
- `approval_required`
- `approval_resolved`
- `tool_started`
- `tool_output_delta`
- `tool_finished`
- `usage_recorded`
- `error`
- `session_finished`

用途:

- TUI 从事件重建 transcript。
- `flash replay` 只读回放。
- 调试流式输出和工具执行。
- 早期兼任 trajectory。

v1 中,`events.jsonl` 就是唯一事件账本。公开评测接入前,不必单独拆出 `trajectory.jsonl`。

## 10. Artifacts

`artifacts/` 只在需要时创建。

用于保存:

- 很长的 stdout。
- 很长的 stderr。
- patch。
- diff。
- debug request/response。

示例:

```text
artifacts/
  tool_call_call_001.stdout.txt
  tool_call_call_001.stderr.txt
  tool_call_call_002.patch
```

message 和 event 中只保存摘要和相对路径:

```json
{
  "path": "artifacts/tool_call_call_001.stderr.txt",
  "kind": "stderr",
  "bytes": 18422
}
```

规则:

- artifact path 必须是 session 目录内相对路径。
- 文件名不能直接来自用户输入。
- 保存前做 secret redaction。
- 大文件有大小上限。

## 11. Resume 流程

```text
current cwd
  -> resolve workspace root
  -> read .flash/workspace.json
  -> locate .flash/sessions/<session_id>/session.json
  -> compare workspace id/root
  -> read messages.jsonl
  -> rebuild History
  -> continue
```

如果 session 不属于当前 workspace:

```text
error: session belongs to another workspace
current: /path/to/B
session: /path/to/A
hint: run from the original workspace, or use `flash replay <path>` for read-only replay
```

## 12. Replay 流程

```text
flash replay .flash/sessions/session_xxx/events.jsonl
  -> read events.jsonl
  -> render timeline
```

Replay 不调用模型,不执行工具,不要求当前目录等于 session workspace。

## 13. 写入顺序

用户输入:

```text
append messages.jsonl
append events.jsonl
update session.json
```

模型流式输出:

```text
append events.jsonl per delta or coalesced delta
commit assistant message to messages.jsonl only after done
update session.json
```

工具执行:

```text
append tool events to events.jsonl
write artifacts if needed
append committed tool result to messages.jsonl
update session.json
```

崩溃恢复:

- 如果 `events.jsonl` 有 assistant delta,但 `messages.jsonl` 没有 assistant commit,该 turn 视为 interrupted。
- resume 时只用 `messages.jsonl` 重建 History。
- TUI 可以通过 `events.jsonl` 显示 interrupted 状态。

## 14. 后续可选扩展

以下都不是 v1 必须项:

- `sessions/index.json`:session 列表缓存。
- `trajectory.jsonl`:面向公开评测的完整审计轨迹。
- `compactions.jsonl`:上下文压缩记录。
- `snapshots/`:最终 diff、summary 等派生产物。
- 全局 recent sessions。
- storage migrate/gc/repair 命令。
- 多 profile 配置。
- 独立 provider 配置文件。

引入条件:

- session 数量多到扫描目录变慢,再加 `index.json`。
- 接 Terminal-Bench/SWE-bench 后,再拆 `trajectory.jsonl`。
- 实现上下文压缩后,再加 `compactions.jsonl`。
- TUI 需要首页最近任务时,再加 recent cache。
- 多模型/多供应商配置复杂后,再拆 `providers.toml` 或 profiles。

## 15. v1 实现要求

0.1 必须实现:

- workspace root 解析。
- 用户级 `~/.config/flash-code/config.toml` 读取。
- workspace 级 `.flash/config.toml` 读取。
- 配置优先级合并。
- `api_key_env` 检查,不落盘 secret。
- `.flash/workspace.json`。
- session directory 创建。
- `session.json`。
- `messages.jsonl`。
- `events.jsonl`。
- `flash replay` 读取 events。
- `flash resume` workspace 校验。

0.2 增加:

- `artifacts/`。
- 工具输出截断和 artifact 引用。
- interrupted run 恢复展示。

0.6 前后再考虑:

- `trajectory.jsonl`。
- `compactions.jsonl`。
- `index.json`。
- eval report artifact。
