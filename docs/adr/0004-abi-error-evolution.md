# ADR 0004：错误的双向传输与兼容演进

- **状态**：已接受（阶段 2 完成，含已知边界：线格式/能力协商/详细入口/有界降级；异步详细通道、真实旧 ABI 错误、卸载/取消原因与 provider 业务错误已落地。兼容矩阵只有 2 格真端到端，另外 2 格是明确标注的降级分支桩；详细入口的同步失败仍返回 `String`，宿主预解析失败仍走文本）
- **日期**：2026-10-31
- **依据**：`crates/abi/src/lib.rs`（`PluginDesc` / `HostApi` / `STATUS_*`）、
  `crates/abi/src/error.rs`（`ErrorPayload`）、`crates/abi/src/sdk.rs`、
  `crates/host/src/lib.rs`、`docs/adr/0003-result-and-error-contract.md`

## 1. 现状

阶段 1 的两个方向都只有文本：

| 方向 | 载体 | 阶段 1 形态 |
|---|---|---|
| 插件 → 宿主 | `STATUS_ERR` + out blob | 宿主按 `String::from_utf8_lossy` 读取 |
| 宿主 → 插件（宿主函数失败） | `STATUS_ERR` + out blob | 字符串；插件自己解释 |
| 宿主 → 插件（出站调用失败） | `on_done` 的 `error` 字段 | 字符串 |

结果是 `ResolveError` 与 `IfaceVersionMismatch` 跨 ABI 后都变成文本；
宿主无法可靠区分“版本过高”“参数形状错误”“业务错误”“超时”。

## 2. 决定

1. **保留** `STATUS_ERR` 与人类可读消息。不新增错误状态码。
2. 结构化错误是**可选层**：能力为 0 时完全走旧文本行为。
3. 能力位放在两侧已有的尾追加槽，按 `size` 判界：
   - `PluginDesc.error_format: u32`（位于 `provision` 之后）；
     `0` = 只支持文本，`ERROR_FORMAT_V1` = **读写双向**：插件会用
     [`ErrorPayload`] 写 `STATUS_ERR`，并接受宿主按同一能力位发来的结构化错误；
     手写插件若只想发不想收，必须保持 `0` 或自行解析宿主载荷；
   - `PluginDesc.tail_reserved: u32` 紧跟其后，占住结构体尾部填充；当前必须为 0，
     后续字段只能从 96B 边界追加；
   - `HostApi.error_format: u32`（位于 `ctx_stream_fail` 之后）；
     `0` = 只支持文本，`ERROR_FORMAT_V1` = 宿主会写结构化错误，并解析插件的结构化错误；
   - `HostApi.reserved: u32` 紧跟其后，占住结构体尾部填充；当前必须为 0，
     后续字段只能从下一个 8 字节边界追加；
   - `ERROR_FORMAT_V1 = 1`。
   - 宿主→插件的结构化写入实际要求 `host_bit && plugin_bit`；SDK 生成的插件
     两向都支持，手写插件必须显式遵守上面的双向约定。
   - `PluginDesc::error_format_at(desc)` / `HostApi::error_format()` 是唯一的
     读取入口；`PluginDesc` 用裸指针读取，避免对 72B v1 分配形成 96B `&PluginDesc`。
     `HostApi::field_at` 先按 `size` 判界再 `read_unaligned`，不先读字段值；
     `HostApi` 的引用必须覆盖完整结构（`sdk::attach` 的 copy-on-attach 负责）。
     能力位是 4 字节，不是 `Option<u32>` 的 8 字节；尺寸判界测试覆盖“恰好够 / 差一字节”。
4. **线格式**（`STATUS_ERR` 的 out blob）是一个 JSON 对象：
   ```json
   {"error":"人类可读消息","code":"E_X","category":"business",
    "certainty":"unknown","retry":"unknown","detail":null}
   ```
   - `error` 必须是字符串；它是旧宿主/旧插件也能读到的人类可读消息；
   - `code` / `category` / `certainty` / `retry` 是开放字符串，本层只传输；
   - 缺字段默认 `"unknown"`，未知字段忽略，未知码原样保留；
   - 结构化字段是 `error` 的兄弟，不把 `error` 改成对象。
