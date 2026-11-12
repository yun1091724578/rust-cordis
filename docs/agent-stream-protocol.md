# Agent 流协议与消费层设计

- 状态：提议；仓外 `desolate` 已落地第一版事件记录器
- 日期：2026-11-04
- 层级：agent/harness 消费层，**不是** `cordis-core` 的新内核机制，也不进入首发支持契约
- 代码位置：`../desolate/host/src/agent_stream.rs`、`../desolate/host/src/agent.rs`
- 证据：`../desolate/AGENT-REPORT.md` 与 `reports/agent-report*.events.jsonl`

## 1. 调研结论

真实的 agent/模型流协议虽然传输格式不同，语义层高度一致：

| 协议 | 关键事件 | 对本项目的结论 |
|---|---|---|
| OpenAI Responses | `response.created`、`response.output_text.delta`、`response.output_text.done`、`response.completed`、`response.failed`、`error`、`sequence_number` | 语义事件 + 终止事件；`200` 之后仍可能出错；按 `sequence_number` 排序。[^openai-stream] |
| OpenAI Agents SDK | raw response events、`run_item_stream_event`（`tool_called`、`tool_output` 等）、`cancel()` | 底层 token 事件与高层 agent 事件分层；取消不是“回滚”。[^openai-agents] |
| Anthropic Messages | `message_start`、`content_block_start/delta/stop`、`message_delta`、`message_stop`、`ping`、`error` | 内容块有显式起止；`error` 是终止事件，之后没有 `message_stop`；未知事件要忽略。[^anthropic] |
| AG-UI | `RUN_STARTED`、`TEXT_MESSAGE_START/CONTENT/END`、`TOOL_CALL_START/ARGS/END/RESULT`、`RUN_FINISHED` 或 `RUN_ERROR` | 一次 run 必须由 `RunStarted` 开始，以 `RunFinished` 或 `RunError` 结束；HTTP 200 不代表成功。[^agui-events] [^agui-sse] |
| Vercel AI SDK | `start`、`text-start/delta/end`、`error`、`tool-input-*`、`tool-output-*`、`finish`、`abort`、`[DONE]`、`reset-step` | SSE 传输；文本/工具都有 start/delta/end；`reset-step` 可显式作废失败步骤的部分输出。[^vercel] |
| LangGraph event streaming | `ProtocolEvent { seq, method, namespace, timestamp, data }`；message 的 `start/delta/finish`、`tools` 的 `tool-started/output-delta/finished/error`、`lifecycle` | `seq` 用于排序，`timestamp` 不用于排序；内容块和工具都有边界。[^langgraph] |
| Google ADK | `partial`、`turn_complete`、`error_code/error_message` | 有的提供方同时给增量块与最终合并块；错误可能出现在流中。[^adk] |
| A2A | `TaskStatusUpdateEvent`、`TaskArtifactUpdateEvent`、`append`/`lastChunk`、终止状态 | 任务状态和产物分开；终止后不再发事件；断线可重新订阅。[^a2a] |
| MCP Streamable HTTP | 每个请求一个 SSE 流；最终 JSON-RPC 响应终止；2026 修订不再支持 `Last-Event-ID`；注释行可做 keep-alive | 不要假设 SSE 一定可恢复；关闭流可能是取消信号。[^mcp] |
| WHATWG SSE | `event:`、`data:`、`id:`、`retry:`；空行分帧；`Last-Event-ID`；注释行 | 只把 SSE 当传输；`id` 是 cursor，不是业务序号；重连是可选能力。[^sse] |

共同结论：

1. **事件化，而不是裸 token 字节。** 文本、工具、生命周期各有显式事件。
2. **正常受控执行中，生产者最多形成一个终止决定；终止之后不得再发事件。**
   但这不等于“接收方一定收到终止”：进程崩溃、网络断开、sink 写入失败都可能
   让接收方只看到不完整流。没有终止事件时，接收方不得判定成功。
3. **流打开后的错误是 in-band 终止事件。** HTTP 状态已经发出，调用方不能从
   `200` 推断成功。错误事件之后可能没有正常收尾事件。
4. **部分输出是事实，不是错误的一部分。** 已交付的 delta 要保留；终止事件应带
   部分快照。若上层要重试并作废某一步，需要显式的 invalidate/reset 语义。
