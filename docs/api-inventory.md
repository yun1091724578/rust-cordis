# 契约公开面 / 状态码 / 载荷清单（受控子集）

> 由 `cargo xtask inventory --write` 生成；`cargo xtask inventory --check` 在 CI 比对。
> 范围：ABI `lib.rs` 的顶层常量/`repr(C)` 结构 + 宿主/核心/SDK 的受控入口；`ErrorPayload` 字段见 `crates/abi/src/error.rs`。**不是**所有 crate 的 `pub`。
> ABI 常量/字段自动扫描；宿主入口与载荷是受控清单，新增/删除必须同步本文件。

## ABI 常量

| 名称 | 类型 | 值 | 位置 |
|---|---|---|---|
| `ABI_VERSION` | `u32` | `1` | `crates/abi/src/lib.rs` |
| `ERROR_FORMAT_V1` | `u32` | `1` | `crates/abi/src/lib.rs` |
| `MAX_CONFIG_SCHEMA_BYTES` | `usize` | `1024 * 1024` | `crates/abi/src/lib.rs` |
| `ENTRY_SYMBOL` | `&[u8]` | `b"cordis_plugin_v1\0"` | `crates/abi/src/lib.rs` |
| `OP_APPLY` | `u32` | `1` | `crates/abi/src/lib.rs` |
| `OP_DISPOSE` | `u32` | `2` | `crates/abi/src/lib.rs` |
| `OP_CALLBACK` | `u32` | `3` | `crates/abi/src/lib.rs` |
| `OP_CLEANUP` | `u32` | `4` | `crates/abi/src/lib.rs` |
| `OP_PING` | `u32` | `5` | `crates/abi/src/lib.rs` |
| `OP_VOLATILE_PATHS` | `u32` | `6` | `crates/abi/src/lib.rs` |
| `OP_VOLATILE_APPLY` | `u32` | `7` | `crates/abi/src/lib.rs` |
| `KIND_EVENT` | `&str` | `"event"` | `crates/abi/src/lib.rs` |
| `KIND_IFACE` | `&str` | `"iface"` | `crates/abi/src/lib.rs` |
| `KIND_DISPATCH` | `&str` | `"dispatch"` | `crates/abi/src/lib.rs` |
| `KIND_NEXT` | `&str` | `"next"` | `crates/abi/src/lib.rs` |
| `KIND_STREAM_CREDIT` | `&str` | `"stream-credit"` | `crates/abi/src/lib.rs` |
| `STATUS_OK` | `i32` | `0` | `crates/abi/src/lib.rs` |
| `STATUS_ERR` | `i32` | `1` | `crates/abi/src/lib.rs` |
| `STATUS_TOO_SMALL` | `i32` | `2` | `crates/abi/src/lib.rs` |
| `STATUS_UNSUPPORTED` | `i32` | `3` | `crates/abi/src/lib.rs` |
| `STATUS_PENDING` | `i32` | `4` | `crates/abi/src/lib.rs` |
| `STATUS_STREAM_GONE` | `i32` | `5` | `crates/abi/src/lib.rs` |
| `PLUGIN_DESC_PREFIX_SIZE` | `usize` | `std::mem::offset_of!(PluginDesc, call) + std::mem::size_of::<unsafe extern "C" fn(u64, u32, Blob, Blob, *mut Out)>()` | `crates/abi/src/lib.rs` |

## ABI 结构