5. **兼容边界**：旧字符串 API（`Ctx::require` / `Ctx::set` / `call_plugin` /
   `Ctx::call_iface` / `Ctx::dispatch` / `Ctx::next`）只取 `.error`；详细入口
   `Ctx::require_detailed` / `Ctx::set_detailed` / `call_plugin_detailed` /
   `Ctx::call_iface_detailed` / `Ctx::dispatch_detailed` / `Ctx::next_detailed`
   保留完整 `ErrorPayload`。内部先保留载荷，旧 API 在边界降级。
6. 解析失败时保留原文，标为未知码；**不因为错误解析失败而改变调用结果**。
7. **本阶段范围**：两个 `STATUS_ERR` out-blob 方向，以及异步 `on_done` /
   `args[0].error`。异步回调始终保留 `error` 字符串；插件声明能力位时追加
   `code/category/certainty/retry/detail` 兄弟字段。旧 SDK 忽略未知字段，新 SDK
   通过 `*_detailed` 读取完整载荷；超时使用 `code="HOST_TIMEOUT"`、`category="timeout"`、
   `certainty="unknown"`。旧宿主/旧插件仍只看到文本。
   `apply_via` 是文本错误的 raw seam；知道 `PluginDesc` 的无 DLL 调用方必须用
   `apply_via_with_error_format`，`LoadedPlugin::apply` 自动传 desc 的能力位。

## 3. 兼容矩阵

| 组合 | 必须行为 | 当前落点 |
|---|---|---|
| 旧宿主 + 旧插件 | 文本，基线不变 | 两边 `error_format` 缺失/0 |
| 新宿主 + 旧插件 | 接受文本；缺失结构按 `Unknown` 处理，不得当成“无错误” | 宿主 `Instance.error_format = 0` |
| 旧宿主 + 新插件 | 插件读到能力 0 ⇒ **只发文本**；消息在旧位置 | `write_error` 的 `error_format < 1` 分支 |
| 新宿主 + 新插件 | 协商后发结构；任一侧解析失败 ⇒ 回退文本 | `decode_plugin_error` / `decode_host_error` |
| 手写插件（不读能力） | 始终文本；宿主按 `Unknown` 处理 | `abi_matrix` 的手写 v1 夹具 |

兼容矩阵与现有证据（**不是四格端到端**）：

| 组合 | 证据 | 边界 |
|---|---|---|
| 旧插件 × 新宿主 | `crates/host/tests/legacy_error.rs::legacy_plugin_error_is_text_and_unknown_not_not_executed` | 真 72B v1 cdylib；`code/category/certainty=unknown`，不是 `not_executed` |
| 旧插件 × 旧宿主桩 | ——（同一文本解码路径；`call_plugin_detailed(..., error_format=0)` 是**插件**能力位，不是宿主能力位） | **无独立证据**；不能算第二格 |
| 新插件 × 新宿主 | `crates/host/tests/sdk_detailed_timeout.rs::call_iface_detailed_receives_timeout_payload_from_the_real_host`；`sdk_iface_unload_detailed.rs`；`sdk_provider_error_detailed.rs`；`apply_error_format.rs` | 真宿主 + 真 cdylib / 真 SDK 插件 |
| 新插件 × 旧宿主桩 | `crates/abi/src/sdk.rs::text_only_host_gets_plain_text`（写侧单测）；`crates/abi/src/sdk.rs::text_host_errors_survive_without_capability`（读侧单测） | **桩**：截断 `HostApi.size` 到 `error_format` 之前；没有真旧宿主进程，也没有真宿主写侧端到端 |

关键格是“旧宿主 + 新插件”：旧宿主把 out blob 当 UTF-8 文本读，对象载荷会
退化成一段 JSON 文本——**损失结构但不致命**。因此新插件在没有能力位时只发
纯文本；结构化放不下时也回退到纯文本，消息不丢。

