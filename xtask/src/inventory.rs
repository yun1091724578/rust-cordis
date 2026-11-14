//! 公开 API / 状态码 / 载荷清单（阶段 0 A2）。
//!
//! 生成策略：
//! - `crates/abi/src/lib.rs` 的**顶层** `pub const` 与 `pub struct` 字段自动扫描；
//! - 宿主/核心/SDK 公开入口与载荷格式是受控清单——每项带一个源码 marker，
//!   `--check` 验证 marker **恰好命中一次**；新增/删除公开面必须同步本清单。
//!
//! 这样既不依赖 rustdoc JSON / nightly，也能让 ABI 常量与字段的漂移变成红灯。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ABI_SOURCE: &str = "crates/abi/src/lib.rs";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Constant {
    name: String,
    ty: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StructDef {
    name: String,
    fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: String,
    ty: String,
}

struct PublicItem {
    item: &'static str,
    kind: &'static str,
    source: &'static str,
    marker: &'static str,
}

struct Payload {
    entry: &'static str,
    request: &'static str,
    response: &'static str,
    source: &'static str,
    marker: &'static str,
}

/// 受控的宿主 / 核心 / SDK 公开入口（覆盖 SDK / CLI 实际消费面，不是所有 `pub`）。
const PUBLIC_ITEMS: &[PublicItem] = &[
    PublicItem {
        item: "App",
        kind: "struct",
        source: "crates/core/src/lib.rs",
        marker: "pub struct App",
    },
    PublicItem {
        item: "Context",
        kind: "struct",
        source: "crates/core/src/context.rs",
        marker: "pub struct Context",
    },
    PublicItem {
        item: "Ctx",
        kind: "type alias",
        source: "crates/core/src/context.rs",
        marker: "pub type Ctx = Arc<Context>",
    },
    PublicItem {
        item: "Fiber",
        kind: "struct",
        source: "crates/core/src/fiber.rs",
        marker: "pub struct Fiber",
    },
    PublicItem {
        item: "FiberState",
        kind: "enum",
        source: "crates/core/src/fiber.rs",
        marker: "pub enum FiberState",
    },
    PublicItem {
        item: "Plugin",
        kind: "trait",
        source: "crates/core/src/store.rs",
        marker: "pub trait Plugin",
    },
    PublicItem {
        item: "ExternService",
        kind: "trait",
        source: "crates/core/src/dyn_value.rs",
        marker: "pub trait ExternService",
    },
    PublicItem {
        item: "ExternValue",
        kind: "struct",
        source: "crates/core/src/dyn_value.rs",
        marker: "pub struct ExternValue",
    },
    PublicItem {
        item: "IfaceRef",
        kind: "struct",
        source: "crates/core/src/dyn_value.rs",
        marker: "pub struct IfaceRef",
    },
    PublicItem {
        item: "ResolveError",
        kind: "enum",
        source: "crates/core/src/store.rs",
        marker: "pub enum ResolveError",
    },
    PublicItem {
        item: "IfaceVersionMismatch",
        kind: "struct",
        source: "crates/core/src/dyn_value.rs",
        marker: "pub struct IfaceVersionMismatch",
    },
    PublicItem {
        item: "ErrorPayload",
        kind: "struct",
        source: "crates/abi/src/error.rs",
        marker: "pub struct ErrorPayload",
    },
    PublicItem {
        item: "Plugin::version",
        kind: "method",
        source: "crates/core/src/store.rs",
        marker: "/// 可选的插件版本（诊断/身份用，不是 ABI 授权）。",
    },
    PublicItem {
        item: "Plugin::config_schema",
        kind: "method",
        source: "crates/core/src/store.rs",
        marker: "/// **配置 schema 声明**（阶段 3）：插件自述的 JSON Schema，供装载前审阅。",
    },
    PublicItem {
        item: "PendingReason",
        kind: "enum",
        source: "crates/core/src/fiber.rs",
        marker: "pub enum PendingReason",
    },
    PublicItem {
        item: "PendingInject",
        kind: "struct",
        source: "crates/core/src/fiber.rs",
        marker: "pub struct PendingInject",
    },
    PublicItem {
        item: "Fiber::pending_injects",
        kind: "fn",
        source: "crates/core/src/fiber.rs",
        marker: "pub fn pending_injects(&self) -> Vec<PendingInject>",
    },
    PublicItem {
        item: "build_plugin_with_schema",
        kind: "fn",
        source: "crates/abi/src/sdk.rs",
        marker: "pub fn build_plugin_with_schema<",
    },
    PublicItem {
        item: "Ctx::require_detailed",
        kind: "fn",
        source: "crates/abi/src/sdk.rs",
        marker: "pub fn require_detailed(",
    },
    PublicItem {
        item: "Ctx::set_detailed",
        kind: "fn",
        source: "crates/abi/src/sdk.rs",
        marker: "pub fn set_detailed(",
    },
    PublicItem {
        item: "Ctx::call_iface_detailed",
        kind: "fn",
        source: "crates/abi/src/sdk.rs",
        marker: "pub fn call_iface_detailed<",
    },
    PublicItem {
        item: "Ctx::dispatch_detailed",
        kind: "fn",
        source: "crates/abi/src/sdk.rs",
        marker: "pub fn dispatch_detailed<",
    },
    PublicItem {
        item: "Ctx::next_detailed",
        kind: "fn",
        source: "crates/abi/src/sdk.rs",
        marker: "pub fn next_detailed<",
    },
    PublicItem {
        item: "STREAM_WINDOW",
        kind: "const",
        source: "crates/core/src/stream.rs",
        marker: "pub const STREAM_WINDOW",
    },
    PublicItem {
        item: "host_api",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn host_api",
    },
    PublicItem {
        item: "call_plugin_detailed",
        kind: "unsafe fn",
        source: "crates/host/src/lib.rs",
        marker: "pub unsafe fn call_plugin_detailed(",
    },
    PublicItem {
        item: "DERIVED_HANDLE_TAG",
        kind: "const",
        source: "crates/host/src/lib.rs",
        marker: "pub const DERIVED_HANDLE_TAG",
    },
    PublicItem {
        item: "apply_via",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn apply_via(",
    },
    PublicItem {
        item: "apply_via_with_keepalive",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn apply_via_with_keepalive(",
    },
    PublicItem {
        item: "apply_via_with_error_format",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn apply_via_with_error_format(",
    },
    PublicItem {
        item: "apply_via_with_keepalive_and_error_format",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn apply_via_with_keepalive_and_error_format(",
    },
    PublicItem {
        item: "set_plugin_source",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn set_plugin_source",
    },
    PublicItem {
        item: "set_dispatch_policy",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn set_dispatch_policy(",
    },
    PublicItem {
        item: "set_dispatch_policy_for",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn set_dispatch_policy_for",
    },
    PublicItem {
        item: "set_host_limits_for",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn set_host_limits_for",
    },
    PublicItem {
        item: "set_default_host_limits",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn set_default_host_limits",
    },
    PublicItem {
        item: "open_verified",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn open_verified(",
    },
    PublicItem {
        item: "open_staged_verified",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn open_staged_verified(",
    },
    PublicItem {
        item: "PluginSourcePolicy",
        kind: "struct",
        source: "crates/host/src/source.rs",
        marker: "pub struct PluginSourcePolicy",
    },
    PublicItem {
        item: "PluginManifest",
        kind: "struct",
        source: "crates/host/src/source.rs",
        marker: "pub struct PluginManifest",
    },
    PublicItem {
        item: "AllowedPlugin",
        kind: "struct",
        source: "crates/host/src/source.rs",
        marker: "pub struct AllowedPlugin",
    },
    PublicItem {
        item: "VerifiedPluginSource",
        kind: "struct",
        source: "crates/host/src/source.rs",
        marker: "pub struct VerifiedPluginSource",
    },
    PublicItem {
        item: "PluginSourcePolicy::verify",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn verify(&self",
    },
    PublicItem {
        item: "set_active_policy",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn set_active_policy",
    },
    PublicItem {
        item: "active_policy",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn active_policy",
    },
    PublicItem {
        item: "set_allow_unverified",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn set_allow_unverified",
    },
    PublicItem {
        item: "allow_unverified",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn allow_unverified",
    },
    PublicItem {
        item: "admission",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn admission(",
    },
    PublicItem {
        item: "admission_mode",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn admission_mode",
    },
    PublicItem {
        item: "read_manifest",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn read_manifest",
    },
    PublicItem {
        item: "sha256_file",
        kind: "fn",
        source: "crates/host/src/source.rs",
        marker: "pub fn sha256_file",
    },
    PublicItem {
        item: "mounted_sources",
        kind: "fn",
        source: "crates/host/src/loader.rs",
        marker: "pub fn mounted_sources",
    },
    PublicItem {
        item: "DispatchPolicy",
        kind: "struct",
        source: "crates/host/src/lib.rs",
        marker: "pub struct DispatchPolicy",
    },
    PublicItem {
        item: "DispatchPolicy::new",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn new(timeout_ms: Option<u64>, outbound_timeout_ms: Option<u64>) -> Self",
    },
    PublicItem {
        item: "HostLimits",
        kind: "struct",
        source: "crates/host/src/lib.rs",
        marker: "pub struct HostLimits",
    },
    PublicItem {
        item: "HostLimits::new",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn new(\n        max_instances_per_app: usize,",
    },
    PublicItem {
        item: "streams_len",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn streams_len",
    },
    PublicItem {
        item: "pending_len",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn pending_len",
    },
    PublicItem {
        item: "waterfall_len",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn waterfall_len",
    },
    PublicItem {
        item: "dispatches_len",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn dispatches_len",
    },
    PublicItem {
        item: "stage_plugin",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn stage_plugin",
    },
    PublicItem {
        item: "staged_leaked",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn staged_leaked",
    },
    PublicItem {
        item: "StagedFile",
        kind: "struct",
        source: "crates/host/src/lib.rs",
        marker: "pub struct StagedFile",
    },
    PublicItem {
        item: "LoadedPlugin",
        kind: "struct",
        source: "crates/host/src/lib.rs",
        marker: "pub struct LoadedPlugin",
    },
    PublicItem {
        item: "probe_capabilities",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn probe_capabilities(",
    },
    PublicItem {
        item: "probe_capabilities_in",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn probe_capabilities_in(",
    },
    PublicItem {
        item: "Capabilities",
        kind: "struct",
        source: "crates/host/src/lib.rs",
        marker: "pub struct Capabilities",
    },
    PublicItem {
        item: "Capabilities::schema",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "/// 插件自述的配置 schema（合法 JSON 对象时返回原文）。",
    },
    PublicItem {
        item: "Capabilities::origin",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "pub fn origin(&self) -> &'static str",
    },
    PublicItem {
        item: "LoadedPlugin::schema",
        kind: "fn",
        source: "crates/host/src/lib.rs",
        marker: "/// 插件自述的配置 schema（合法 JSON 对象；未声明/非法 ⇒ `None`）。",
    },
    PublicItem {
        item: "PendingRow",
        kind: "struct",
        source: "crates/host/src/loader.rs",
        marker: "pub struct PendingRow",
    },
    PublicItem {
        item: "LoaderState::pending_report",
        kind: "fn",
        source: "crates/host/src/loader.rs",
        marker: "pub fn pending_report(&self) -> Vec<PendingRow>",
    },
    PublicItem {
        item: "DiskPluginSource",
        kind: "struct",
        source: "crates/host/src/loader.rs",
        marker: "pub struct DiskPluginSource",
    },
    PublicItem {
        item: "PluginSource",
        kind: "trait",
        source: "crates/host/src/loader.rs",
        marker: "pub trait PluginSource",
    },
    PublicItem {
        item: "LoadedFile",
        kind: "struct",
        source: "crates/host/src/loader.rs",
        marker: "pub struct LoadedFile",
    },
    PublicItem {
        item: "load_with_source",
        kind: "fn",
        source: "crates/host/src/loader.rs",
        marker: "pub async fn load_with_source",
    },
    PublicItem {
        item: "LoaderConfig::new",
        kind: "fn",
        source: "crates/host/src/loader.rs",
        marker: "pub fn new(path: impl Into<String>) -> Self",
    },
    PublicItem {
        item: "Entry",
        kind: "struct",
        source: "crates/host/src/patch.rs",
        marker: "pub struct Entry",
    },
    PublicItem {
        item: "service",
        kind: "attribute macro",
        source: "crates/macros/src/lib.rs",
        marker: "pub fn service(",
    },
    PublicItem {
        item: "service_impl",
        kind: "attribute macro",
        source: "crates/macros/src/lib.rs",
        marker: "pub fn service_impl(",
    },
];

/// 跨 ABI 的 op 与宿主函数载荷（受控清单，必须覆盖所有 `OP_*` 与 `HostApi` 字段）。
const PAYLOADS: &[Payload] = &[
    Payload {
        entry: "OP_APPLY",
        request: "插件配置 JSON",
        response: "空正文 + STATUS_OK；失败为文本或 ErrorPayload（看能力位）",
        source: ABI_SOURCE,
        marker: "pub const OP_APPLY",
    },
    Payload {
        entry: "OP_CALLBACK",
        request: "kind=event/iface/dispatch/next/stream-credit；waterfall 是 mode；id / dispatch_id / event / method / args；args[0] 的 error 可带 code/category/certainty/retry/detail 兄弟字段",
        response: "{\"ret\": ...} / STATUS_ERR（文本或 ErrorPayload） / STATUS_PENDING",
        source: ABI_SOURCE,
        marker: "pub const OP_CALLBACK",
    },
    Payload {
        entry: "OP_PING",
        request: "空",
        response: "空正文 + STATUS_OK",
        source: ABI_SOURCE,
        marker: "pub const OP_PING",
    },
    Payload {
        entry: "OP_VOLATILE_PATHS",
        request: "空",
        response: "{\"ret\": [路径...]}",
        source: ABI_SOURCE,
        marker: "pub const OP_VOLATILE_PATHS",
    },
    Payload {
        entry: "OP_VOLATILE_APPLY",
        request: "{\"paths\": [{\"path\": ..., \"value\": ...}]}",
        response: "空正文 + STATUS_OK；失败 STATUS_ERR（文本或 ErrorPayload）",
        source: ABI_SOURCE,
        marker: "pub const OP_VOLATILE_APPLY",
    },
    Payload {
        entry: "OP_DISPOSE",
        request: "空",
        response: "空正文 + STATUS_OK",
        source: ABI_SOURCE,
        marker: "pub const OP_DISPOSE",
    },
    Payload {
        entry: "OP_CLEANUP",
        request: "{\"id\": <u64>}",
        response: "空正文 + STATUS_OK",
        source: ABI_SOURCE,
        marker: "pub const OP_CLEANUP",
    },
    Payload {
        entry: "ctx_get",
        request: "服务名",
        response: "{\"value\": ...} / {\"missing\": true}",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_get(",
    },
    Payload {
        entry: "ctx_provide",
        request: "name + value_json",
        response: "disposer JSON",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_provide(",
    },
    Payload {
        entry: "ctx_on",
        request: "event + cb",
        response: "空正文 + STATUS_OK",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_on(",
    },
    Payload {
        entry: "ctx_effect",
        request: "cb",
        response: "空正文 + STATUS_OK",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_effect(",
    },
    Payload {
        entry: "ctx_emit",
        request: "{\"mode\": ..., \"args\": [...]}",
        response: "内联 {\"pending\":false,\"result\":...,\"dispatch_id\":...}；无 cb 时 {\"dispatch_id\":...}；异步 cb 的 args[0] 带 result/error；能力位为 1 时 error 带结构化兄弟字段",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_emit(",
    },
    Payload {
        entry: "log",
        request: "level + message",
        response: "STATUS_OK",
        source: "crates/host/src/lib.rs",
        marker: "fn host_log(",
    },
    Payload {
        entry: "ctx_provide_iface",
        request: "name + version + methods + cb",
        response: "空正文 + STATUS_OK",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_provide_iface(",
    },
    Payload {
        entry: "ctx_call_iface",
        request: "service + version + method + args + cb",
        response: "内联 {\"pending\":false,\"result\":...} / {\"pending\":false,\"error\":...} / {\"pending\":true}；异步 cb 的 args[0] 带 result/error；能力位为 1 时 error 带结构化兄弟字段",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_call_iface(",
    },
    Payload {
        entry: "ctx_dispatch",
        request: "mode + event + args + cb",
        response: "内联 {\"pending\":false,\"result\":...,\"dispatch_id\":...} / {\"pending\":false,\"error\":...,\"dispatch_id\":...} / {\"pending\":true,\"dispatch_id\":...}；能力位为 1 时 error 带结构化兄弟字段",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_dispatch(",
    },
    Payload {
        entry: "ctx_require",
        request: "服务名",
        response: "{\"value\": ...} / STATUS_ERR + ResolveError（文本或 ErrorPayload）",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_require(",
    },
    Payload {
        entry: "ctx_set",
        request: "name + value_json",
        response: "空正文 + STATUS_OK",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_set(",
    },
    Payload {
        entry: "ctx_isolate",
        request: "config JSON",
        response: "{\"handle\": <u64>}（bit63 = 派生句柄，见 DERIVED_HANDLE_TAG）",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_isolate(",
    },
    Payload {
        entry: "ctx_intercept",
        request: "metadata JSON",
        response: "{\"handle\": <u64>}",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_intercept(",
    },
    Payload {
        entry: "ctx_use",
        request: "url / @native 描述",
        response: "handle 或错误",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_use(",
    },
    Payload {
        entry: "ctx_dispose",
        request: "{\"handle\": ...}",
        response: "空正文 + STATUS_OK",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_dispose(",
    },
    Payload {
        entry: "ctx_dispose_self",
        request: "空",
        response: "空正文 + STATUS_OK",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_dispose_self(",
    },
    Payload {
        entry: "ctx_fulfill",
        request: "dispatch_id + value",
        response: "STATUS_OK / STATUS_ERR + ErrorPayload（文本或结构化；宿主生命周期失败为 host_fulfill/lifecycle）",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_fulfill(",
    },
    Payload {
        entry: "ctx_next",
        request: "dispatch_id + args",
        response: "STATUS_OK / STATUS_ERR（文本或 ErrorPayload；生命周期失败为 host_next/lifecycle）/ STATUS_PENDING；on_done 的 args[0] 带 result/error，能力位为 1 时 error 带结构化兄弟字段",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_next(",
    },
    Payload {
        entry: "ctx_abort",
        request: "dispatch_id + reason",
        response: "STATUS_OK / STATUS_ERR",
        source: "crates/host/src/lib.rs",
        marker: "fn host_ctx_abort(",
    },
    Payload {
        entry: "ctx_stream_push",
        request: "dispatch_id + chunk + on_credit",
        response: "STATUS_OK / STATUS_PENDING / STATUS_STREAM_GONE / STATUS_ERR + ErrorPayload（能力位为 1 时带结构化兄弟字段）",
        source: "crates/host/src/lib.rs",
        marker: "fn host_stream_push(",
    },
    Payload {
        entry: "ctx_stream_close",
        request: "dispatch_id",
        response: "STATUS_OK / STATUS_STREAM_GONE",
        source: "crates/host/src/lib.rs",
        marker: "fn host_stream_close(",
    },
    Payload {
        entry: "ctx_stream_fail",
        request: "dispatch_id + reason",
        response: "STATUS_OK / STATUS_STREAM_GONE / STATUS_ERR + ErrorPayload（能力位为 1 时带结构化兄弟字段）",
        source: "crates/host/src/lib.rs",
        marker: "fn host_stream_fail(",
    },
    Payload {
        entry: "error_format",
        request: "能力位：HostApi.error_format = 1；PluginDesc.error_format = 1",
        response: "STATUS_ERR 的 out：{\"error\": ..., \"code\": ..., \"category\": ..., \"certainty\": ..., \"retry\": ..., \"detail\": ...}；缺失/0 = 纯文本",
        source: ABI_SOURCE,
        marker: "pub error_format: u32,\n    /// 尾追加后的保留字：占住结构体尾部填充，保证后续字段从下一个 8 字节边界开始",
    },
    Payload {
        entry: "reserved",
        request: "HostApi 尾部保留字，必须为 0；不参与能力协商",
        response: "0",
        source: ABI_SOURCE,
        marker: "pub reserved: u32",
    },
    Payload {
        entry: "plugin_error_format",
        request: "PluginDesc.error_format = 1（provision 后的尾字段）",
        response: "0 / ERROR_FORMAT_V1；宿主按 PluginDesc.size 判界",
        source: ABI_SOURCE,
        marker: "pub error_format: u32,\n    /// 尾追加后的保留字：占住结构体尾部填充，保证后续字段从 96B 开始",
    },
];

fn scan_constants(source: &str) -> Vec<Constant> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        // 只收顶层常量；`impl` 里的关联常量是缩进的，不会被误收。
        if let Some(rest) = line.strip_prefix("pub const ") {
            if let Some((name, rest)) = rest.split_once(':') {
                if let Some((ty, value)) = rest.split_once('=') {
                    let mut value = value.trim().to_string();
                    while !value.contains(';') && index + 1 < lines.len() {
                        index += 1;
                        value.push(' ');
                        value.push_str(lines[index].trim());
                    }
                    // 行尾注释可能跟在 `;` 后面；只取分号前的值，且不要吞掉下一条声明。
                    let value = value.split(';').next().unwrap_or("").trim().to_string();
                    out.push(Constant {
                        name: name.trim().to_string(),
                        ty: ty.trim().to_string(),
                        value,
                    });
                }
            }
        }
        index += 1;
    }
    out
}

fn scan_structs(source: &str) -> Result<Vec<StructDef>, String> {
    let mut out = Vec::new();
    let mut current: Option<StructDef> = None;
    for line in source.lines() {
        if let Some(rest) = line.strip_prefix("pub struct ") {
            if current.is_some() {
                return Err(format!("pub struct 嵌套/未闭合: {line}"));
            }
            let name = rest.split([' ', '{', '<']).next().unwrap_or("").to_string();
            if name.is_empty() || !rest.trim_end().ends_with('{') {
                return Err(format!("不支持的 pub struct 形态: {line}"));
            }
            current = Some(StructDef {
                name,
                fields: Vec::new(),
            });
            continue;
        }
        let Some(def) = current.as_mut() else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed == "}" {
            if def.fields.is_empty() {
                return Err(format!("结构 {} 没有 pub 字段；清单不能静默漏掉", def.name));
            }
            out.push(current.take().expect("current 存在"));
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("pub ") {
            if let Some((name, ty)) = rest.split_once(':') {
                def.fields.push(Field {
                    name: name.trim().to_string(),
                    ty: ty.trim().trim_end_matches(',').trim().to_string(),
                });
                continue;
            }
        }
        return Err(format!("结构 {} 里有无法归类的行: {line}", def.name));
    }
    if let Some(def) = current {
        return Err(format!("结构 {} 未闭合", def.name));
    }
    Ok(out)
}

fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|")
}

fn unique_marker(source: &str, marker: &str) -> Result<(), String> {
    let count = source.match_indices(marker).count();
    if count == 1 {
        Ok(())
    } else {
        Err(format!(
            "marker `{marker}` 命中 {count} 次（必须恰好 1 次）"
        ))
    }
}

fn read(root: &Path, source: &str) -> Result<String, String> {
    // 多行 marker 含 `\n`；CRLF 检出必须先归一化，否则 Windows 上匹配不到。
    std::fs::read_to_string(root.join(source))
        .map(|text| text.replace("\r\n", "\n"))
        .map_err(|err| format!("读取 {source} 失败: {err}"))
}

fn source_text<'a>(
    root: &Path,
    cache: &'a mut HashMap<&'static str, String>,
    source: &'static str,
) -> Result<&'a str, String> {
    if !cache.contains_key(source) {
        cache.insert(source, read(root, source)?);
    }
    Ok(cache.get(source).expect("刚插入").as_str())
}

/// 生成完整清单文本。任何 marker 缺失/重复或 payload 缺项都会返回 Err。
pub fn render(root: &Path) -> Result<String, String> {
    let abi = read(root, ABI_SOURCE)?;
    let constants = scan_constants(&abi);
    let structs = scan_structs(&abi)?;
    if constants.is_empty() {
        return Err("ABI 源码里没有扫到任何顶层 pub const".into());
    }
    if structs.is_empty() {
        return Err("ABI 源码里没有扫到任何 pub struct".into());
    }

    // 每个 OP_* 与每个 HostApi 字段都必须有载荷行；KIND_* 的值必须出现在
    // OP_CALLBACK 的请求描述里。这样“新增 op/字段但忘了写载荷”会直接失败。
    let host = structs
        .iter()
        .find(|def| def.name == "HostApi")
        .ok_or_else(|| "ABI 缺少 HostApi 结构".to_string())?;
    for field in &host.fields {
        if field.name == "abi_version" || field.name == "size" {
            continue;
        }
        if !PAYLOADS.iter().any(|payload| payload.entry == field.name) {
            return Err(format!("payload 缺少 HostApi.{}", field.name));
        }
    }
    for constant in &constants {
        if constant.name.starts_with("OP_")
            && !PAYLOADS
                .iter()
                .any(|payload| payload.entry == constant.name)
        {
            return Err(format!("payload 缺少 {}", constant.name));
        }
    }
    let callback = PAYLOADS
        .iter()
        .find(|payload| payload.entry == "OP_CALLBACK")
        .ok_or_else(|| "payload 缺少 OP_CALLBACK".to_string())?;
    for kind in constants
        .iter()
        .filter(|constant| constant.name.starts_with("KIND_"))
    {
        let value = kind.value.trim_matches('"');
        if !callback.request.contains(value) {
            return Err(format!("OP_CALLBACK 请求缺少 KIND 值 {value}"));
        }
    }

    let mut cache: HashMap<&'static str, String> = HashMap::new();
    let mut out = String::new();
    out.push_str("# 契约公开面 / 状态码 / 载荷清单（受控子集）\n\n");
    out.push_str(
        "> 由 `cargo xtask inventory --write` 生成；`cargo xtask inventory --check` 在 CI 比对。\n",
    );
    out.push_str(
        "> 范围：ABI `lib.rs` 的顶层常量/`repr(C)` 结构 + 宿主/核心/SDK 的受控入口；`ErrorPayload` 字段见 `crates/abi/src/error.rs`。**不是**所有 crate 的 `pub`。\n",
    );
    out.push_str(
        "> ABI 常量/字段自动扫描；宿主入口与载荷是受控清单，新增/删除必须同步本文件。\n\n",
    );

    out.push_str("## ABI 常量\n\n");
    out.push_str("| 名称 | 类型 | 值 | 位置 |\n|---|---|---|---|\n");
    for constant in &constants {
        out.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{}` |\n",
            constant.name,
            escape_cell(&constant.ty),
            escape_cell(&constant.value),
            ABI_SOURCE
        ));
    }

    out.push_str("\n## ABI 结构\n\n");
    out.push_str("| 结构 | 字段 | 类型 | 位置 |\n|---|---|---|---|\n");
    for def in &structs {
        for field in &def.fields {
            out.push_str(&format!(
                "| `{}` | `{}` | `{}` | `{}` |\n",
                def.name,
                field.name,
                escape_cell(&field.ty),
                ABI_SOURCE
            ));
        }
    }

    out.push_str("\n## 宿主 / 核心 / SDK 公开入口（受控子集）\n\n");
    out.push_str("| 项 | 形态 | 位置 |\n|---|---|---|\n");
    for item in PUBLIC_ITEMS {
        let text = source_text(root, &mut cache, item.source)?;
        unique_marker(text, item.marker)
            .map_err(|err| format!("{}（{}）：{err}", item.item, item.kind))?;
        out.push_str(&format!(
            "| `{}` | {} | `{}` |\n",
            item.item, item.kind, item.source
        ));
    }

    out.push_str("\n## 载荷格式\n\n");
    out.push_str("| 入口 | 请求 | 响应 | 证据 |\n|---|---|---|---|\n");
    for payload in PAYLOADS {
        let text = source_text(root, &mut cache, payload.source)?;
        unique_marker(text, payload.marker)
            .map_err(|err| format!("载荷 {}：{err}", payload.entry))?;
        out.push_str(&format!(
            "| `{}` | {} | {} | `{}` |\n",
            payload.entry,
            escape_cell(payload.request),
            escape_cell(payload.response),
            payload.source
        ));
    }

    Ok(out)
}

/// 默认清单路径（与 `cargo xtask` 的当前目录无关）。
pub fn default_path() -> PathBuf {
    repo_root().join("docs").join("api-inventory.md")
}

/// 仓库根：用于把显式相对路径解析到仓库根，而不是调用方 CWD。
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask 的上一级是仓库根")
        .to_path_buf()
}

/// 读取并比对一份清单；返回违规（空 = 通过）。
pub fn check_path(path: &Path) -> Vec<String> {
    let actual = match std::fs::read_to_string(path) {
        Ok(text) => text.replace("\r\n", "\n"),
        Err(err) => return vec![format!("读取 {} 失败: {err}", path.display())],
    };
    let expected = match render(&repo_root()) {
        Ok(text) => text,
        Err(err) => return vec![err],
    };
    if actual == expected {
        Vec::new()
    } else {
        vec![format!(
            "{} 与源码不一致；运行 `cargo xtask inventory --write` 重新生成",
            path.display()
        )]
    }
}

/// 生成并写入清单；父目录不存在时创建。
pub fn write_path(path: &Path) -> Result<(), String> {
    let expected = render(&repo_root())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("创建 {} 失败: {err}", parent.display()))?;
    }
    std::fs::write(path, expected).map_err(|err| format!("写入 {} 失败: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_top_level_constants_and_skips_associated_ones() {
        let source = r#"
pub const A: u32 = 1;
pub const B: &str = "x";
impl X {
    pub const C: u32 = 2;
}
"#;
        let constants = scan_constants(source);
        assert_eq!(
            constants,
            vec![
                Constant {
                    name: "A".into(),
                    ty: "u32".into(),
                    value: "1".into()
                },
                Constant {
                    name: "B".into(),
                    ty: "&str".into(),
                    value: "\"x\"".into()
                }
            ]
        );
    }

    #[test]
    fn read_normalizes_crlf_before_marker_matching() {
        let dir =
            std::env::temp_dir().join(format!("cordis-inventory-crlf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        std::fs::write(dir.join("source.rs"), "first\r\nsecond\r\n").expect("写临时源码");
        let text = read(&dir, "source.rs").expect("读取临时源码");
        assert_eq!(text, "first\nsecond\n");
        assert!(
            unique_marker(&text, "first\nsecond").is_ok(),
            "CRLF 源码归一化后必须能匹配多行 marker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scans_multiline_constant_values() {
        let source = "pub const N: usize = std::mem::size_of::<u8>()\n    + 1;\n";
        let constants = scan_constants(source);
        assert_eq!(constants.len(), 1);
        assert_eq!(constants[0].value, "std::mem::size_of::<u8>() + 1");
    }

    #[test]
    fn scans_constants_with_trailing_comments_without_eating_the_next_one() {
        let source = "pub const A: u32 = 1; // 见 ADR\npub const B: u32 = 2;\n";
        let constants = scan_constants(source);
        assert_eq!(constants.len(), 2);
        assert_eq!(constants[0].value, "1");
        assert_eq!(constants[1].name, "B");
    }

    #[test]
    fn scans_struct_fields() {
        let source = r#"
pub struct P {
    pub a: u32,
    pub b: Option<Blob>,
}

pub struct Q {
    pub c: *const u8,
}
"#;
        let structs = scan_structs(source).expect("命名结构体必须能解析");
        assert_eq!(structs.len(), 2);
        assert_eq!(structs[0].name, "P");
        assert_eq!(structs[0].fields.len(), 2);
        assert_eq!(structs[1].name, "Q");
        assert_eq!(structs[1].fields[0].name, "c");
    }

    #[test]
    fn rejects_unsupported_struct_forms() {
        assert!(scan_structs("pub struct T(u32);").is_err());
        assert!(scan_structs("pub struct U;").is_err());
        assert!(scan_structs("pub struct S { pub a: u32 }").is_err());
        assert!(scan_structs("pub struct P {\n    pub(crate) a: u32,\n}\n").is_err());
        assert!(scan_structs("pub struct P {\n    pub a: u32,\n").is_err());
    }

    #[test]
    fn real_catalog_counts_are_pinned() {
        let abi = read(&repo_root(), ABI_SOURCE).expect("读取 ABI 源码");
        assert_eq!(scan_constants(&abi).len(), 23, "ABI 常量数量漂移");
        let structs = scan_structs(&abi).expect("ABI 结构体必须可解析");
        assert_eq!(structs.len(), 4, "ABI 结构体数量漂移");
        assert_eq!(
            structs.iter().map(|item| item.fields.len()).sum::<usize>(),
            41,
            "ABI 字段总数漂移"
        );
        assert_eq!(PUBLIC_ITEMS.len(), 80, "受控公开入口数量漂移");
        assert_eq!(PAYLOADS.len(), 32, "载荷清单数量漂移");
    }

    #[test]
    fn renders_the_real_catalog() {
        let text = render(&repo_root()).expect("真实 ABI/宿主源码必须能生成清单");
        for expected in [
            "`ABI_VERSION`",
            "`STATUS_ERR`",
            "`PluginDesc`",
            "`HostApi`",
            "`probe_capabilities`",
            "`OP_CALLBACK`",
            "`ctx_stream_push`",
            "`service`",
            "`service_impl`",
            "`Context`",
            "`DERIVED_HANDLE_TAG`",
            "`ErrorPayload`",
            "`call_plugin_detailed`",
        ] {
            assert!(text.contains(expected), "清单缺少 {expected}");
        }
    }

    #[test]
    fn every_op_and_host_field_has_a_payload_row() {
        let abi = read(&repo_root(), ABI_SOURCE).expect("读取 ABI 源码");
        for constant in scan_constants(&abi)
            .iter()
            .filter(|constant| constant.name.starts_with("OP_"))
        {
            assert!(
                PAYLOADS
                    .iter()
                    .any(|payload| payload.entry == constant.name),
                "payload 缺少 {}",
                constant.name
            );
        }
        let host = scan_structs(&abi)
            .expect("ABI 结构体必须可解析")
            .into_iter()
            .find(|def| def.name == "HostApi")
            .expect("ABI 必须有 HostApi");
        for field in &host.fields {
            if field.name == "abi_version" || field.name == "size" {
                continue;
            }
            assert!(
                PAYLOADS.iter().any(|payload| payload.entry == field.name),
                "payload 缺少 HostApi.{}",
                field.name
            );
        }
    }

    #[test]
    fn payload_texts_cover_wire_details() {
        let callback = PAYLOADS
            .iter()
            .find(|payload| payload.entry == "OP_CALLBACK")
            .expect("OP_CALLBACK 必须有 payload 行");
        assert!(
            callback.request.contains("dispatch"),
            "OP_CALLBACK 漏了 kind=dispatch：{}",
            callback.request
        );
        assert!(
            callback.request.contains("waterfall") && callback.request.contains("mode"),
            "waterfall 是 mode，不是 kind：{}",
            callback.request
        );
        assert!(
            callback.response.contains("STATUS_PENDING"),
            "OP_CALLBACK 漏了 STATUS_PENDING：{}",
            callback.response
        );
        let cleanup = PAYLOADS
            .iter()
            .find(|payload| payload.entry == "OP_CLEANUP")
            .expect("OP_CLEANUP 必须有 payload 行");
        assert!(
            cleanup.request.contains("\"id\""),
            "OP_CLEANUP 请求是 {{\"id\": ...}}：{}",
            cleanup.request
        );
        assert!(
            PAYLOADS
                .iter()
                .any(|payload| payload.entry == "OP_VOLATILE_APPLY"),
            "OP_VOLATILE_APPLY 必须有 payload 行"
        );
    }

    #[test]
    fn marker_must_be_unique() {
        let source = "pub fn apply_via(...)\npub fn apply_via_with_keepalive(...)";
        assert!(unique_marker(source, "pub fn apply_via(").is_ok());
        assert!(
            unique_marker(source, "pub fn apply_via").is_err(),
            "同前缀的兄弟函数必须让 marker 失败"
        );
        assert!(
            unique_marker(source, "pub fn nope(").is_err(),
            "缺失的 marker 必须失败"
        );
    }
}
