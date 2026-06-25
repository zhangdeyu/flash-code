# flash-code v1 测试用例矩阵

本文档列出 v1 重构后核心模块的测试用例清单,作为主线开发者的参考备忘。每条用例用一行短描述,包含输入条件、期望产物或错误。覆盖范围对齐 spec §3 / §5 / §10 / §11 / §12。

未列出端到端集成测试与 Tool 实现层用例(BashTool / ApprovalGate 决策表本身),专注于 History、Compaction、Loop、Provider 四层。

---

## 1. History 单测矩阵(§3 / §5)

### 1.1 push_user / push_assistant / push_tool_results 正常路径

- [push_user] blocks=[Text("hi")] -> messages.len=1, role=User, 返回非空 MessageId
- [push_user] blocks=[Text, Image(Base64)] -> 入栈成功,role=User
- [push_assistant] blocks=[Text("ok")] -> messages.len+=1, role=Assistant
- [push_assistant] blocks=[Text, ToolUse{c1}, Reasoning] -> 入栈成功,顺序保留
- [push_assistant] blocks=[Reasoning(text, sig=None)] -> 允许 signature 缺失
- [push_tool_results] 前置 assistant=[ToolUse{c1}] + results=[ToolResult{c1}] -> Ok,role=Tool
- [push_tool_results] 前置 assistant=[ToolUse{c1,c2}] + results=[ToolResult{c2}, ToolResult{c1}] -> Ok(集合相等,顺序无关)

### 1.2 push_tool_results 配对校验失败

- [push_tool_results] expected={c1,c2}, got={c1} -> Err(ToolResultMismatch{expected:[c1,c2], got:[c1]})
- [push_tool_results] expected={c1}, got={c1,c2} -> Err(ToolResultMismatch)(多余 c2)
- [push_tool_results] expected={c1}, got={c2} -> Err(ToolResultMismatch)(完全不匹配)
- [push_tool_results] expected={}(前一条 assistant 无 ToolUse) + got={c1} -> Err(ToolResultMismatch)
- [push_tool_results] 前一条非 Assistant(e.g. User) -> Err(ToolResultMismatch{expected:[], got:[...]}) 或同等错误

### 1.3 record_compaction 4 条硬校验

- [record_compaction] tail_start_id 在 messages 中找不到 -> Err(TailNotFound)
- [record_compaction] tail_start_id 指向 Assistant message -> Err(TailNotOnTurnBoundary)
- [record_compaction] tail_start_id 指向 User -> 通过(turn 边界合法)
- [record_compaction] tail_start_id 指向 Tool -> 通过(turn 边界合法)
- [record_compaction] 第二次压缩 new_pos == prev_pos -> Err(NonMonotonicCompaction{new, prev})
- [record_compaction] 第二次压缩 new_pos < prev_pos -> Err(NonMonotonicCompaction)
- [record_compaction] 第二次压缩 new_pos > prev_pos -> Ok
- [record_compaction] tail_start_id == messages.last().id -> Err(EmptyTail)

### 1.4 to_prompt_messages 投影

- [to_prompt_messages] 无压缩 -> 返回 messages.clone(),长度与 raw 相等
- [to_prompt_messages] 单次压缩 tail_idx=3 -> [Summary, msg3, msg4, ...],第一条 role=Summary
- [to_prompt_messages] 多次压缩 -> 仅保留 last() compaction,前面被合并(Summary 体内含 previous summary)
- [to_prompt_messages] 输出 message id 与 raw 一致(tail 段不重新生成 id)
- [to_prompt_messages] Summary message role=Summary,且不进入 history.messages

### 1.5 Reasoning block 投影

- [reasoning_projection] 无压缩,assistant 含 Reasoning -> 投影输出保留 Reasoning
- [reasoning_projection] 压缩点之前的 assistant 含 Reasoning -> 投影输出不含该 Reasoning(被 Summary 替代)
- [reasoning_projection] 压缩点之后(tail)的 assistant 含 Reasoning -> 投影输出保留该 Reasoning(含 signature)

### 1.6 投影幂等