宿主函数的**预解析失败**（例如非法 JSON、未解析到实例）仍走 `fail()` 文本；
这些路径没有插件能力位可查，**阶段 2 没有统一**，留到阶段 3/4 的调用事实通道。
`with_resolved` / `with_instance` 覆盖的失败才按能力位发结构。

宿主函数包装层的默认失败目前仍是 `code = "host_call"`、`category = "protocol"`
这个**传输哨兵**；阶段 2 已把 `ctx_require` 标成 `host_require / resolve`，
`ctx_fulfill` 标成 `host_fulfill / lifecycle`，`ctx_abort` 标成
`host_abort / lifecycle`，`ctx_next` 标成 `host_next / lifecycle`。
其余入口的最终分类留到阶段 3/4，消费方不得把哨兵当最终分类依据。

内联 `ctx_call_iface` / `ctx_dispatch` 的结果同样受 16 KiB 初始缓冲限制：
超过时返回 `STATUS_TOO_SMALL`，SDK 报 `host_too_small`，**不重放、也不伪装成功**。

## 4. 解析规则

1. 能力缺失或为 0 ⇒ 不尝试解析结构。
2. 结构解析失败 ⇒ 保留原始文本，`code = unknown`，**不覆盖**已有消息。
3. 未知错误码原样保留，不解释成成功。
4. **不得为了拿到更大的错误载荷而重放有副作用的调用。**
   `invoke_raw` 的 `retry=false` 规则保持；本 ADR 没有新增重试路径。
5. 错误载荷过大时截断结构，保留消息：`write_error` / `fail_to` 先尝试结构化，
   `Out::write` 返回 `STATUS_TOO_SMALL` 时退回纯文本消息；纯文本仍超过 `cap`
   时按 UTF-8 字符边界截断并尽量带 `…` 标记，绝不回填 `len > cap`。
   若插件/宿主违反 `Out` 契约直接回填 `len > cap`，宿主/插件侧给出有界诊断，
   不把零缓冲当消息。
6. 内联 `STATUS_OK` 信封必须是 JSON 对象；非法 JSON、非对象、非布尔
   `pending`、非字符串 `error` ⇒ 续延以**错误**结算，不降级为 `Ok(null)`。
   这条同时覆盖 `call_iface` / `dispatch` 的同步快速路径和 `OP_CALLBACK` 续延。
7. 真实实现：`crates/abi/src/error.rs` 的 `ErrorPayload::{to_json, from_json}`；
   宿主 `decode_plugin_error` / `fail_to` / `call_plugin_detailed` /
   `CallFailure` / `error_arg`；插件 `write_error` / `decode_host_error` /
   `require_detailed` / `set_detailed` / `call_iface_detailed` /
   `dispatch_detailed` / `next_detailed`；
   `LoadedPlugin` / `apply_via_with_error_format` 负责把能力位带进 `Instance`。

## 5. 公开 Rust API 的兼容检查

`cordis-host` / `cordis-core` / `cordis-abi` / `cordis-macros` 都没有
`publish = false`，本来就是 semver 约束；只有 `cordis-sdk`、`cordis-components`
等才是。以下类型今天都是公开的、没有 `#[non_exhaustive]`：

| 类型 | 位置 | 加字段/变体的后果 |
|---|---|---|
| `Capabilities` | `crates/host/src/lib.rs` | 外部 struct 字面量编译失败 |
| `FiberState` | `crates/core/src/fiber.rs` | 外部穷举 `match` 编译失败 |
| `DispatchPolicy` | `crates/host/src/lib.rs` | 外部构造编译失败 |
| `PluginDesc` | `crates/abi/src/lib.rs` | 外部 struct 字面量必须补 `provision` / `error_format` / `tail_reserved`；v1 二进制不受影响，Rust 源码级构造是 0.x 的已知破坏 |
| `HostApi` | `crates/abi/src/lib.rs` | 外部 struct 字面量必须补 `error_format` / `reserved`；旧宿主二进制不受影响，Rust 源码级构造是 0.x 的已知破坏 |
| `ErrorPayload` | `crates/abi/src/error.rs` | 已加 `#[non_exhaustive]`；外部用 `new` / `with_*` 构造，后续可尾加字段 |