5. **工具调用有独立生命周期。** start/args/end 与 result 分开；工具可能已经执行
   但调用方没有收到结果，这时只能记录 `unknown`，不能自动重放。
6. **顺序用序号，不用墙钟。** 时间戳可能漂移；SSE `id` 只用于恢复/去重。
7. **重连不是默认能力，而且各协议并不相同。** AG-UI 和 MCP 2026 明确不恢复；
   A2A 用 `SubscribeToTask` 重新订阅；WHATWG `EventSource` 自己还会自动重连。
   SSE 只提供传输，不支持恢复时必须显式定义“断流 = 不完整运行”。
8. **未知事件类型要容忍。** 提供方会加事件；消费者应按类型路由，忽略未知类型。
9. **心跳是传输控制。** SSE 注释行不是业务事件，不占用序号。

不要把“这些协议都有终止事件”扩大成“它们有完全相同的终止规则”。终止事件是共同
形状，恢复、取消、错误是否可重试、部分输出是否可作废，各协议不同。

## 2. 本项目的分层决策

agent 协议放在**仓外消费层**，不放进 `cordis-core`：

- `cordis-core` 的 `AsyncStream` 仍是字节/值流与背压载体；
- `desolate` 负责把模型 delta、工具生命周期和终止状态转成 agent 事件；
- 未来若多个消费者复用，再抽成独立 crate，而不是先改内核。

这与“内核只提供机制，agent 语义属于领域层”的边界一致。但这不是永久禁令：如果
多个消费者都需要结构化的载体级终止原因，或者现有流错误语义无法支撑可靠消费，
再评估通用能力；不能把 `run.finished`、工具状态等 agent 词汇直接塞进 core。

### 两个维度

流终止和业务执行认知必须分开：

| 维度 | 取值 | 回答的问题 |
|---|---|---|
| 为什么停止等待 | `completed` / `failed` / `cancelled` / `timed_out` | 调用方为什么不再等 |
| 对业务执行知道多少 | `not_executed` / `reported` / `unknown` | 目标方法/工具是否执行、结果是否可观察 |

例如，工具已经派发后取消，记录为 `cancelled + unknown`。`tool.finished` 的
`status=cancelled` 只表示调用不再等待，不证明工具没有执行；`certainty` 必须单独
携带。`side_effect_observed=false` 只表示“没有观察到副作用”，不是“没有副作用”。

## 3. 事件信封

```json
{
  "v": "agent-stream/1",
  "run_id": "agent-ok-1234",
  "seq": 3,
  "ts_ms": 1793777575290,
  "type": "text.delta",
  "message_id": "msg-1",
  "delta": "Looking"
}
```

规则：

- `seq` 从 1 开始，单 run 内严格递增；终止事件是最后一个 `seq`。
- `run_id` 在 run 内稳定；未来的 SSE `id` 使用 `{run_id}:{seq}`。
- `ts_ms` 只用于展示/诊断，不参与排序。
- `v` 是协议版本；新增字段保持向后兼容。
- 未知 `type` 的消费者必须忽略该事件并继续；未知字段忽略。

## 4. 事件类型

| 事件 | 关键字段 | 规则 |
|---|---|---|
| `run.started` | `request` | 每个 run 的第一个事件，恰好一次 |
| `text.started` | `message_id` | 一个文本块开始；同一时刻最多一个打开的文本块 |
| `text.delta` | `message_id`, `delta` | 只追加；`message_id` 必须匹配打开块 |
| `text.finished` | `message_id` | 文本块正常结束；失败时也要关闭已打开的块 |
| `tool.started` | `tool_call_id`, `name`, `arguments` | 工具调用开始；id 在 run 内唯一 |
| `tool.finished` | `tool_call_id`, `name`, `status`, `certainty`, `result`, `error`, `side_effect_observed` | 恰好一次；`status` 可为 `ok/error/unknown/cancelled`；`status` 不能是 `running` |
| `run.finished` | `status`, `certainty`, `error`, `partial`, `usage` | 生产者在受控执行中最多形成一次终止事件；`status` 为 `completed/failed/cancelled/timed_out` |

`run.finished` 的 `partial`：