- [projection_idempotent] 连续两次 to_prompt_messages 返回内容相等(deep eq)
- [projection_idempotent] 投影后再次投影,长度、role 序列、ContentBlock 序列均一致
- [projection_idempotent] MicroCompact 已 redact 的 ToolResult,二次投影不再变化

---

## 2. MicroCompact 矩阵(§10.8)

### 2.1 三条件全满足才 redact

- [micro_turn] 距离=5(>=4), size=10000(>=4000), 不在 keep_recent -> redact
- [micro_turn_only] 距离=5, size=500(<4000), 不在 keep_recent -> 不 redact(仅 turn 满足)
- [micro_size_only] 距离=2(<4), size=10000, 不在 keep_recent -> 不 redact(仅 size 满足)
- [micro_keep_recent] 距离=10, size=10000, rank 在最后 5 条内 -> 不 redact(被 keep_recent 保护)

### 2.2 占位符 size & 幂等

- [micro_idempotent_first] 首次调用产 redacted_ids=[c1,c2], bytes_saved>0
- [micro_idempotent_second] 第二次调用,占位符 size < threshold,不再 redact,redacted_ids=[]
- [micro_placeholder_text] redact 后该 ToolResult.content == [Text("[Old tool result content cleared]")]

### 2.3 不动 raw messages

- [micro_no_mutate] 调用 apply_micro_compact 后 history.raw_messages() 未变化
- [micro_only_projection] 仅投影返回的 Vec 被修改,history.messages 字节级相等
- [micro_replay_safe] replay history(MessageAppended/HistoryCompacted)不重放 micro 事件,投影自动得到相同结果

### 2.4 返回值正确性

- [micro_result_ids] 5 个 tool_result 中 redact 了 2 个 -> redacted_ids.len()==2 且顺序与原 message 顺序一致
- [micro_result_bytes] bytes_saved == 被 redact 的 ToolResult content 字节数总和
- [micro_result_empty] 无任何 ToolResult -> 返回 default(empty ids, 0 bytes)

---

## 3. CompactionPolicy 矩阵(§10.3 / §10.4)

### 3.1 should_compact 预测式触发

- [should_compact_under] current=50k, growth=2k, max_context=200k, reserved=20k -> false(50k+2k <= 180k)
- [should_compact_over] current=170k, growth=15k, max_context=200k, reserved=20k -> true(185k > 180k)
- [should_compact_predictive] current=175k <= usable, 但 current+growth=185k > usable -> true(预测式)
- [should_compact_empty] history 空 -> growth 退化为保底 2000,通常返回 false

### 3.2 select_tail turn 数不足

- [select_tail_short] messages.len < 3 -> None
- [select_tail_few_users] user_indices.len() <= keep_last_turns -> None
- [select_tail_exactly_enough] user_indices.len() == keep_last_turns + 1 -> Some(倒数第 keep_last_turns 个 user.id)

### 3.3 select_tail token 上限触发缩减

- [select_tail_token_ok] turn 选定后 tail_tokens <= max_tail_tokens -> 直接返回该 user.id
- [select_tail_token_shrink] tail_tokens > max_tail_tokens -> 推进到下一个 user message 直到塞下,返回更靠后的 id
- [select_tail_token_overflow_to_end] 推到末尾仍超限 -> None
- [select_tail_last_msg_guard] 唯一可选 candidate 是最后一条 message -> None(避开 EmptyTail)

### 3.4 select_tail_overflow

- [select_tail_overflow_aggressive] overflow_keep_last_turns=1 < keep_last_turns=2 -> 选出更靠后的 tail
- [select_tail_overflow_halve_token] max_tokens / 2 应用,tail 段更短
- [select_tail_overflow_unrecoverable] history 仍太短 -> None,Loop 据此上抛 ContextOverflow

---

## 4. Loop 矩阵(§12.4 / §12.9)

### 4.1 正常 turn