阶段 2 如果要给 `Capabilities` 加 schema 或错误能力，优先**加方法**而不是加字段；
加 `#[non_exhaustive]` 本身也是破坏性变更，需要单独决策。本 ADR 只给引入的
`ErrorPayload` 加了 `#[non_exhaustive]`；`PluginDesc` / `HostApi` 的尾追加字段
是 ABI 兼容的，但会让外部 Rust struct 字面量编译失败。

阶段 3 给 `Capabilities` 加了**私有** `schema` 字段和 `schema()` / `origin()` 方法。
这是有意的 0.x 源级破坏：外部 `Capabilities { ... }` 字面量会失效；没有新增公开字段，
也没有加 `#[non_exhaustive]`。如果未来要恢复字面量兼容，应提供构造器/`Default` 迁移。

## 6. 验收

- [x] 能力位尺寸边界：`crates/abi/tests/error_format.rs` 的
  `plugin_desc_error_format_uses_the_size_gate` /
  `host_error_format_uses_the_size_gate`（恰好够 / 差一字节）；
  `host_field_at_checks_size_before_reading` 钉住“先判界、后读字段”。
- [x] 编解码边界：非法 JSON、缺 `error`、非字符串 `error`、未知码、空消息、
  未知字段、默认 `unknown`（`error_format.rs`）。
- [x] 内联信封边界：`crates/abi/src/sdk.rs` 的
  `malformed_inline_response_is_not_a_success` /
  `non_object_inline_response_is_not_a_success` /
  `non_string_error_field_is_not_a_success` /
  `non_object_callback_payload_is_not_a_success` /
  `pending_inline_response_keeps_the_continuation`。
- [x] `dispatch` / `dispatch_detailed` 的预解析失败必须结算续延：
  `dispatch_settles_text_continuation_on_malformed_inline_json` /
  `dispatch_settles_detailed_continuation_on_malformed_inline_json`
  （不是只 `forget`）。
- [x] 预解析失败：`crates/host/src/lib.rs::malformed_host_arguments_are_protocol_errors_not_null`
  （`ctx_use` / `ctx_call_iface` / `ctx_next` / `dispatch` 的非法 JSON 不再降级成 `null`）。
- [x] 插件 → 宿主：`write_error` 的结构化/纯文本/放不下回退；
  `crates/host/src/lib.rs::call_op_decodes_plugin_errors_only_with_capability`；
  `crates/host/src/lib.rs::call_plugin_detailed_distinguishes_codes_with_the_same_message`
  （同消息异 code）；`crates/host/tests/sdk_apply_failure.rs::failed_apply_releases_sdk_callbacks_registered_before_the_error`
  （apply 失败）与
  `crates/host/tests/sdk_callback_error.rs::structured_callback_error_is_readable_through_apply_via`
  （回调失败，`apply_via_with_error_format`）。
- [x] 宿主 → 插件：`crates/abi/src/sdk.rs::structured_host_errors_are_decoded`
  与 `text_host_errors_survive_without_capability`；`fail_to` 的两格单测；
  `require_detailed_distinguishes_codes_with_the_same_message` /
  `set_detailed_distinguishes_codes_with_the_same_message`（详细入口）。
  生产 `HostApi` 能力位与真实 require 路径由
  `crates/host/tests/sdk_apply_failure.rs::failed_apply_releases_sdk_callbacks_registered_before_the_error`
  覆盖（断言 `host_require / resolve`）。
- [x] 内联结果上限：`crates/host/tests/sdk_async_iface.rs::suspendable_iface_methods_share_everything_with_the_event_path`
  覆盖 20k 结果 → `STATUS_TOO_SMALL` 可读错误、不重放、无残留挂起。