```json
{
  "messages": [
    {
      "message_id": "msg-1",
      "text": "before tool",
      "status": "finished"
    },
    {
      "message_id": "msg-2",
      "text": "after tool",
      "status": "open"
    }
  ],
  "tools": [
    {
      "tool_call_id": "tool-1",
      "name": "archive.search",
      "status": "unknown",
      "certainty": "unknown",
      "side_effect_observed": true
    }
  ]
}
```

终止事件仍携带部分快照，因为“失败”不抹掉已经交付的 delta；`partial` 是调用方
恢复/展示/记账的依据。部分快照按消息/内容块分别保存，不能把两条消息的文本挂在
最后一个 message ID 下。第一版不做 `reset-step`；如果以后重试模型步骤，必须新增
显式 invalidate 事件，不能静默删除已交付的 delta。

终止一致性规则：

- `completed` 要求没有打开的文本块、没有 `running` 工具；否则拒绝终止。
- `failed` / `timed_out` / `cancelled` 会关闭打开的文本块，并把仍在运行的工具
  收尾为 `unknown`（失败/超时）或 `cancelled`（取消），`certainty=unknown`、
  `side_effect_observed=false`。
- `tool.finished` 不接受 `running`，也不允许同一工具结算两次。
- 终止后拒绝任何新事件；接收方没有收到终止事件时，必须把运行视为不完整。

## 5. 错误与工具未知

统一错误对象沿用 ABI 的字段词汇：

```json
{
  "message": "host-side tool deadline exceeded",
  "code": "HOST_TIMEOUT",
  "category": "timeout",
  "certainty": "unknown",
  "retry": "unknown"
}
```

规则：

- 模型流在部分输出后失败：先 `text.finished`（关闭块），再
  `run.finished(status=failed, certainty=unknown, error=..., partial.messages=已交付文本)`。
- 工具在宿主 deadline 到期时：先 `tool.finished(status=unknown, certainty=unknown,
  side_effect_observed=...)`，再 `run.finished(status=timed_out, certainty=unknown,
  error=HOST_TIMEOUT, partial.tools=...)`。
- 工具错误：`tool.finished(status=error, certainty=reported, error=...)`，然后 run
  终止。`reported` 表示工具的结果被观察到，不表示没有副作用。
- 取消：已打开的工具以 `cancelled` 结束，`certainty=unknown`，run 以 `cancelled`
  结束；不声称副作用已撤销。
- 任何路径都不自动重试；策略层要结合副作用、幂等、预算和取消状态再决定。
- `side_effect_observed=false` 只能说明“没有观察到副作用”，不能证明没有副作用。

## 6. 传输映射

### JSONL（当前）

一行一个完整事件；用于本地证据和调试。当前 sink 在**每个事件 emit 时**立即
序列化、检查大小并写入/flush，不再等 run 结束一次性写。超限事件在写文件前
拒绝，不会留下半个 JSON 行。文件位置：`reports/agent-report*.events.jsonl`。

有界配置（`StreamLimits`）：

```text
maxEventBytes = 64 KiB
maxEvents     = 10_000
maxTextBytes  = 256 KiB（单消息与总保留文本）
maxTools      = 1_024
```

0 值在打开 sink 前拒绝。

### SSE（下一传输）

```text
id: {run_id}:{seq}
event: {type}
data: {完整事件 JSON}

```

- `id` 是 cursor，不是业务序号；第一版不启用 `Last-Event-ID` replay。
- keep-alive 用 SSE 注释行（`: keep-alive`），不产生业务事件、不占 `seq`。
- 终止事件后关闭响应体；断流但没有终止事件 = truncated run，调用方不能当成功。
- 未来需要 resume 时，增加事件存储 + `Last-Event-ID`；不能只靠 `id` 假装可恢复。

## 7. 已落地与验证

`desolate/host/src/agent_stream.rs` 已有：

- 类型化 `AgentEvent` / `AgentEventKind` / `PartialSnapshot` / `EventError`；
- 单 run 的 `AgentStreamRecorder`：序号、多消息快照、工具生命周期、终止一致性；
- JSONL 写入与 SSE 帧编码；
- 18 个 agent_stream 单测：序号单调、completed 拒绝打开文本/运行中工具、失败/
  取消自动收尾、多消息快照、工具不可重复/不可 running、非法 certainty、终止后
  拒绝事件、SSE 形状；新增增量 sink 可见性、无重复行、超大事件无半行、事件/
  文本/工具上限、0 值拒绝、sink 路径错误；