- [loop_normal_turn] 模型 -> ToolUse(c1) -> tool 执行 -> 模型 -> Done(EndTurn) -> Ok(())
- [loop_normal_block_order] assistant_blocks 顺序 [Text, ToolUse]:Text 先 flush 再 ToolUse(spec §12.5)
- [loop_normal_event_pairing] AssistantMessageStart 与 AssistantMessageEnd 各 emit 一次,配对

### 4.2 纯文本 turn 不计 MAX_TURNS

- [loop_text_only_no_turn_inc] 模型直接 Done(EndTurn) 无 ToolUse -> turns 不加,直接 return Ok
- [loop_text_then_tool_inc] 第一轮纯文本(Done) 立即返回;若有 ToolUse 才 +1

### 4.3 MAX_TURNS 超限

- [loop_max_turns_at_limit] 连续 50 个 tool 轮 -> turns==50 时 emit Error,return Err(MaxTurnsExceeded)
- [loop_max_turns_below] 49 轮 -> 正常结束不上抛
- [loop_max_turns_check_after_tool] turn 计数发生在 tool_calls 非空判定之后(不在纯文本路径上)

### 4.4 ContextOverflow 兜底

- [loop_overflow_first] stream 首次 Err(ContextOverflow) -> overflow_compact 调用一次,transition=OverflowRetried,continue
- [loop_overflow_second] 兜底后再次 Err(ContextOverflow) -> 上抛 Err(Provider(ContextOverflow))
- [loop_overflow_compact_fail] overflow_compact 内部 select_tail_overflow 返回 None -> Err(ContextOverflow) 上抛

### 4.5 RateLimited 重试

- [loop_rate_limited_first] Err(RateLimited{retry_after: Some(2s)}) -> sleep 2s,transition=TransientRetried,continue
- [loop_rate_limited_second] 重试后再次 RateLimited -> 上抛
- [loop_rate_limited_no_retry_after] retry_after=None -> 不 sleep 直接 continue

### 4.6 MaxTokens 升级 max_output

- [loop_maxtokens_first] StopReason::MaxTokens -> max_output_override = capability.max_output*4, transition=MaxTokensRetried, continue,assistant_blocks 不入 history
- [loop_maxtokens_second] 重试后仍 MaxTokens -> 接受当前 blocks,push_assistant,继续循环(不再升级)
- [loop_maxtokens_reset_on_success] 成功一轮后 max_output_override 复位为 None

### 4.7 Cancel 三场景的 history 修复

- [loop_cancel_in_stream] cancel 在 stream 中触发 -> assistant_blocks 丢弃,history 不变,emit Cancelled
- [loop_cancel_in_approval] cancel 在 ApprovalCallback await 期间 -> tool 未执行,history 不变,emit Cancelled
- [loop_cancel_in_tool_exec] cancel 在 tool 并发执行中 -> partial_results(已完成 + cancelled 占位)push_tool_results,emit Cancelled
- [loop_cancel_finalize_emits_end] 流中 cancel 路径补 AssistantMessageEnd 配对(sink 完整性)
- [loop_cancel_at_entry] step 0 入口 cancel 已触发 -> 直接 finalize,history 不变

### 4.8 AllRejected 路径

- [loop_all_rejected] 2 个 ToolUse 全部 callback 返回 false -> AllRejected,push 2 条 ToolResult(is_error=true,text="user rejected this tool call"),回模型
- [loop_all_rejected_event] emit ApprovalRejected 事件 N 次,无 ApprovalGranted

### 4.9 部分 approve 部分 reject

- [loop_partial_approve] 3 个 ToolUse,c1=approve, c2=reject, c3=approve -> 执行 c1/c3,c2 直接产 rejection ToolResult
- [loop_partial_order] 最终 push_tool_results 顺序 == 原 ToolUse 顺序 [c1, c2, c3](merge_in_call_order 校验)
- [loop_partial_history_pair] push_tool_results 不抛 ToolResultMismatch(集合相等)

### 4.10 未知工具

- [loop_unknown_tool_no_panic] ToolUse{name="ghost"} -> ToolPhase 直接生成 failure ToolResult,不 panic
- [loop_unknown_tool_event] emit ToolError{error:"unknown tool: ghost"}
- [loop_unknown_tool_history] push_tool_results 成功(call_id 集合配对),回模型继续