- [x] 有界降级：SDK/宿主各覆盖 20k→16k、中文 UTF-8 边界、极小缓冲；
  `out.len` 永不超 `cap`，`out.len > cap` 时给有界诊断而不是 NUL 串。
- [x] **兼容矩阵分流分支**的单测（旧/旧、旧/新、新/旧、新/新）；真实端到端证据见 §3，只有 2 格。
- [x] 手写 v1 夹具（`abi_matrix`）的 `error_format` 缺失读取 + 矩阵探针。
- [x] 真实旧插件返回错误端到端：
  `crates/host/tests/legacy_error.rs::legacy_plugin_error_is_text_and_unknown_not_not_executed`
  （独立 `tests/fixtures/abi-err-v1`，不改冻结的 `abi-v1`；文本保留，
  `certainty=unknown` 而非 `not_executed`）。
- [x] 异步详细通道：`crates/abi/src/sdk.rs` 的
  `detailed_continuation_preserves_error_siblings` /
  `text_continuation_still_reads_only_the_error_string`；
  `crates/host/tests/sdk_detailed_timeout.rs::call_iface_detailed_receives_timeout_payload_from_the_real_host`
  覆盖 `call_iface_detailed` 的真实超时载荷；
  `next_detailed_delivers_a_structured_error_payload` 直接覆盖 `next_detailed` 的续延；
  `sdk_provider_error_detailed` 覆盖 provider 业务错误；
  `sdk_iface_unload_detailed` 覆盖卸载时的 `host_lifecycle/lifecycle/unknown`。
  `dispatch_detailed` 与 `next_detailed` 仍是同一续延/解码路径，没有独立的真宿主
  `dispatch_detailed` 回调用例；这是已知边界，不写成“全部端到端”。
- [x] 超时通道端到端：`abi_outbound_timeout` / `abi_outbound_dispatch_timeout` /
  `abi_next_timeout` 均断言可读原因与 `code=HOST_TIMEOUT`、`category=timeout`、
  `certainty=unknown`；事件日志路径仍是已知例外。
- [x] 旧宿主桩明确标注：`text_host_errors_survive_without_capability` 用截断
  `HostApi.size` 模拟能力 0（读侧）；`text_only_host_gets_plain_text` 覆盖写侧单测。
  `legacy_error` 的 `error_format=0` 是**插件**能力位，不是旧宿主，不能算第二格。
  ADR 与测试注释均写明是**桩**，不冒充实旧宿主进程。

## 7. 未决问题

1. 字段名与偏移：已在 §2 钉死；后续只能尾追加。
2. `error` 对象还是兄弟：已选“`error` 字符串 + 兄弟字段”。
3. `certainty` / `retry` 如何编码给手写插件：本阶段写 `"unknown"`；
   适配器的 `not_executed` 事实仍未填，见 ADR 0003 §2。
4. 是否为“派发前拒绝”提供宿主→插件的结构化 `NotExecuted` 信号：
   本 ADR 不做；留给调用事实通道（阶段 4）。
5. 结构化字段目前只被解码、尚未参与分类或重试判断；A3/A4 已提供
   `require_detailed` / `set_detailed` / `call_plugin_detailed` /
   `call_iface_detailed` / `dispatch_detailed` / `next_detailed` 让调用者读取
   `code/category/...`，但“按 `certainty`/`retry` 做决策”的消费者留到后续。
6. 挂起的 provider 目前只能 `fulfill(value)`，不能通过异步通道提交
   `ErrorPayload`；异步业务错误需要新的 `fulfill_error` 语义，留到阶段 3/4。
   当前异步失败的可证明来源是宿主超时、取消、卸载与协议错误。
7. `call_iface_detailed` / `dispatch_detailed` / `next_detailed` 的**同步**
   失败仍返回 `String`（回调内的失败才是 `ErrorPayload`）；统一为结构化返回值
   需要新的公开 API 形状，留到阶段 3/4。