- 另有 2 个 agent 分类回归：普通模式真实超时仍是 `timed_out/unknown`，`Ok(Err)`
  读取实际错误证据、无证据保持 `unknown`。

`agent` 六模式已接入：

- `ok`：3 个 token → 工具结果 → `run.finished(completed, reported)`；
- `stream_fail`：1 个已交付 token → 文本错误 → `run.finished(failed, unknown)`；
- `tool_timeout`：工具写出结果后挂起 → 实际 `Err(elapsed)` →
  `tool.finished(unknown, unknown)` → `run.finished(timed_out, unknown)`；
- `normal_timeout`：同样的实际超时，但 mode 名不是 timeout；仍记录
  `timed_out/unknown`，证明分类来自结果而不是模式；
- `tool_error`：工具方法已进入、尚未向下游 `desolate.index` 派发就报错 →
  `tool.finished(error, reported)` → `run.finished(failed, reported)`；
- `model_start_error`：模型适配器同步拒绝 → `run.finished(failed, unknown)`；
  所有模式都显式释放插件并关闭 App。

工具结果按实际分支解释，不看 mode 名：

| 结果 | 处理 |
|---|---|
| `Ok(Ok(value))` | 提供者返回成功，`reported` |
| `Ok(Err(error))` | 优先读取工具写出的错误证据；无证据时 `unknown`，不默认 `reported` |
| `Err(elapsed)` | 宿主等待超时，`timed_out/unknown` |

测试模式只负责制造故障；它不能决定分类。`Ok(Err)` 只能说明顶层方法返回了
错误，不能据此声称目标下游方法未执行。

## 8. 下一步（按批次）

1. **状态机与收尾修正（本轮已完成）**：统一异常收尾、多消息快照、非法终止拒绝、
   工具执行认知、取消/失败自动收尾。
2. **有界增量交付（已完成基础版）**：事件大小、累计输出、工具数量、事件数量上限，
   JSONL emit 即写；剩余慢消费者队列、无终止事件 EOF、取消与 sink 写入竞争。
   SSE 编解码可以在这里做，但网络服务不急。
3. **一个真实模型适配器**：验证文本、工具参数增量、正常终止、流内错误和意外
   EOF；先支持一个提供方，不急着抽象所有协议。
4. **取消与预算贯穿**：模型等待、工具执行、sink 阻塞；区分首包、空闲和总运行
   超时；未知结果不自动重试。

达到“面对非预设错误、慢消费者和取消，链路仍能给出可信结果并完成清理”后，再
决定 rust-cordis 需要增加哪些通用机制。当前 18 个 agent_stream 单测、2 个分类
回归和六模式证明的是有界 JSONL 交付基础，不是在线可靠交付：同步文件 sink 没有
队列/背压，无终止事件 EOF 仍由调用方判为不完整，取消与 sink 写入竞争未覆盖。

## 参考

[^openai-stream]: OpenAI, “Streaming API responses”, https://developers.openai.com/api/docs/guides/streaming-responses ; event reference: https://developers.openai.com/api/reference/resources/responses/streaming-events
[^openai-agents]: OpenAI Agents SDK, “Streaming”, https://openai.github.io/openai-agents-python/streaming/
[^anthropic]: Anthropic, “Streaming messages”, https://platform.claude.com/docs/en/build-with-claude/streaming
[^agui-events]: AG-UI, “Events”, https://docs.ag-ui.com/concepts/events
[^agui-sse]: AG-UI, “HTTP + Server-Sent Events”, https://docs.ag-ui.com/spec/1.0/basic/transports/http-sse
[^vercel]: Vercel AI SDK, “Stream Protocols”, https://ai-sdk.dev/docs/ai-sdk-ui/stream-protocol
[^langgraph]: LangChain, “Event streaming”, https://docs.langchain.com/oss/python/langgraph/event-streaming
[^adk]: Google ADK, “Events”, https://adk.dev/events/
[^a2a]: A2A Protocol, “Streaming & Asynchronous Operations”, https://a2a-protocol.org/latest/topics/streaming-and-async/
[^mcp]: Model Context Protocol, “Streamable HTTP”, https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http
[^sse]: WHATWG, “Server-sent events”, https://html.spec.whatwg.org/multipage/server-sent-events.html