### 4.11 Compaction 失败降级

- [loop_compaction_fail_skip] maybe_compact 返回 Err -> emit Error event,transition 不变,继续本轮(history 不变)
- [loop_compaction_fail_keep_old_boundary] 失败前的 compactions 数组未 push 新元素,旧 last() 仍是上次的
- [loop_compaction_fail_no_overflow_path] 这条路径不进入 OverflowRetried 状态(区别于 ContextOverflow 兜底)

---

## 5. Provider 矩阵(§11.5)

### 5.1 SSE 累积

- [sse_accumulate_args] 同 index=0 多次 ToolUseDelta(args_delta="{\"a\":") + ("1}") -> 解析后 ToolUseComplete{input:{"a":1}}
- [sse_id_name_only_first] 仅首个 delta 携带 id/name,后续 delta 仅 args_delta -> 累积器内 id/name 不被覆盖
- [sse_multiple_indices] index=0 与 index=1 并行累积 -> 各自独立 ToolUseComplete
- [sse_args_invalid_json] args 累积完毕但 JSON 解析失败 -> ToolUseComplete{input:{}} 兜底
- [sse_text_delta_passthrough] delta.content="hi" -> ProviderEvent::TextDelta("hi") 直接 emit

### 5.2 finish_reason 翻译表

- [finish_stop] finish_reason="stop" -> StopReason::EndTurn
- [finish_tool_calls] finish_reason="tool_calls" -> StopReason::ToolUse
- [finish_length] finish_reason="length" -> StopReason::MaxTokens
- [finish_unknown] finish_reason="content_filter" -> StopReason::Other
- [finish_missing] finish_reason 字段缺失 + [DONE] 行 -> 补 Done{Other}

### 5.3 ContextOverflow 错误识别

- [overflow_by_code] HTTP 400 body.error.code="context_length_exceeded" -> ProviderError::ContextOverflow
- [overflow_by_message_fallback] HTTP 400 body 无 code,message 含 "maximum context length" -> ContextOverflow
- [overflow_by_message_too_long] message 含 "context length" / "too long" -> ContextOverflow
- [invalid_request_400_other] HTTP 400 但 message 与 overflow 关键词无关 -> InvalidRequest(不要错认成 Overflow)
- [overflow_in_stream_chunk] 流中途收到含 "context length" 的错误 chunk -> 流元素 Err(ContextOverflow)

### 5.4 complete_once 强制 tools=[]

- [complete_once_strips_tools] 入参 prompt.tools=[bash, read] -> 实际请求 body.tools=[]
- [complete_once_collects_text] 流中 TextDelta x N -> 返回拼接字符串
- [complete_once_ignores_tooluse] 即便流中混入 ToolUseComplete(理论不应发生),不进结果
- [complete_once_propagates_provider_error] stream() 返回 Err(Auth) -> complete_once 直接上抛 Auth

### 5.5 流中途 5xx

- [stream_5xx_mid] stream 已 emit 几个 TextDelta 后,upstream 返回 502 -> 流元素 Err(Transient("stream interrupted: ..."))
- [stream_network_drop] reqwest 连接断开 -> 流元素 Err(Transient(...))
- [stream_sse_parse_error] data: 行 JSON 解析失败 -> 流元素 Err(Transient(...))
- [stream_initial_5xx] stream() 第一次握手就 5xx -> 外层 Err(Transient),不进入流

---

## 备注

- 测试侧重不变量(§8 / §10.10 / §11.7 / §12.12 Cheat Sheet),用例命名建议与不变量一一对应,便于回归审计
- Loop 测试需要 MockProvider(可控 stream 元素序列)+ MockTool(可控 ToolOutput / 延迟 / 取消响应)
- Provider 测试需要 mock HTTP 服务器(wiremock 或同等)模拟 SSE 与 4xx/5xx 响应体
- Compaction 测试不依赖真 LLM,run_summary 通过 MockProvider.complete_once 桩出固定字符串