| 结构 | 字段 | 类型 | 位置 |
|---|---|---|---|
| `Blob` | `ptr` | `*const u8` | `crates/abi/src/lib.rs` |
| `Blob` | `len` | `usize` | `crates/abi/src/lib.rs` |
| `Out` | `ptr` | `*mut u8` | `crates/abi/src/lib.rs` |
| `Out` | `cap` | `usize` | `crates/abi/src/lib.rs` |
| `Out` | `len` | `usize` | `crates/abi/src/lib.rs` |
| `HostApi` | `abi_version` | `u32` | `crates/abi/src/lib.rs` |
| `HostApi` | `size` | `u32` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_get` | `Option<CtxGetFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_provide` | `Option<CtxProvideFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_on` | `Option<CtxOnFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_effect` | `Option<CtxEffectFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_emit` | `Option<CtxEmitFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `log` | `Option<LogFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_provide_iface` | `Option<CtxProvideIfaceFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_call_iface` | `Option<CtxCallIfaceFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_dispatch` | `Option<CtxDispatchFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_require` | `Option<CtxRequireFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_set` | `Option<CtxSetFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_isolate` | `Option<CtxIsolateFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_intercept` | `Option<CtxInterceptFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_use` | `Option<CtxUseFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_dispose` | `Option<CtxDisposeFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_dispose_self` | `Option<CtxDisposeSelfFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_fulfill` | `Option<CtxFulfillFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_next` | `Option<CtxNextFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_abort` | `Option<CtxAbortFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_stream_push` | `Option<CtxStreamPushFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_stream_close` | `Option<CtxStreamCloseFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `ctx_stream_fail` | `Option<CtxStreamFailFn>` | `crates/abi/src/lib.rs` |
| `HostApi` | `error_format` | `u32` | `crates/abi/src/lib.rs` |
| `HostApi` | `reserved` | `u32` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `abi_version` | `u32` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `size` | `u32` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `id` | `Blob` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `inject` | `Blob` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `schema` | `Blob` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `attach` | `unsafe extern "C" fn(host: *const HostApi)` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `call` | `unsafe extern "C" fn(ctx: u64, op: u32, a: Blob, b: Blob, out: *mut Out) -> i32` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `provision` | `Blob` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `error_format` | `u32` | `crates/abi/src/lib.rs` |
| `PluginDesc` | `tail_reserved` | `u32` | `crates/abi/src/lib.rs` |

## 宿主 / 核心 / SDK 公开入口（受控子集）

| 项 | 形态 | 位置 |
|---|---|---|
| `App` | struct | `crates/core/src/lib.rs` |
| `Context` | struct | `crates/core/src/context.rs` |
| `Ctx` | type alias | `crates/core/src/context.rs` |
| `Fiber` | struct | `crates/core/src/fiber.rs` |
| `FiberState` | enum | `crates/core/src/fiber.rs` |
| `Plugin` | trait | `crates/core/src/store.rs` |
| `ExternService` | trait | `crates/core/src/dyn_value.rs` |
| `ExternValue` | struct | `crates/core/src/dyn_value.rs` |
| `IfaceRef` | struct | `crates/core/src/dyn_value.rs` |
| `ResolveError` | enum | `crates/core/src/store.rs` |
| `IfaceVersionMismatch` | struct | `crates/core/src/dyn_value.rs` |
| `ErrorPayload` | struct | `crates/abi/src/error.rs` |
| `Plugin::config_schema` | method | `crates/core/src/store.rs` |
| `PendingReason` | enum | `crates/core/src/fiber.rs` |
| `PendingInject` | struct | `crates/core/src/fiber.rs` |
| `Fiber::pending_injects` | fn | `crates/core/src/fiber.rs` |
| `build_plugin_with_schema` | fn | `crates/abi/src/sdk.rs` |
| `Ctx::require_detailed` | fn | `crates/abi/src/sdk.rs` |
| `Ctx::set_detailed` | fn | `crates/abi/src/sdk.rs` |
| `Ctx::call_iface_detailed` | fn | `crates/abi/src/sdk.rs` |
| `Ctx::dispatch_detailed` | fn | `crates/abi/src/sdk.rs` |
| `Ctx::next_detailed` | fn | `crates/abi/src/sdk.rs` |
| `STREAM_WINDOW` | const | `crates/core/src/stream.rs` |
| `host_api` | fn | `crates/host/src/lib.rs` |
| `call_plugin_detailed` | unsafe fn | `crates/host/src/lib.rs` |
| `DERIVED_HANDLE_TAG` | const | `crates/host/src/lib.rs` |
| `apply_via` | fn | `crates/host/src/lib.rs` |
| `apply_via_with_keepalive` | fn | `crates/host/src/lib.rs` |
| `apply_via_with_error_format` | fn | `crates/host/src/lib.rs` |
| `apply_via_with_keepalive_and_error_format` | fn | `crates/host/src/lib.rs` |
| `set_plugin_source` | fn | `crates/host/src/lib.rs` |
| `set_dispatch_policy` | fn | `crates/host/src/lib.rs` |
| `set_dispatch_policy_for` | fn | `crates/host/src/lib.rs` |
| `open_verified` | fn | `crates/host/src/lib.rs` |
| `open_staged_verified` | fn | `crates/host/src/lib.rs` |
| `PluginSourcePolicy` | struct | `crates/host/src/source.rs` |
| `PluginManifest` | struct | `crates/host/src/source.rs` |
| `AllowedPlugin` | struct | `crates/host/src/source.rs` |
| `VerifiedPluginSource` | struct | `crates/host/src/source.rs` |
| `PluginSourcePolicy::verify` | fn | `crates/host/src/source.rs` |
| `set_active_policy` | fn | `crates/host/src/source.rs` |
| `active_policy` | fn | `crates/host/src/source.rs` |
| `set_allow_unverified` | fn | `crates/host/src/source.rs` |
| `allow_unverified` | fn | `crates/host/src/source.rs` |
| `admission` | fn | `crates/host/src/source.rs` |
| `admission_mode` | fn | `crates/host/src/source.rs` |
| `read_manifest` | fn | `crates/host/src/source.rs` |
| `sha256_file` | fn | `crates/host/src/source.rs` |
| `mounted_sources` | fn | `crates/host/src/loader.rs` |
| `DispatchPolicy` | struct | `crates/host/src/lib.rs` |
| `streams_len` | fn | `crates/host/src/lib.rs` |
| `pending_len` | fn | `crates/host/src/lib.rs` |
| `waterfall_len` | fn | `crates/host/src/lib.rs` |
| `dispatches_len` | fn | `crates/host/src/lib.rs` |
| `stage_plugin` | fn | `crates/host/src/lib.rs` |
| `staged_leaked` | fn | `crates/host/src/lib.rs` |
| `StagedFile` | struct | `crates/host/src/lib.rs` |
| `LoadedPlugin` | struct | `crates/host/src/lib.rs` |
| `probe_capabilities` | fn | `crates/host/src/lib.rs` |
| `probe_capabilities_in` | fn | `crates/host/src/lib.rs` |
| `Capabilities` | struct | `crates/host/src/lib.rs` |
| `Capabilities::schema` | fn | `crates/host/src/lib.rs` |
| `Capabilities::origin` | fn | `crates/host/src/lib.rs` |
| `LoadedPlugin::schema` | fn | `crates/host/src/lib.rs` |
| `PendingRow` | struct | `crates/host/src/loader.rs` |
| `LoaderState::pending_report` | fn | `crates/host/src/loader.rs` |
| `DiskPluginSource` | struct | `crates/host/src/loader.rs` |
| `PluginSource` | trait | `crates/host/src/loader.rs` |
| `LoadedFile` | struct | `crates/host/src/loader.rs` |
| `load_with_source` | fn | `crates/host/src/loader.rs` |
| `Entry` | struct | `crates/host/src/patch.rs` |
| `service` | attribute macro | `crates/macros/src/lib.rs` |
| `service_impl` | attribute macro | `crates/macros/src/lib.rs` |

## 载荷格式

| 入口 | 请求 | 响应 | 证据 |
|---|---|---|---|
| `OP_APPLY` | 插件配置 JSON | 空正文 + STATUS_OK；失败为文本或 ErrorPayload（看能力位） | `crates/abi/src/lib.rs` |
| `OP_CALLBACK` | kind=event/iface/dispatch/next/stream-credit；waterfall 是 mode；id / dispatch_id / event / method / args；args[0] 的 error 可带 code/category/certainty/retry/detail 兄弟字段 | {"ret": ...} / STATUS_ERR（文本或 ErrorPayload） / STATUS_PENDING | `crates/abi/src/lib.rs` |
| `OP_PING` | 空 | 空正文 + STATUS_OK | `crates/abi/src/lib.rs` |
| `OP_VOLATILE_PATHS` | 空 | {"ret": [路径...]} | `crates/abi/src/lib.rs` |
| `OP_VOLATILE_APPLY` | {"paths": [{"path": ..., "value": ...}]} | 空正文 + STATUS_OK；失败 STATUS_ERR（文本或 ErrorPayload） | `crates/abi/src/lib.rs` |
| `OP_DISPOSE` | 空 | 空正文 + STATUS_OK | `crates/abi/src/lib.rs` |
| `OP_CLEANUP` | {"id": <u64>} | 空正文 + STATUS_OK | `crates/abi/src/lib.rs` |
| `ctx_get` | 服务名 | {"value": ...} / {"missing": true} | `crates/host/src/lib.rs` |
| `ctx_provide` | name + value_json | disposer JSON | `crates/host/src/lib.rs` |
| `ctx_on` | event + cb | 空正文 + STATUS_OK | `crates/host/src/lib.rs` |
| `ctx_effect` | cb | 空正文 + STATUS_OK | `crates/host/src/lib.rs` |
| `ctx_emit` | {"mode": ..., "args": [...]} | 内联 {"pending":false,"result":...,"dispatch_id":...}；无 cb 时 {"dispatch_id":...}；异步 cb 的 args[0] 带 result/error；能力位为 1 时 error 带结构化兄弟字段 | `crates/host/src/lib.rs` |
| `log` | level + message | STATUS_OK | `crates/host/src/lib.rs` |
| `ctx_provide_iface` | name + version + methods + cb | 空正文 + STATUS_OK | `crates/host/src/lib.rs` |
| `ctx_call_iface` | service + version + method + args + cb | 内联 {"pending":false,"result":...} / {"pending":false,"error":...} / {"pending":true}；异步 cb 的 args[0] 带 result/error；能力位为 1 时 error 带结构化兄弟字段 | `crates/host/src/lib.rs` |
| `ctx_dispatch` | mode + event + args + cb | 内联 {"pending":false,"result":...,"dispatch_id":...} / {"pending":false,"error":...,"dispatch_id":...} / {"pending":true,"dispatch_id":...}；能力位为 1 时 error 带结构化兄弟字段 | `crates/host/src/lib.rs` |
| `ctx_require` | 服务名 | {"value": ...} / STATUS_ERR + ResolveError（文本或 ErrorPayload） | `crates/host/src/lib.rs` |
| `ctx_set` | name + value_json | 空正文 + STATUS_OK | `crates/host/src/lib.rs` |
| `ctx_isolate` | config JSON | {"handle": <u64>}（bit63 = 派生句柄，见 DERIVED_HANDLE_TAG） | `crates/host/src/lib.rs` |
| `ctx_intercept` | metadata JSON | {"handle": <u64>} | `crates/host/src/lib.rs` |
| `ctx_use` | url / @native 描述 | handle 或错误 | `crates/host/src/lib.rs` |
| `ctx_dispose` | {"handle": ...} | 空正文 + STATUS_OK | `crates/host/src/lib.rs` |
| `ctx_dispose_self` | 空 | 空正文 + STATUS_OK | `crates/host/src/lib.rs` |
| `ctx_fulfill` | dispatch_id + value | STATUS_OK / STATUS_ERR + ErrorPayload（文本或结构化；宿主生命周期失败为 host_fulfill/lifecycle） | `crates/host/src/lib.rs` |
| `ctx_next` | dispatch_id + args | STATUS_OK / STATUS_ERR（文本或 ErrorPayload；生命周期失败为 host_next/lifecycle）/ STATUS_PENDING；on_done 的 args[0] 带 result/error，能力位为 1 时 error 带结构化兄弟字段 | `crates/host/src/lib.rs` |
| `ctx_abort` | dispatch_id + reason | STATUS_OK / STATUS_ERR | `crates/host/src/lib.rs` |
| `ctx_stream_push` | dispatch_id + chunk + on_credit | STATUS_OK / STATUS_PENDING / STATUS_STREAM_GONE / STATUS_ERR + ErrorPayload（能力位为 1 时带结构化兄弟字段） | `crates/host/src/lib.rs` |
| `ctx_stream_close` | dispatch_id | STATUS_OK / STATUS_STREAM_GONE | `crates/host/src/lib.rs` |
| `ctx_stream_fail` | dispatch_id + reason | STATUS_OK / STATUS_STREAM_GONE / STATUS_ERR + ErrorPayload（能力位为 1 时带结构化兄弟字段） | `crates/host/src/lib.rs` |
| `error_format` | 能力位：HostApi.error_format = 1；PluginDesc.error_format = 1 | STATUS_ERR 的 out：{"error": ..., "code": ..., "category": ..., "certainty": ..., "retry": ..., "detail": ...}；缺失/0 = 纯文本 | `crates/abi/src/lib.rs` |
| `reserved` | HostApi 尾部保留字，必须为 0；不参与能力协商 | 0 | `crates/abi/src/lib.rs` |
| `plugin_error_format` | PluginDesc.error_format = 1（provision 后的尾字段） | 0 / ERROR_FORMAT_V1；宿主按 PluginDesc.size 判界 | `crates/abi/src/lib.rs` |
