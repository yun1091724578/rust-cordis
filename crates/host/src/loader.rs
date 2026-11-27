//! 配置驱动的插件树装载器 + 热重载。
//!
//! 对应 cordis 的 `@cordisjs/plugin-loader` + `@cordisjs/plugin-include`：
//! 读一份 `cordis.yml`，叠若干 `cordis.patch.yml`，把每条配置行变成一棵插件 fiber 树。
//!
//! # 为什么它必须在内核之外
//! 内核只认 `Plugin` 与 `inject`，完全不关心"插件从哪来"。装载器是唯一知道
//! "配置行 → 解析 → 动态库"这条链的地方，换包管理器（npm 风格解析、OCI、内置 registry）
//! 只需要换 [`crate::resolve_plugin_path`] 一个函数。
//!
//! # 热重载的两条硬约束
//! 1. **必须先卸后装。** 内核里"同一隔离域中一个服务名只有一个实现"是不变量，
//!    所以新实例的 `provide` 会在旧实例还活着时直接失败。这不是实现缺陷，
//!    而是 cordis 服务容器的定义性质——热重载因此天然是"卸载 + 重挂"，
//!    不是"热插拔"。能做的补偿是**把预检提前**（见下），而不是取消这条约束。
//! 2. **必须换文件名。** 已加载的动态库在 Windows 上被文件系统锁住，
//!    在 Linux 上覆盖已映射的 `.so` 会让进程进入未定义状态。
//!    所以新库落到 `<name>.r<修订号>.<ext>`，旧句柄继续指向旧文件。

use crate::patch::{
    apply_entry_patches_full, flatten_startup_table, parse_entries_with_startup,
    parse_patches_with_startup, Entry, PatchOwners, StartupNode,
};
use crate::plan::PlanEntrySpec;
use crate::source::{PluginSourcePolicy, SourceAdmission};
use crate::{
    mount_with_label, resolve_plugin_path, set_dispatch_policy_for, set_host_limits_for,
    stage_plugin, startup_policy_for, DispatchPolicy, FailureCategory, FailureDisposition,
    HostLimits, LoadedPlugin, RowStartup, StagedFile, StartupPolicy,
};
use cordis_core::{
    equal_except_volatile, split_path, values_at, BoxFuture, Call, CloseReport, CloseStatus,
    Config, Ctx, Disposable, Dyn, ExternValue, Fiber, FiberState, FnPlugin, Inject, Level, Mode,
    PendingInject, Plugin, RealmId, Result, Ret, Scope, UpdateOutcome, ValidationError,
    UID_DISPOSED,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

pub const LOADER_ID: &str = "cordis:include";

/// 装载器配置。
///
/// # 键名风格是兼容性的一部分
/// cordis 的配置字段是 **camelCase**（`baseUrl` / `enableLogs` / `pluginDir`），
/// 因为它们是 TS 接口的字段名直接序列化来的。Rust 侧用默认的 snake_case
/// 会让现有配置文件里的键**静默失效**——`#[serde(default)]` 会把它悄悄变成缺省值，
/// 这是最难查的一类错误。所以这里必须显式对齐 camelCase。
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoaderConfig {
    /// 主配置文件路径（YAML 条目数组）。
    pub path: String,
    /// 依次叠加的 patch 文件路径。
    #[serde(default)]
    pub patches: Vec<String>,
    /// 插件动态库的搜索目录；缺省时用主配置文件所在目录。
    #[serde(default)]
    pub plugin_dir: Option<String>,
    /// 有任一条目装载失败时是否让装载器本身失败。
    ///
    /// 默认 `true`：cordis 的立场是 **fail loud**（harness 的 app-boot 甚至有
    /// `auditStartupEntries` 专门审计必需条目）。静默跳过一行配置，
    /// 会让人在几小时后才发现某个能力根本没装上。
    #[serde(default = "default_strict")]
    pub strict: bool,
    /// 声明级依赖环是否升级为装载失败。
    ///
    /// **默认 `false`**：A6 要求环默认只报告、不拒挂。`strict` 只负责
    /// 逐行装载失败/插值失败；把两个语义混成一个开关会让默认配置在遇到环时
    /// 直接 boot 失败。
    #[serde(default)]
    pub strict_cycles: bool,
    /// 损坏/不可用的 sidecar manifest 是否升级为装载失败。
    ///
    /// **默认 `false`**（兼容既有配置）：[`PluginSource::plan_entry`] 报错时按
    /// "这一行没有计划"处理，并让**整次** manifest 预检保守跳过——混合来源里
    /// 读不到 sidecar 不等于"没有提供者"，跳过比误判安全。设为 `true` 时，
    /// 错误会带上行号、插件名与原因让整次装载失败。
    #[serde(default)]
    pub strict_sidecars: bool,
    /// 热重载。缺省关闭——生产宿主不该默认轮询文件系统。
    #[serde(default)]
    pub hmr: Option<HmrConfig>,
    /// 宿主注入的 `!!js` 求值引擎（**可选**；缺省用内置子集 [`crate::js::BuiltinJs`]）。
    ///
    /// 不参与序列化：它是**宿主注入的能力**，不是配置文件里的内容。
    /// 注入即获全量语义（覆盖内置子集）；两者都不认的表达式一律 fail loud
    /// （绝不静默降级）——M4-4 起这条纪律的落点是"决策期预检 + `internal/config` 链"。
    #[serde(skip, default)]
    pub js_evaluator: Option<Arc<dyn JsEvaluator>>,
    /// `!!js` 的求值模式（M9，D5）。缺省 `"builtin"`。
    #[serde(default)]
    pub interpolation: InterpolationMode,
    /// 本装载器所在 **App** 的派发看门狗（P0-2）。缺省不覆盖。
    ///
    /// 装载开始时按 `ctx.store` 的同一性登记：本装载器挂载的插件（以及同一 App
    /// 上之后注册的实例）使用它；其他 App 不受影响。只覆盖等待挂起结算，
    /// 不中断正在执行的同步 `apply`/回调。没有配置时回落到进程级
    /// [`crate::set_dispatch_policy`]。
    #[serde(default)]
    pub dispatch_policy: Option<DispatchPolicy>,
    /// 本装载器所在 **App** 的容量准入上限（P0-3）。缺省不覆盖。
    ///
    /// 装载开始时按 `ctx.store` 的同一性登记，作用于本装载器挂载的插件以及同一
    /// App 上之后注册的实例；其他 App 不受影响。这是宿主侧表项准入（实例数、
    /// 派发记录、挂起续延、流），不是每插件的内存/CPU 硬上限。没有配置时回落到
    /// 进程级 [`crate::set_default_host_limits`]；任何字段为 0 都会在挂载前报错。
    #[serde(default)]
    pub host_limits: Option<HostLimits>,
    /// **显式**的依赖计划（P1-1）：配置级声明，不是插件自述。
    ///
    /// 每一项给出 `id` / `version` / `provides` / `requires`；非空时，
    /// [`load_with_source`] 在**任何挂载之前**用 [`crate::plan::resolve`] 求解。
    /// 缺失、版本不可满足或同版本多提供者都会让本次装载**大声失败**，且不会
    /// 挂上/应用任何插件。
    ///
    /// 非空还意味着**覆盖**：装载器不会读取 [`PluginSource::plan_entry`] 收集的
    /// sidecar manifest 计划，也不会与它们合并。空（缺省）时才会逐行收集
    /// `PluginManifest` 的可选 `provides` / `requires`；没有任何 manifest 声明时
    /// 不做预检（既有配置零行为变化）。`PluginDesc` 自述仍不参与求解。
    #[serde(default)]
    pub dependency_plan: Vec<PlanEntrySpec>,
}

impl LoaderConfig {
    /// 嵌入方的稳定构造入口：只要求主配置路径，其余使用 fail-loud 默认值。
    /// 需要 patch / HMR / 自定义策略时再显式覆盖公开字段。
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            patches: Vec::new(),
            plugin_dir: None,
            strict: true,
            strict_cycles: false,
            strict_sidecars: false,
            hmr: None,
            js_evaluator: None,
            interpolation: InterpolationMode::Builtin,
            dispatch_policy: None,
            host_limits: None,
            dependency_plan: Vec::new(),
        }
    }
}

/// `!!js` 的求值模式（M9，D5，**P2**）。
///
/// 为什么需要它：内置子集是**本仓的兼容产物**（参考实现是 `new Function + with(ctx)`，
/// 读得到完整 ctx）。有些宿主希望"要么用我注入的引擎，要么谁都别求值"——
/// 让表达式在一个**宿主没审阅过**的解释器里跑，比报错危险得多。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterpolationMode {
    /// **缺省**：宿主注入的引擎优先；没注入就用内置子集（M4-4a 起的行为，逐字不变）。
    #[default]
    Builtin,
    /// 只用宿主注入的引擎；没注入 ⇒ 任何 `!!js` 都 fail loud（不悄悄回落到内置子集）。
    Injected,
}

impl InterpolationMode {
    /// 报告里的字面量（`loader.report.interpolation.mode`）。
    pub fn as_str(self) -> &'static str {
        match self {
            InterpolationMode::Builtin => "builtin",
            InterpolationMode::Injected => "injected",
        }
    }
}

/// 手写 `Debug`：`Arc<dyn JsEvaluator>` 不是 `Debug`，而配置结构可打印是基本礼貌
/// （`#[derive(Debug)]` 会因此编译不过）。
impl std::fmt::Debug for LoaderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoaderConfig")
            .field("path", &self.path)
            .field("patches", &self.patches)
            .field("plugin_dir", &self.plugin_dir)
            .field("strict", &self.strict)
            .field("strict_cycles", &self.strict_cycles)
            .field("strict_sidecars", &self.strict_sidecars)
            .field("hmr", &self.hmr)
            .field(
                "js_evaluator",
                &self.js_evaluator.as_ref().map(|_| "已注入"),
            )
            .field("interpolation", &self.interpolation)
            .field("dispatch_policy", &self.dispatch_policy)
            .field("host_limits", &self.host_limits)
            .field("dependency_plan", &self.dependency_plan)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HmrConfig {
    /// 轮询间隔（毫秒）。
    ///
    /// # 为什么是轮询而不是 inotify/FSEvents
    /// 跨平台的文件通知需要额外依赖（`notify`）与各自的边缘情况（编辑器"先写临时文件
    /// 再 rename"、网络盘不触发事件、事件可能丢失）。对一个插件宿主来说，
    /// 500ms 的轮询足够，而"少一个依赖、行为在三平台完全一致"更值钱。
    /// 真要换事件驱动，只需要替换这一个函数。
    #[serde(default = "default_interval")]
    pub interval_ms: u64,
}

fn default_strict() -> bool {
    true
}

fn default_interval() -> u64 {
    500
}

/// `!!js` 求值器：定义搬到了 [`crate::js`]（内置子集与它同处一个文件，
/// 免得"支持清单"和"谁在调用"分家）。这里重新导出，保住 `loader::JsEvaluator` 这条路径。
pub use crate::js::JsEvaluator;

/// volatile 声明的组合器（M4-5）：`declare_volatile(plugin_fn(…), &["data"])`。
///
/// 定义在 [`cordis_core::declare_volatile`]（它是 `Plugin` 的组合器，与 volatile 引用
/// 同处一个概念）；这里重新导出，让**只依赖 host** 的宿主（比如 demo）也能用上。
pub use cordis_core::declare_volatile;

// 真值判断（JS 的 `Boolean(x)`）：`disabled` 位用的就是它。定义在 `crate::js`，
// 这里改名导入是为了让本文件里的调用点读起来就是"JS 真值"这件事。
use crate::js::truthy as js_truthy;

fn validate(raw: Config) -> std::result::Result<Config, ValidationError> {
    let mut issues = Vec::new();
    if raw.get("path").and_then(|v| v.as_str()).is_none() {
        issues.push("path: required (at path)".to_string());
    }
    if issues.is_empty() {
        Ok(raw)
    } else {
        Err(ValidationError { issues })
    }
}

/// 一行**来自配置的哪一层**。
///
/// 配置回写必须落到行真正所在的那一层：主配置里的行写主配置；patch 层插入的行
/// 只能写回 patch 层。把合并树写回主配置会把 patch 插入的行固化进主配置，
/// 下一次读取时 patch 又会插一遍（重复挂载、禁用行复活，P1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowOrigin {
    Main,
    Patch,
}

/// 一份已挂载记录。配置回写与热重载共用它。
#[derive(Clone)]
struct Mounted {
    label: String,
    /// 这一行来自主配置还是 patch 层（配置回写的落点选择）。
    origin: RowOrigin,
    /// 用户提供的插件文件（不会被覆盖，只被读）。
    source: PathBuf,
    /// 实际加载的那个副本（Windows 上不能原地覆盖已加载的库）。
    staged: PathBuf,
    /// `(mtime_nanos, size)`：轮询比对用的指纹。
    ///
    /// `None` = **指纹未知**（假源、非文件来源）：此时这一行**永远不算陈旧**——
    /// 把"未知"当成"永远变了"会让每个轮询周期都去重挂一次（重试风暴）。
    fingerprint: Option<(u64, u64)>,
    config: Config,
    inject: Option<Config>,
    /// 弱引用：记录不该在插件卸载后把 fiber 吊住。
    fiber: Weak<Fiber>,
    /// 这一行的**派生挂载上下文**（M4-1）：隔离域与 intercept 都在它上面。
    ///
    /// 热重载必须用它重挂——用根 ctx 重挂等于悄悄丢掉隔离域，把行搬回默认域。
    scope: Ctx,
    /// 挂载时的**行声明**（M4-3 的 diff 基准）。
    ///
    /// 只比 `name`/`inject`/`isolate`/`intercept`（身份与装配面）；`config` 以
    /// [`Mounted::config`] 为准（插件驱动的更新经提交观察者推进它，raw 声明不会跟着变）。
    declared: Entry,
    /// 挂载时的**派生上下文输入**（链上每层的 `isolate`/`intercept`）：变了就得换实例。
    scope_inputs: Vec<ScopeInput>,
    /// 这一行的插件**声明的 volatile 路径**（M4-5）：`None` = 不参与 volatile。
    ///
    /// 缓存它（而不是每次决策去问插件）有两个理由：① 对账是同步的、纯的；
    /// ② ABI 插件的声明要**跨边界查询**（`OP_VOLATILE_PATHS`），不该出现在每轮 diff 里。
    volatile: Option<Vec<String>>,
    /// 当前版本的插件对象。
    ///
    /// **持有它是为了回滚**：新版本 `apply` 失败时，旧实例已经拆掉了，
    /// 只有手上还留着这个 `Arc` 才能把它原样装回来。
    /// （另一条路是重新 `open()` 旧 `staged` 文件，但那要先等 `FreeLibrary`
    /// 把文件解锁，在 Windows 上是一次不必要的赌博——留着 Arc 就不必赌。）
    plugin: Arc<dyn Plugin>,
    /// 已暂存副本的 SHA-256（WP2 变体身份）。`None` = 非文件来源（内建/假源），
    /// 报告里填 `"builtin"`；文件来源但 hash 失败时填 `"unavailable: ..."`。
    sha256: Option<String>,
    /// 挂载时是否是**文件来源**（有指纹）。不能用报告时的 `fingerprint(源路径)`
    /// 反推：源文件在换副本与报告之间可能被移走，那会把 `builtin` 误报给文件插件。
    file_source: bool,
}

mod file_io;
use file_io::*;

/// 一行未就绪（Pending）的诊断。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRow {
    pub label: String,
    pub plugin: String,
    pub missing: Vec<cordis_core::PendingInject>,
}

/// 带故障分类的装载失败（供 [`PluginSource::load_classified`] 与装载器按类别独立处置）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadFailure {
    pub category: FailureCategory,
    pub message: String,
}

impl LoadFailure {
    pub fn new(category: FailureCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self::new(FailureCategory::RuntimeUnavailable, message)
    }

    pub fn source(message: impl Into<String>) -> Self {
        Self::new(FailureCategory::Source, message)
    }

    pub fn integrity(message: impl Into<String>) -> Self {
        Self::new(FailureCategory::Integrity, message)
    }
}

impl std::fmt::Display for LoadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 一条结构化的已知失败行记录（供 `loader.report`、启动策略审计与下游 Pending 关联使用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedRow {
    pub label: String,
    pub plugin: String,
    pub category: FailureCategory,
    pub startup: RowStartup,
    pub degraded: bool,
    pub provides: Vec<String>,
    pub message: String,
}

impl FailedRow {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "label": self.label,
            "plugin": self.plugin,
            "category": self.category.as_str(),
            "startup": self.startup.as_str(),
            "degraded": self.degraded,
            "provides": self.provides,
            "message": self.message,
        })
    }
}

/// 行的隔离域链类型别名（只读，不分配新 realm）。
type IsolateScope = Arc<Scope<Box<str>, RealmId>>;

/// 失败行内部台账项：保留公开 [`FailedRow`] 及该行隔离域链（只读，不分配新 realm），
/// 供 [`LoaderState::correlated_pending_report`] 按精确同域可见性过滤上游候选。
#[derive(Clone)]
struct FailedRowRecord {
    row: FailedRow,
    isolate: Option<IsolateScope>,
}

/// 单个下游缺失依赖键关联到的已知失败上游行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingUpstreamFailure {
    pub label: String,
    pub plugin: String,
    pub category: FailureCategory,
    pub startup: RowStartup,
    pub degraded: bool,
    pub matched_key: String,
    pub message: String,
}

impl PendingUpstreamFailure {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "label": self.label,
            "plugin": self.plugin,
            "category": self.category.as_str(),
            "startup": self.startup.as_str(),
            "degraded": self.degraded,
            "matchedKey": self.matched_key,
            "message": self.message,
        })
    }
}

/// 单个缺失依赖及其关联到的上游失败行列表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorrelatedPendingInject {
    pub inject: PendingInject,
    pub failed_upstream: Vec<PendingUpstreamFailure>,
}

/// 带上游失败关联诊断的 Pending 行（保留 0.1 [`PendingRow`] 字段不变，见 ADR 0006）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorrelatedPendingRow {
    pub label: String,
    pub plugin: String,
    pub missing: Vec<CorrelatedPendingInject>,
}

/// 判断单条故障是否应按策略降级（不阻断启动）。
///
/// - `RuntimeUnavailable`：行级 `Optional` 或 `strict = false` 时降级；
/// - `Source` / `Integrity`：安装 [`StartupPolicy`] 时严格服从各自独立处置开关（忽略 `Optional` 与 `strict`）；
///   未安装时 `strict = true` 一律 fail-closed（含 `Optional` 行），`strict = false` 保留 0.1 诊断降级兼容；
/// - `PersistedState`：未安装 [`StartupPolicy`] 时始终 fail-closed；安装后服从 `policy.persisted_state()`。
fn should_degrade_failure(
    policy: Option<&StartupPolicy>,
    strict: bool,
    category: FailureCategory,
    startup: RowStartup,
) -> bool {
    match category {
        FailureCategory::RuntimeUnavailable => startup == RowStartup::Optional || !strict,
        FailureCategory::Source => match policy {
            Some(policy) => policy.source() == FailureDisposition::Degrade,
            None => !strict,
        },
        FailureCategory::Integrity => match policy {
            Some(policy) => policy.integrity() == FailureDisposition::Degrade,
            None => !strict,
        },
        FailureCategory::PersistedState => match policy {
            Some(policy) => policy.persisted_state() == FailureDisposition::Degrade,
            None => false,
        },
    }
}

/// 把服务键拆为 `(base, Option<major>)`（仅当 `@` 后为纯 ASCII 数字时视为版本后缀）。
fn split_service_major(key: &str) -> (&str, Option<&str>) {
    if let Some((base, suffix)) = key.rsplit_once('@') {
        if !base.is_empty() && !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            return (base, Some(suffix));
        }
    }
    (key, None)
}

/// 匹配失败上游声明的 `provided` 键与下游缺失的 `missing_key`。
///
/// 精确匹配优先；若一侧带 `@<major>` 而另一侧为裸名则按同 base 匹配；
/// 两侧均显式带 `@<major>` 且主版本不同（如 `db@2` vs `db@1`）时不误关联。
fn matches_declared_provide(provided: &str, missing_key: &str) -> bool {
    if provided == missing_key {
        return true;
    }
    let (p_base, p_major) = split_service_major(provided);
    let (m_base, m_major) = split_service_major(missing_key);
    p_base == m_base && (p_major.is_none() || m_major.is_none() || p_major == m_major)
}

/// 校验失败上游行与下游 `Pending` 消费者在同一服务键上是否落入同一个隔离域（零分配只读检查）。
fn same_realm_visibility(
    upstream: Option<&Scope<Box<str>, RealmId>>,
    consumer: &Scope<Box<str>, RealmId>,
    provided_key: &str,
    missing_key: &str,
) -> bool {
    let Some(upstream) = upstream else {
        return true;
    };
    let u_realm = upstream
        .get(provided_key)
        .or_else(|| upstream.get(missing_key))
        .copied();
    let c_realm = consumer
        .get(missing_key)
        .or_else(|| consumer.get(provided_key))
        .copied();
    u_realm == c_realm
}

/// 一次对账的产物（M4-2 的 `SyncOutcome` 在 M4-3 长成了动作表）。
///
/// 每个字段都是**行标**列表：谁被挂了、谁被卸了、谁被换了实例、谁的配置被推进了、
/// 谁的 volatile 值被原地提交了、谁被监听者短路了、谁的更新被否决了、谁失败了。
/// `report.reconcile`（M4-6）就读它。
#[derive(Debug, Default, Clone)]
pub struct ReconcileOutcome {
    /// 本次**新**挂上的行（按期望态顺序）。
    pub mounted: Vec<String>,
    /// 本次卸下的行（按字典序，便于断言）。
    pub disposed: Vec<String>,
    /// 换实例的行（`name`/`inject`/`isolate`/`intercept` 或祖先隔离域变了）。
    pub replaced: Vec<String>,
    /// 只推进配置、没有重挂的行。
    pub updated: Vec<String>,
    /// **只差 volatile 字段**的行：值原地提交，实例没动（M4-5）。
    pub volatile: Vec<String>,
    /// 动作被 `loader/patch-context` 的监听者短路（尾链没跑到）。
    ///
    /// 只有 `Mount`/`Update` 会落在这里：它们的短路等于"什么都没发生"。`Replace` 的
    /// 短路意味着**旧实例已经卸下**（先卸后装），所以它进 [`Self::failures`]（O5b）。
    pub skipped: Vec<String>,
    /// `internal/update` 被否决（`UpdateOutcome::Vetoed`）⇒ 内存里记着、实例上没落地。
    pub lagging: Vec<String>,
    /// 逐行失败消息（`<label>: <原因>`，与装载失败通道同格式）。
    ///
    /// `Replace` 的失败分两档，文案必须能区分开（O5b）：
    /// - **artifact 预检失败**（O5b-2：写错 `name` / artifact 缺失、stage/open/ping 报错）⇒
    ///   拆卸根本没发生，文案写"保留当前实例"；
    /// - 预检通过之后（`apply` 失败 / 被 `loader/patch-context` 短路）⇒ 旧实例已经卸下、
    ///   不回滚，文案写明"旧实例已卸下"。
    ///
    /// 后者与 HMR 路径上"保留当前实例"的错误文案含义正好相反，所以措辞不能共用。
    pub failures: Vec<String>,
}

impl ReconcileOutcome {
    /// 真的下手了几件事（日志与 `report.reconcile.applied` 用它）。
    pub fn applied(&self) -> usize {
        self.mounted.len()
            + self.disposed.len()
            + self.replaced.len()
            + self.updated.len()
            + self.volatile.len()
    }

    /// 什么都没发生（相等 ⇒ 空计划）。
    pub fn is_noop(&self) -> bool {
        self.applied() == 0
            && self.skipped.is_empty()
            && self.lagging.is_empty()
            && self.failures.is_empty()
    }
}

/// 对账的**动作表**（M4-3 规格 3）：期望态与现状之差。
///
/// 相等 ⇒ 不产生动作（没有 `Keep`：计划里只列"要动手的事"）。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 期望态里有、装载表里没有。
    Mount(String),
    /// 装载表里有、期望态里没有（移除 / 被禁用 / 决策失败）。
    Dispose(String),
    /// 只有 `config` 变了：`fiber.update(config, no_save=true)`，不重挂。
    Update { label: String, config: Config },
    /// **只差 volatile 路径**（M4-5）：把新值原地提交进引用，**不重挂**、
    /// 不发 `loader/partial-dispose`、不走 `loader/patch-context`。
    ///
    /// 之所以是独立动作而不是 `Update` 的一个标志：它对上层是**另一种事**
    /// （实例继续活着），报告与测试都要看得见这个区别。
    Volatile { label: String, config: Config },
    /// 身份或上下文变了（`name`/`inject`/`isolate`/`intercept`）：先预检、再卸、再装。
    ///
    /// 顺序是**决策 → 前移预检 → 拆卸 → 挂载**（O5b-2）：artifact 的 stage/open/ping
    /// 在旧实例还活着时跑完，预检失败就不拆（旧实例继续服务），预检通过才先卸后装。
    Replace(String),
}

/// `loader/patch-context` 尾链的执行结果（**以尾链是否真的跑过为准**）。
#[derive(Debug, Clone, PartialEq)]
enum ActionOutcome {
    Done,
    /// `internal/update` 否决了这次更新。
    Vetoed,
    Failed {
        failure: LoadFailure,
        provides: Vec<String>,
    },
}

/// 一次"文件 → 对账"的结局。
#[derive(Debug, Clone)]
pub enum ReloadOutcome {
    /// 文件正在被写（两遍读不一致）：这一轮不采纳，下一轮再看。
    Unsettled,
    /// 读取或解析失败：**保留上一快照**，只记警告（编辑中的半截文件是常态）。
    Failed(String),
    /// 这次调用**没有应用**自己的候选：已并入**在飞**的那一轮对账（只有最新的一份树
    /// 会被应用），或候选因**代次更旧 / 回写版本过期**被丢弃（更新的意图已经存在，
    /// 或者它基于一棵已被运行时回写改过的树——后者会按磁盘重建一次，
    /// 见 [`LoaderState::reload`]）。
    Coalesced,
    /// 本次调用驱动完成的对账。
    Reconciled(ReconcileOutcome),
}

/// 装载器看到的配置文件集合（主配置 + patch 层；可为相对或绝对路径）。
#[derive(Debug, Clone)]
struct ConfigFiles {
    main: PathBuf,
    patches: Vec<PathBuf>,
}

/// 一组配置文件的内容快照（稳读的输入与产物）。
#[derive(Debug, Clone, PartialEq)]
struct FileSnapshot {
    main: String,
    patches: Vec<String>,
}

/// [`LoaderState::parse_snapshot`] 的产物：目标树 + **随它一起被采纳**的回写元数据。
///
/// 两件事必须是一份：回写落到哪一层（`owners`）、以哪份文本做比较（`baseline`）、
/// 主配置自己长什么样（`main`）——都是"这份配置被采纳了"的一部分。
struct ParsedSnapshot {
    entries: Vec<Entry>,
    main: Vec<Entry>,
    main_startup: Vec<StartupNode>,
    row_startup: HashMap<String, RowStartup>,
    owners: PatchOwners,
    baseline: String,
}

/// 磁盘候选构造失败的强类型分类：区分持久化状态错误（保留精确失败文件路径）与依赖预检失败。
#[derive(Debug)]
enum BuildCandidateError {
    Persisted { label: String, message: String },
    Preflight(String),
}

impl BuildCandidateError {
    fn into_message(self) -> String {
        match self {
            Self::Persisted { message, .. } | Self::Preflight(message) => message,
        }
    }
}

/// 一份**待采纳**的配置候选：目标树与回写元数据一起排队、一起校验、一起提交。
///
/// 为什么必须整份带着走：`main_only` / `owners` / `main_baseline` 决定**之后的回写**
/// 落到哪个文件、以哪份文本做比较。解析完就提前写它们，会让一份永远不会被采纳的配置
/// 参与回写决策——预检失败或候选过期之后，插件自我卸载与 `fiber.update` 会按
/// "没生效的配置"操作磁盘。
struct Candidate {
    /// 登记代次（[`LoaderState::next_candidate_gen`]）：**请求登记顺序**，不是解析完成顺序。
    ///
    /// `0` 表示本地候选（`sync` / `set_disabled` / 显式 `reconcile_once`）：它们在
    /// `mutation` 下立即执行，不参与代次比较，但仍校验 `base_writeback` 防止覆盖并发回写。
    gen: u64,
    /// 候选构造时的**回写版本**（[`LoaderState::writeback`]）。
    ///
    /// 运行时回写（`fiber.update` / 插件自我卸载）会改已采纳的目标；基于旧版本的候选
    /// 一旦采纳就会把树退回候选内容，而 watcher 的指纹早已对齐磁盘、不会自动补偿。
    base_writeback: u64,
    /// 目标条目树（主配置 + patch 合并结果）。
    entries: Vec<Entry>,
    /// 是否连同回写元数据一起采纳（文件驱动的候选）。
    ///
    /// `set_disabled` / `sync` / 显式 `reconcile_once` 这类**本地**候选只换树：
    /// 它们不改文件基线，硬塞一份元数据反而会把已采纳的回写状态退回去。
    adopt_metadata: bool,
    /// 主配置自己的条目树（`adopt_metadata` 时生效）。
    main: Vec<Entry>,
    /// 主配置自己的行级启动策略树（`adopt_metadata` 时生效）。
    main_startup: Vec<StartupNode>,
    /// 合并树展平后的行级启动策略表（`adopt_metadata` 时生效）。
    row_startup: HashMap<String, RowStartup>,
    /// `config` / `disabled` 的字段归属（决定回写落到哪一层 patch）。
    owners: PatchOwners,
    /// 候选对应的主配置原文（回写前的"未改动"比较基线）。
    baseline: String,
}

impl Candidate {
    /// 本地候选：只换树，不动回写元数据、不参与代次比较，但仍校验 `base_writeback`。
    fn local(entries: Vec<Entry>, base_writeback: u64) -> Self {
        Self {
            gen: 0,
            base_writeback,
            entries,
            adopt_metadata: false,
            main: Vec::new(),
            main_startup: Vec::new(),
            row_startup: HashMap::new(),
            owners: PatchOwners::default(),
            baseline: String::new(),
        }
    }

    /// 排队候选：带登记代次与构造时的回写版本，参与"新旧"与"过期"判断。
    fn queued(entries: Vec<Entry>, gen: u64, base_writeback: u64) -> Self {
        Self {
            gen,
            ..Self::local(entries, base_writeback)
        }
    }
}

/// 一次对账内核的完整产物：对账结果 + 采纳被拒的原因（没有拒绝时为 `None`）。
struct ReconcileAttempt {
    outcome: ReconcileOutcome,
    rejection: Option<AdoptRejection>,
}

/// 候选被 `adopt` 拒绝的**原因**：驱动器要区分"能重建"与"已经过期得没救"。
///
/// 两者都不得采纳，但后续动作相反：
/// - 代次更旧 ⇒ 更新的意图已经在路上，重建只会白白覆盖它 ⇒ **直接丢弃**；
/// - 回写版本过期 ⇒ 候选本身仍是**当前意图**（文件还是那份文件），只是构造时看到的
///   树已经被运行时回写改过了 ⇒ 必须**重新读取/构造并预检**（规格 §O5），
///   否则 watcher 的指纹早已对齐磁盘、文件不变就不会再来一次 `reload`——
///   用户那次编辑会永久停在"只剩一条 Error 日志"。
enum AdoptRejection {
    /// 代次 ≤ 已采纳代次：迟到的旧解析，丢弃（不得复活）。
    StaleGeneration(String),
    /// 构造时回写版本 ≠ 当前：基于一棵已经被运行时回写改过的树。
    StaleWriteback(String),
}

impl AdoptRejection {
    fn message(&self) -> &str {
        match self {
            AdoptRejection::StaleGeneration(message) | AdoptRejection::StaleWriteback(message) => {
                message
            }
        }
    }
}

/// 同一份候选因**回写版本过期**被拒后，最多重建几次。
///
/// 每次重建都重新稳读磁盘并快照新的回写版本，所以持续的重建意味着"重建期间又发生了
/// 运行时回写"——那不是我们能在这一轮解决的竞态。上限把它变成一条可诊断的失败
/// （日志 + `failures`），而不是无限循环。
const MAX_STALE_REBUILDS: u32 = 3;

/// 一次过期候选重建的结局（[`LoaderState::rebuild_stale_candidate`]）。
enum StaleRebuild {
    /// 新候选已登记进合并槽：调用方继续用同一个单飞位驱动它。
    Queued,
    /// 这一轮拿不到可驱动的候选：附带的说明要进 `failures`（不得静默）。
    Stopped(String),
}

/// 一行**派生挂载上下文的输入**：链上每层的 label + `isolate`/`intercept` 声明。
///
/// 它就是"挂载上下文"这个函数的自变量：只要它变了，行的 ctx 原型链就变了，
/// 已挂的实例必须换（本仓 `Replace` 的判据之一）。祖先的 `config`/`disabled`
/// **不在**这里——前者是子条目列表（对子行无影响），后者会以 Mount/Dispose 的形式出现在 diff 里。
#[derive(Debug, Clone, PartialEq)]
struct ScopeInput {
    label: String,
    isolate: Option<Config>,
    intercept: Option<Config>,
}

/// 行级生命周期唯一真源状态机（单把 `LoaderState::rows` 锁原子跃迁）。
///
/// 取代原本分散在 `mounted` / `disposing` / `self_disposing` / `draining` 四个独立集合里的标记，
/// 通过显式互斥态在结构上消灭跨锁竞态、取消漏记 draining、以及自我卸载与 reload/replace 互踩。
enum RowSlot {
    /// 正在 `mount_loaded` 挂入并等待 `settle().await`。
    Mounting { ticket: u64 },
    /// 正常已挂载行（仅此状态对外表现为已挂载）。
    Mounted(Box<Mounted>),
    /// 装载器有意卸载/热替换中（`dispose_row` / `exec_replace_action` / `finish_reload_swap`）。
    Disposing { ticket: u64 },
    /// 插件自我卸载已由 `on_plugin` 原子认领，正在异步回写 `tree`/`disabled: true` 落盘。
    SelfDisposing { ticket: u64 },
    /// 旧实例（或被取消挂载/卸载的实例）仍在后台清理中；清理未完成前阻断旁路重挂。
    Draining {
        fiber: Arc<Fiber>,
        ticket: u64,
        /// `true` 表示后台任务正在主动跑 `fiber.dispose().await`，必须等该任务完成并摘表；
        /// `false` 表示由 `dispose_with_report` 超时转入，只要 `pending_background_cleanups() == 0` 即可摘除。
        wait_task: bool,
    },
}

enum DisposeClaim {
    /// 成功从 `Mounted` 原子跃迁至 `Disposing`，持有取消安全事务守卫。
    Claimed(RowTransactionGuard),
    /// 该行正处于自我卸载（或期望的旧实例在并发窗口内已因自我卸载消失）。
    SelfDisposing,
    /// 该行仍有旧实例在后台清理中。
    Draining,
    /// 该行当前未挂载（且不存在冲突态）。
    NotMounted,
}

impl DisposeClaim {
    fn into_claimed(self) -> Option<RowTransactionGuard> {
        match self {
            Self::Claimed(guard) => Some(guard),
            _ => None,
        }
    }
}

/// 单行事务守卫（挂载 / 卸载 / 热替换共用）：
/// 任何正常完成路径必须调用 `commit_mounted` / `finish_empty` / `finish_draining` 解除武装；
/// 一旦 future 在 `.await` 期间被取消或 panic，`Drop` 会原子把槽位推进到 `RowSlot::Draining`
/// 并派发后台 `fiber.dispose().await`，且因为声明在外层 `mutation` 锁守卫之后，
/// 析构必然发生在释放 `mutation` 锁之前。
struct RowTransactionGuard {
    state: Arc<LoaderState>,
    label: String,
    ticket: u64,
    active_fiber: Option<Arc<Fiber>>,
    armed: bool,
}

impl RowTransactionGuard {
    fn set_active_fiber(&mut self, fiber: Option<Arc<Fiber>>) {
        self.active_fiber = fiber;
    }

    fn active_fiber(&self) -> Option<Arc<Fiber>> {
        self.active_fiber.clone()
    }

    fn commit_mounted(mut self, record: Mounted) {
        self.armed = false;
        self.active_fiber = None;
        let mut rows = self.state.rows.lock().unwrap();
        rows.insert(self.label.clone(), RowSlot::Mounted(Box::new(record)));
    }

    fn finish_empty(mut self) {
        self.armed = false;
        self.active_fiber = None;
        self.state.clear_slot_if_ticket(&self.label, self.ticket);
    }

    fn finish_draining(mut self, fiber: Arc<Fiber>) {
        self.armed = false;
        self.active_fiber = None;
        self.state
            .enter_draining_for_ticket(&self.label, Some(self.ticket), fiber, false);
    }
}

impl Drop for RowTransactionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(fiber) = self.active_fiber.take() {
            self.state
                .enter_draining_for_ticket(&self.label, Some(self.ticket), fiber, true);
        } else {
            self.state.clear_slot_if_ticket(&self.label, self.ticket);
        }
    }
}

/// 装载器的运行时状态。
///
/// # 为什么它必须是显式类型
/// "运行期启停"要求**装载之后**还能改一行的 `disabled` 并立刻装卸——这需要条目树、
/// 装载表、派生域缓存与修订号落在同一处（M4-2 之前它们是 `load()` 的局部量，
/// 于是"装完就定死"）。热重载与对账状态机（M4-3）共用同一份状态。
type PendingSelfDispose = (Arc<Fiber>, String, Option<Mounted>, u64);

pub struct LoaderState {
    /// 装载器自己的上下文：派生行 ctx、写日志、发事件都以它为根。
    ///
    /// 存进状态而不是每次传参：对账的公开签名（`plan`/`reconcile_once`）不该再带一个
    /// **可以传错**的 ctx——M4-2 的 `sync(&ctx)` 就是这种 footgun。
    ctx: Ctx,
    /// 归一化后的条目树（主配置 + patch 合并结果；对账的输入）。
    tree: Mutex<Vec<Entry>>,
    /// **主配置自己的**条目树（patch 叠加之前，配置回写的唯一来源）。
    ///
    /// `tree` 是合并结果，绝不能写回主配置（P1：否则 patch 插入的行被固化、
    /// 下次又插一遍 ⇒ 重复 / 禁用行复活）。热重载时随新主配置刷新。
    main_only: Mutex<Vec<Entry>>,
    /// 与 `main_only` 一一对应的行级启动策略树（`persist` 回写主配置时保真）。
    main_startup: Mutex<Vec<StartupNode>>,
    /// 当前已采纳树展平后的行级启动策略（`label -> RowStartup`）。
    row_startup: Mutex<HashMap<String, RowStartup>>,
    /// 主配置初始基线是否有效（仅当主配置损坏且 `StartupPolicy.persisted_state == Degrade` 时为 `false`，
    /// 防止运行时回写覆盖磁盘上尚未修复的原始配置文件）。
    main_baseline_valid: AtomicBool,
    /// 行级状态机唯一真源：label → [`RowSlot`]（`Mounting` / `Mounted` / `Disposing` / `SelfDisposing` / `Draining`）。
    rows: Mutex<HashMap<String, RowSlot>>,
    /// 单调递增的行槽位票号，用于精准匹配 `Mounting` / `Disposing` / `SelfDisposing` / `Draining` 释放点。
    row_ticket: AtomicU64,
    /// 后台 `Draining` 槽位清空通知，供取消补偿收敛等待旧实例清理完成后再执行 `sync()`。
    draining_changed: tokio::sync::Notify,
    /// 变更序列的**互斥闸门**（对账、同步、启停、单行重载共用）。
    ///
    /// 这些操作都会经过"读现状 → 算动作 → 改实例 → 改记录"的窗口；没有互斥，
    /// 两个窗口交错就会双挂载或让刚卸下的行复活。用 `tokio::sync::Mutex` 而不是
    /// 标准锁：它必须跨 `.await` 持有（mount/dispose 都是异步的）。
    mutation: tokio::sync::Mutex<()>,
    /// 派生域缓存（M4-1）：私有域必须跨 remount 稳定，所以它跟着状态走。
    scopes: Mutex<RowScopes>,
    revision: AtomicU64,
    /// 配置文件的真相来源（主配置 + patch 层）。
    files: ConfigFiles,
    /// 插件动态库的搜索目录。
    search: Vec<PathBuf>,
    /// 稳读的两遍之间等多久（编辑器的"写临时文件再 rename"就靠它消化）。
    settle: Duration,
    /// 主配置 + patch 层的指纹（watcher 只比对它，不必每轮解析）。
    config_fingerprint: Mutex<Vec<Option<(u64, u64)>>>,
    /// `config` / `disabled` 最后一次被哪一层 patch 覆盖（运行期回写落点）。
    owners: Mutex<PatchOwners>,
    /// 待应用的最新一份候选（`request_reconcile` 的合并槽：旧的那份会被覆盖）。
    ///
    /// 存**整份候选**（树 + 回写元数据）而不是只存树：采纳是一次事务，
    /// 只有整份候选一起被采纳，"已采纳的配置"才只有一个含义。
    pending: Mutex<Option<Candidate>>,
    /// 是否有一轮对账在飞（重入保护）。
    reconciling: AtomicBool,
    /// 候选代次取号器。取号发生在**请求登记时**（`reload` 在解析之前），所以
    /// "谁是最新意图"由请求先后决定，而不是由谁先解析完决定。
    next_gen: AtomicU64,
    /// 最近一次被采纳的候选代次。迟到的旧解析（代次更小）不得复活。
    adopted_gen: AtomicU64,
    /// 回写版本：`persist` 成功写盘、提交观察者与自我卸载**改了已采纳目标**时推进。
    ///
    /// 在飞/排队的候选记下构造时的版本；版本变了就说明候选基于一棵已经过时的树，
    /// 采纳它会悄悄退回运行时回写的结果（watcher 的指纹已对齐磁盘，不会补偿）。
    writeback: AtomicU64,
    /// 插件自我卸载在同步 `on_plugin` 阶段暂存的 `(fiber, label, record, ticket)`，
    /// 供 `on_plugin_dispose_async` 在 `spawn_blocking` 中持 `config_gate` 完成落盘而不阻塞 Tokio worker。
    pending_self_disposes: Mutex<Vec<PendingSelfDispose>>,
    /// 最近一次真正执行过的对账产物（`report.reconcile` 与测试用）。
    last: Mutex<Option<ReconcileOutcome>>,
    /// 最近一次热重载的结构化报告（`report.lastReload` 与 [`LoaderState::last_reload`]）。
    last_reload: Mutex<Option<ReloadReport>>,
    /// 已知失败行的结构化台账（`label -> FailedRowRecord`），用于 `loader.report` 与同域下游 Pending 关联。
    failed_rows: Mutex<HashMap<String, FailedRowRecord>>,
    /// 持久化状态错误台账（配置/patch 读取解析降级、回写冲突/落盘失败等）。
    persisted_errors: Mutex<Vec<FailedRow>>,
    /// 最近一次声明预检中从已验证 sidecar manifest 收集到的每行 `provides` 声明（`label -> keys`）。
    preflight_provides: Mutex<HashMap<String, Vec<String>>>,
    /// 最近一次声明预检的分类结论（O7，`report.preflight`）。
    ///
    /// 与 `last` 同构：**只做诊断**，不参与任何门禁。`None` 只可能出现在"还没跑过
    /// 预检"的实例上（crate 内部直接构造的测试状态）。
    preflight: Mutex<Option<PreflightReport>>,
    /// 最近一次对账里**休眠**的叶子行标（M4-6：报告要能回答"我配了它，为什么没生效"）。
    disabled: Mutex<Vec<String>>,
    /// `loader.report` 出版过没有：装载期的第一次对账发生在 `provide` **之前**，
    /// 所以刷新要用这个开关挡住（`set` 一个没 provide 过的名字本来就是错误）。
    published: AtomicBool,
    /// 主配置最近一次**成功解析**时的原始文本。
    ///
    /// 运行时回写（插件自我卸载 / 配置提交）不能覆盖一份“磁盘上已改、但还没被
    /// 装载器采纳”的外部编辑；写盘前必须拿它做比较。
    main_baseline: Mutex<String>,
    /// 主配置快照解析与回写共用的一把同步闸门。
    ///
    /// 没有它，`persist` 可能在“dump 内存树”和“比较基线”之间被 `parse_snapshot`
    /// 插进来：基线推进到新文件、写回的却是旧树，外部编辑仍会被覆盖。所有
    /// `main_only` / `main_baseline` 的读写都在这里串行化（纯同步、无 await）。
    config_gate: Mutex<()>,
    source: Arc<dyn PluginSource>,
    /// **生效的**求值器（M4-4）：宿主注入的引擎，或内置子集；外面套着计数代理。
    evaluator: Arc<dyn JsEvaluator>,
    /// `!!js` 求值失败的累计次数（`loader.report.interpolation.failed`）。
    ///
    /// 累计而不是"本次对账"：它是诊断量，问的是"这套配置一共让插值失败过几次"。
    interp_failures: Arc<AtomicU64>,
    /// **生效的** `!!js` 模式（M9，D5）：报告要如实说出"这份配置现在由谁在解释"。
    interpolation_mode: InterpolationMode,
    /// 本装载器登记的派发策略（P0-2）：装载时已按 `ctx.store` 写进宿主注册表。
    ///
    /// 留一份在这里是为了让装载器的诊断/调试面能回答"这次装载声明了什么"；
    /// 实际生效值在宿主侧按 App 保存，且只作用于之后注册的实例。
    dispatch_policy: Option<DispatchPolicy>,
    /// 本装载器登记的容量准入上限（P0-3）：与 `dispatch_policy` 同一套快照语义。
    host_limits: Option<HostLimits>,
    /// 显式依赖计划（P1-1）。非空时覆盖 sidecar 收集；对账/重载前重新预检。
    dependency_plan: Vec<PlanEntrySpec>,
    /// `LoaderConfig.strict` 快照（与行级 `RowStartup` 和 [`StartupPolicy`] 共同决定降级）。
    strict: bool,
    /// `plan_entry` 报错时的策略（`LoaderConfig.strict_sidecars`）：对账/重载沿用同一份。
    strict_sidecars: bool,
    /// 初装已经预检过；第一次 reconcile 不再重复问 source。
    skip_next_preflight: AtomicBool,
    /// 测试闸门（`#[cfg(test)]` 每实例一份：不用全局 static，release 构建零成本）。
    ///
    /// 用它把"解析完了但还没登记""预检完了但还没采纳"这两个瞬间钉成确定性的，
    /// 于是"迟到的旧解析""回写在飞插入""采纳前被取消"都能被断言而不是靠 sleep 竞态。
    #[cfg(test)]
    hooks: AdoptHooks,
    /// `#[cfg(test)]`：**没持** `config_gate` 就写 `main_only` 的次数。
    ///
    /// 锁序纪律不能只写在注释里：这条计数让"写入与采纳交织"变成一个可断言的数字。
    /// release 构建没有这个字段。
    #[cfg(test)]
    ungated_main_writes: AtomicU64,
    /// `#[cfg(test)]`：过期候选按磁盘重建的次数（断言"有限重试"用）。
    #[cfg(test)]
    stale_rebuilds: AtomicU64,
}

/// 一次对账的**推进点**上的测试闸门（`#[cfg(test)]`）。
///
/// 每个点是独立的 `Option<Arc<TestGate>>`：测试装一个、用一次、再
/// [`LoaderState::clear_hooks`] 摘掉，后续轮次不会被同一个闸门绊住。
#[cfg(test)]
#[derive(Default)]
struct AdoptHooks {
    /// `reload` 解析完成之后（还没预检、没登记）。
    after_parse: Mutex<Option<Arc<TestGate>>>,
    /// `reconcile_once_inner` 的预检之后、采纳之前（已持 `mutation`）。
    before_adopt: Mutex<Option<Arc<TestGate>>>,
    /// `adopt` 的 `config_gate` 段内：校验通过、写树之前（**持闸门、同步**）。
    inside_gate: Mutex<Option<Arc<BlockGate>>>,
    /// `ReconcileDriver::drop` 里：候选已回填、单飞位**还没**放开（**同步回调**）。
    ///
    /// 这个瞬间就是"回填 → 清位"的两条指令之间：此刻登记的更新候选，它的
    /// `drive_pending` 仍会读到 `true` 而返回 `None`。回调而不是闸门：`Drop` 不能
    /// await，而阻塞一个 worker 会连带卡住 timer driver。
    before_release_flight: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// 提交观察者**已进入**的到达信号（每实例一份 `Semaphore`，不是闸门）。
    ///
    /// 观察者不在这里停：报到后继续去取 `config_gate`。测试用它把"回写任务真的跑到
    /// 观察者里了"变成一个可观测的前提——否则"回写还没完成"只能靠墙钟猜，慢 CI 上
    /// "任务还没被调度"会被误读成"被闸门挡住了"（假绿）。
    commit_entered: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// `reload_one` 里**暂存之后、打开暂存副本之前**（O9）。
    ///
    /// 回调而不是闸门：这个瞬间没有 `.await` 点（持着 `mutation`、流水线是同步的），
    /// 而测试要做的动作（用别的字节覆盖 `.rN`）本身也是同步的。回调在 `open_staged_verified`
    /// 之前**就地**执行，于是"校验的到底是哪个字节"变成一个确定性的断言，而不是
    /// 靠 sleep 去赌竞态窗口。
    before_staged_open: Mutex<Option<StagedOpenHook>>,
}

/// 暂存副本落地之后、被打开之前的同步回调（O9，`#[cfg(test)]`）。
#[cfg(test)]
type StagedOpenHook = Arc<dyn Fn(&Path) + Send + Sync>;

#[cfg(test)]
impl AdoptHooks {
    async fn pass_after_parse(&self) {
        let gate = self.after_parse.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.pass().await;
        }
    }

    async fn pass_before_adopt(&self) {
        let gate = self.before_adopt.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.pass().await;
        }
    }

    /// 同步版：`adopt` 的 `config_gate` 段里没有 `.await` 点可用，只能用阻塞闸门。
    fn pass_inside_gate(&self) {
        let gate = self.inside_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.pass();
        }
    }

    /// 同步版：`Drop` 里没有 `.await` 点可用；也**不能阻塞**——堵住一个 runtime worker
    /// 会把 timer driver 一起卡住（同 runtime 里的 `sleep` 再也不推进）。
    fn pass_before_release_flight(&self) {
        let hook = self.before_release_flight.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// 同步版：提交观察者入口报到（只给许可、不阻塞）。
    fn pass_commit_entered(&self) {
        let signal = self.commit_entered.lock().unwrap().clone();
        if let Some(signal) = signal {
            signal.add_permits(1);
        }
    }

    /// 同步版：暂存副本已落地、还没被打开（O9）。回调拿到的是**那个** `.rN` 路径。
    fn pass_before_staged_open(&self, staged: &Path) {
        let hook = self.before_staged_open.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(staged);
        }
    }
}

/// 一进一出的测试闸门：`pass()` 到达并等放行；测试侧 `wait_arrived()` + `release()`。
///
/// 两个 `Semaphore` 而不是 `Notify`：`add_permits` 会**记住**许可，于是
/// "先放行后到达"也不会丢信号（`Notify::notify_waiters` 会）。
#[cfg(test)]
struct TestGate {
    arrived: tokio::sync::Semaphore,
    released: tokio::sync::Semaphore,
}

#[cfg(test)]
impl TestGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            arrived: tokio::sync::Semaphore::new(0),
            released: tokio::sync::Semaphore::new(0),
        })
    }

    async fn pass(&self) {
        self.arrived.add_permits(1);
        let permit = self.released.acquire().await.expect("闸门未关闭");
        permit.forget();
    }

    async fn wait_arrived(&self) {
        let permit = self.arrived.acquire().await.expect("闸门未关闭");
        permit.forget();
    }

    fn release(&self) {
        self.released.add_permits(1);
    }
}

/// [`LoaderState::install_hook`] 能装的推进点（`#[cfg(test)]`）。
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum HookPoint {
    AfterParse,
    BeforeAdopt,
    /// `adopt` 的 `config_gate` 段内：**校验通过、写树之前**（持闸门、同步）。
    InsideAdoptGate,
}

/// **阻塞式**测试闸门：给"持 `config_gate`、不含 `.await`"的同步临界区用。
///
/// [`TestGate`] 只能在 `.await` 点停住；而 TOCTOU 的窗口（校验返回 → 写树）里没有
/// await 点，要在其中停住只能用阻塞原语：到达信号仍走 `Semaphore`（非阻塞，测试侧
/// 照旧 `.await`），放行则用 `Condvar` 唤醒被阻塞的工作线程。
#[cfg(test)]
struct BlockGate {
    arrived: tokio::sync::Semaphore,
    released: std::sync::Mutex<bool>,
    cond: std::sync::Condvar,
}

#[cfg(test)]
impl BlockGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            arrived: tokio::sync::Semaphore::new(0),
            released: std::sync::Mutex::new(false),
            cond: std::sync::Condvar::new(),
        })
    }

    /// 同步侧：报到后阻塞，直到测试放行（`release` 会记住已放行）。
    fn pass(&self) {
        self.arrived.add_permits(1);
        let mut open = self.released.lock().unwrap();
        while !*open {
            open = self.cond.wait(open).unwrap();
        }
    }

    async fn wait_arrived(&self) {
        let permit = self.arrived.acquire().await.expect("闸门未关闭");
        permit.forget();
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.cond.notify_all();
    }
}

/// 对账驱动者的取消安全闸门。
///
/// `reconciling` 是“有一轮在飞”的单飞位；它必须由**持有驱动的那个 future**
/// 在正常结束或**被丢弃**时都复位。否则一次 `timeout` / abort 会把标志永久
/// 留在 `true`，后续 `request_reconcile` 全部返回 `None`（“已合并”），
/// 但再也没有人把 `pending` 里的树应用掉。
struct ReconcileDriver {
    state: Arc<LoaderState>,
    active: bool,
    /// 已 take 但**尚未落定**的候选：取消/超时时回填合并槽（不丢、也不静默丢弃）。
    taken: Option<Candidate>,
    /// 本轮候选是否已通过 `adopt()` 写入 `tree` 且动作循环尚未跑完：
    /// 若在此时被取消，必须安排一次补偿收敛 `sync()`，防止 `tree` 与运行时实例永久漂移。
    adopted_in_progress: Arc<AtomicBool>,
}

impl ReconcileDriver {
    fn new(state: Arc<LoaderState>) -> Self {
        Self {
            state,
            active: true,
            taken: None,
            adopted_in_progress: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 正常收尾：候选已经落定（采纳或**明确拒绝**），不再回填。
    fn finish(&mut self) {
        self.active = false;
        self.taken = None;
        self.adopted_in_progress.store(false, Ordering::SeqCst);
    }
}

impl Drop for ReconcileDriver {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // 取消/超时/panic：先把候选还回合并槽，**再**释放单飞位。
        //
        // 顺序不能反：先放开 `reconciling` 的话，别人可能进来 take 到空槽并收工，
        // 紧接着我们才回填——那份候选就再也没人驱动了（幽灵滞后）。
        let needs_convergence = self.adopted_in_progress.load(Ordering::SeqCst);
        // 代次先记下来：清位之后要靠它辨认"槽里那份还是不是我们这份"。
        let held_gen = self.taken.as_ref().map(|candidate| candidate.gen);
        let restored = if needs_convergence {
            // 候选已通过 `adopt()` 提交进 `tree` 且 `adopted_gen` 已推进，不再回填旧候选；
            // 清位后通过补偿收敛驱动把未跑完的动作推到位。
            self.taken.take();
            None
        } else {
            self.taken
                .take()
                .map(|candidate| self.state.restore_candidate(candidate))
        };
        // 测试停点：钉住"已回填、还没清位"的两条指令之间（R1 的窗口）。
        #[cfg(test)]
        self.state.hooks.pass_before_release_flight();
        self.state.reconciling.store(false, Ordering::SeqCst);
        // 三条都会永久滞留的路径，都必须安排**一次**补偿驱动把它带走：
        // ① 回填**被拒**（`false`）：槽里已经有一份我们没持有的更新候选；
        // ② 回填**成功**、但清位之后槽里已经**不是**我们这份了；
        // ③ `adopt()` 已提交 `tree` 但动作循环中途被取消：槽里没有更新候选，必须补一次收敛 `sync()`。
        if restored == Some(false) || self.state.has_foreign_pending(held_gen) {
            self.state.schedule_compensation();
        } else if needs_convergence {
            self.state.schedule_adopted_convergence();
        }
    }
}

/// 手写 `Debug`：状态里有 `Arc<dyn JsEvaluator>` 与几个锁，`#[derive]` 过不去；
/// 而"装载表现在挂着什么"正是排查时最想看的那一眼。
impl std::fmt::Debug for LoaderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoaderState")
            .field("mounted", &self.mounted_labels())
            .field("revision", &self.revision.load(Ordering::SeqCst))
            .field("config", &self.files.main)
            .field("interpolation_failed", &self.interpolation_failures())
            .field("dispatch_policy", &self.dispatch_policy)
            .field("host_limits", &self.host_limits)
            .field("dependency_plan", &self.dependency_plan)
            .finish()
    }
}

/// 计数代理（M4-4）：把"`!!js` 求值失败"这件事收成**一个数字**。
///
/// 为什么是代理而不是在各个调用点各加一次计数：失败点有三处（挂载决策的 `disabled`、
/// 决策期的 `config` 预检、`internal/config` 链上的真求值），将来还会有 M4-5 的 volatile。
/// 代理让"什么算一次插值失败"只有一个定义——`evaluate` 返回 `Err` 就算。
struct CountedEvaluator {
    inner: Arc<dyn JsEvaluator>,
    failures: Arc<AtomicU64>,
}

impl JsEvaluator for CountedEvaluator {
    fn evaluate(&self, expr: &str, ctx: &Ctx) -> std::result::Result<Config, String> {
        let result = self.inner.evaluate(expr, ctx);
        if result.is_err() {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

impl LoaderState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: Ctx,
        tree: Vec<Entry>,
        main_only: Vec<Entry>,
        main_startup: Vec<StartupNode>,
        row_startup: HashMap<String, RowStartup>,
        main_baseline_valid: bool,
        files: ConfigFiles,
        search: Vec<PathBuf>,
        settle: Duration,
        source: Arc<dyn PluginSource>,
        evaluator: Arc<dyn JsEvaluator>,
        interp_failures: Arc<AtomicU64>,
        interpolation_mode: InterpolationMode,
        owners: PatchOwners,
        main_baseline: String,
        dispatch_policy: Option<DispatchPolicy>,
        host_limits: Option<HostLimits>,
        dependency_plan: Vec<PlanEntrySpec>,
        strict: bool,
        strict_sidecars: bool,
    ) -> Self {
        let fingerprint = config_fingerprint(&files);
        Self {
            ctx,
            tree: Mutex::new(tree),
            main_only: Mutex::new(main_only),
            main_startup: Mutex::new(main_startup),
            row_startup: Mutex::new(row_startup),
            main_baseline_valid: AtomicBool::new(main_baseline_valid),
            rows: Mutex::new(HashMap::new()),
            row_ticket: AtomicU64::new(0),
            draining_changed: tokio::sync::Notify::new(),
            mutation: tokio::sync::Mutex::new(()),
            scopes: Mutex::new(RowScopes::default()),
            revision: AtomicU64::new(0),
            files,
            search,
            settle,
            config_fingerprint: Mutex::new(fingerprint),
            owners: Mutex::new(owners),
            pending: Mutex::new(None),
            reconciling: AtomicBool::new(false),
            next_gen: AtomicU64::new(0),
            adopted_gen: AtomicU64::new(0),
            writeback: AtomicU64::new(0),
            pending_self_disposes: Mutex::new(Vec::new()),
            last: Mutex::new(None),
            last_reload: Mutex::new(None),
            failed_rows: Mutex::new(HashMap::new()),
            persisted_errors: Mutex::new(Vec::new()),
            preflight_provides: Mutex::new(HashMap::new()),
            preflight: Mutex::new(None),
            disabled: Mutex::new(Vec::new()),
            published: AtomicBool::new(false),
            main_baseline: Mutex::new(main_baseline),
            config_gate: Mutex::new(()),
            source,
            evaluator,
            interp_failures,
            interpolation_mode,
            dispatch_policy,
            host_limits,
            dependency_plan,
            strict,
            strict_sidecars,
            skip_next_preflight: AtomicBool::new(false),
            #[cfg(test)]
            hooks: AdoptHooks::default(),
            #[cfg(test)]
            ungated_main_writes: AtomicU64::new(0),
            #[cfg(test)]
            stale_rebuilds: AtomicU64::new(0),
        }
    }

    /// 装/摘测试闸门（`#[cfg(test)]`）：装在哪个推进点由调用方决定。
    #[cfg(test)]
    fn install_hook(&self, hook: HookPoint) -> Arc<TestGate> {
        let gate = TestGate::new();
        let slot = match hook {
            HookPoint::AfterParse => &self.hooks.after_parse,
            HookPoint::BeforeAdopt => &self.hooks.before_adopt,
            HookPoint::InsideAdoptGate => {
                panic!("`InsideAdoptGate` 是同步闸门：用 `install_block_hook`")
            }
        };
        *slot.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// 装一个**阻塞式**闸门（`#[cfg(test)]`）：只能装在同步临界区里的推进点。
    #[cfg(test)]
    fn install_block_hook(&self, hook: HookPoint) -> Arc<BlockGate> {
        let gate = BlockGate::new();
        match hook {
            HookPoint::InsideAdoptGate => {
                *self.hooks.inside_gate.lock().unwrap() = Some(gate.clone())
            }
            HookPoint::AfterParse | HookPoint::BeforeAdopt => {
                panic!("`{hook:?}` 在 `.await` 点上：用 `install_hook`")
            }
        }
        gate
    }

    /// 装一个**同步回调**（`#[cfg(test)]`）：只能在 `Drop` 这种"既不能 await、也不该
    /// 阻塞 runtime 线程"的推进点上用。回调跑在驱动被丢弃的那个线程上，就在
    /// "候选已回填、单飞位还没放开"这两条指令之间。
    #[cfg(test)]
    fn install_release_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.hooks.before_release_flight.lock().unwrap() = Some(hook);
    }

    /// 装一个"提交观察者已进入"的**到达信号**（`#[cfg(test)]`）：只报到、不阻塞。
    ///
    /// 用 `Semaphore` 而不是 `Notify`：许可会被记住，于是"信号先到、测试后等"也不丢。
    #[cfg(test)]
    fn install_commit_entered(&self) -> Arc<tokio::sync::Semaphore> {
        let signal = Arc::new(tokio::sync::Semaphore::new(0));
        *self.hooks.commit_entered.lock().unwrap() = Some(signal.clone());
        signal
    }

    /// 装一个"暂存副本已落地、还没打开"的**同步回调**（`#[cfg(test)]`，O9）。
    ///
    /// 回调拿到的是 `.rN` 的路径：用它可以在 `open_staged_verified` 之前把副本
    /// 换成别的字节，于是"二次校验真的跑了"变成一个确定性的断言。
    #[cfg(test)]
    fn install_before_staged_open_hook(&self, hook: StagedOpenHook) {
        *self.hooks.before_staged_open.lock().unwrap() = Some(hook);
    }

    /// 摘掉所有测试闸门：已经卡在闸门里的那一轮仍由它自己持有的 `Arc` 放行。
    #[cfg(test)]
    fn clear_hooks(&self) {
        *self.hooks.after_parse.lock().unwrap() = None;
        *self.hooks.before_adopt.lock().unwrap() = None;
        *self.hooks.inside_gate.lock().unwrap() = None;
        *self.hooks.before_release_flight.lock().unwrap() = None;
        *self.hooks.commit_entered.lock().unwrap() = None;
        *self.hooks.before_staged_open.lock().unwrap() = None;
    }

    /// 已采纳的候选代次（`#[cfg(test)]` 只读观测）。
    #[cfg(test)]
    fn adopted_generation(&self) -> u64 {
        self.adopted_gen.load(Ordering::SeqCst)
    }

    /// 当前回写版本（`#[cfg(test)]` 只读观测）。
    #[cfg(test)]
    fn writeback_version(&self) -> u64 {
        self.writeback.load(Ordering::SeqCst)
    }

    /// 过期候选按磁盘重建的次数（`#[cfg(test)]` 只读观测；断言"有限重试"）。
    #[cfg(test)]
    fn stale_rebuild_count(&self) -> u64 {
        self.stale_rebuilds.load(Ordering::SeqCst)
    }

    /// 合并槽里待采纳候选的条目树（`#[cfg(test)]` 只读观测；`None` = 槽空）。
    #[cfg(test)]
    fn pending_entries(&self) -> Option<Vec<Entry>> {
        self.pending
            .lock()
            .unwrap()
            .as_ref()
            .map(|candidate| candidate.entries.clone())
    }

    /// 没持 `config_gate` 就写 `main_only` 的次数（`#[cfg(test)]` 只读观测）。
    #[cfg(test)]
    fn ungated_main_write_count(&self) -> u64 {
        self.ungated_main_writes.load(Ordering::SeqCst)
    }

    /// 对当前条目树重新做 P1-1 预检：显式计划覆盖 sidecar；
    /// 否则收集启用行的 manifest `provides` / `requires`。
    ///
    /// **只求解、不落报告**（O7）：结论由调用方在"这棵树真的成了当前树"之后才用
    /// [`Self::record_preflight`] 写进去。被 `adopt` 拒绝的候选、预检本身就失败的
    /// 那一轮都不该覆盖上一份已采纳结论。
    fn preflight_dependencies(
        &self,
        entries: &[Entry],
    ) -> std::result::Result<(PreflightReport, HashMap<String, Vec<String>>), String> {
        let row_startup = self.row_startup.lock().unwrap().clone();
        self.preflight_dependencies_with(entries, &row_startup)
    }

    fn preflight_dependencies_with(
        &self,
        entries: &[Entry],
        row_startup: &HashMap<String, RowStartup>,
    ) -> std::result::Result<(PreflightReport, HashMap<String, Vec<String>>), String> {
        if self.dependency_plan.is_empty() {
            let startup_policy = startup_policy_for(&self.ctx);
            preflight_source_dependency_plan(
                entries,
                &self.search,
                self.source.as_ref(),
                self.strict_sidecars,
                startup_policy.as_ref(),
                row_startup,
            )
        } else {
            Ok((
                PreflightReport::NotApplicable {
                    reason: PreflightNotApplicable::ExplicitPlan,
                },
                HashMap::new(),
            ))
        }
    }

    /// 记下最近一次**已采纳**树的声明预检结论（O7，纯诊断）以及已校验 sidecar 的 `provides` 声明。
    ///
    /// 调用点必须保证"这棵树是当前树"：初装（`load_with_source` 建状态时）、
    /// 对账 `adopt` 成功之后、`reload_one` 对当前树重跑预检之后。
    fn record_preflight(&self, report: PreflightReport, provides: HashMap<String, Vec<String>>) {
        *self.preflight.lock().unwrap() = Some(report);
        *self.preflight_provides.lock().unwrap() = provides;
    }

    fn next_row_ticket(&self) -> u64 {
        self.row_ticket.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn mounted_record_clone(&self, label: &str) -> Option<Mounted> {
        match self.rows.lock().unwrap().get(label) {
            Some(RowSlot::Mounted(record)) => Some(record.as_ref().clone()),
            _ => None,
        }
    }

    fn with_mounted_mut<T>(&self, label: &str, f: impl FnOnce(&mut Mounted) -> T) -> Option<T> {
        let mut rows = self.rows.lock().unwrap();
        match rows.get_mut(label) {
            Some(RowSlot::Mounted(record)) => Some(f(record)),
            _ => None,
        }
    }

    /// 当前装载表里的行标（排序）。诊断与断言都在用它——加载器不该只有日志这一种观测面。
    pub fn mounted_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, slot)| matches!(slot, RowSlot::Mounted(_)))
            .map(|(label, _)| label.clone())
            .collect();
        labels.sort();
        labels
    }

    /// 当前挂载行的 `(行标, 用户提供的 artifact 路径)`（排序）。
    ///
    /// `status` 用它从**原始 artifact**（而不是暂存副本）读 sidecar manifest 与
    /// SHA256；暂存副本的 `.rN` 名字旁边没有发布方写的 manifest。
    pub fn mounted_sources(&self) -> Vec<(String, PathBuf)> {
        let mut rows: Vec<(String, PathBuf)> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter_map(|slot| match slot {
                RowSlot::Mounted(record) => Some((record.label.clone(), record.source.clone())),
                _ => None,
            })
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// 诊断：装载表里每一行**是否指向活着的实例**。
    ///
    /// 这是对账路径的不变量：**记录先摘、后 dispose；先挂、后记录**——于是任何时刻
    /// 表里的记录都不会指向一个已经拆掉的实例（表不撒谎）。
    pub fn mounted_liveness(&self) -> Vec<(String, bool)> {
        let mut rows: Vec<(String, bool)> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(label, slot)| match slot {
                RowSlot::Mounted(record) => {
                    let alive = record
                        .fiber
                        .upgrade()
                        .map(|fiber| !fiber.is_disposed())
                        .unwrap_or(false);
                    Some((label.clone(), alive))
                }
                _ => None,
            })
            .collect();
        rows.sort();
        rows
    }

    /// 一行的根 fiber（诊断/测试用；没有实例时为 `None`）。
    pub fn fiber_of(&self, label: &str) -> Option<Arc<Fiber>> {
        match self.rows.lock().unwrap().get(label) {
            Some(RowSlot::Mounted(record)) => record.fiber.upgrade(),
            _ => None,
        }
    }

    /// 当前**插件文件已变**的行标（HMR 的重载队列，诊断/测试用）。
    ///
    /// `fingerprint` 为 `None` 的行（假源、非文件来源）永远不算陈旧——
    /// 把"未知"当成"永远变了"会让每个轮询周期都重挂一次。
    pub fn stale_labels(&self) -> Vec<String> {
        let candidates: Vec<_> = {
            let table = self.rows.lock().unwrap();
            table
                .values()
                .filter_map(|slot| match slot {
                    RowSlot::Mounted(record) if record.fingerprint.is_some() => Some((
                        record.label.clone(),
                        record.source.clone(),
                        record.fingerprint,
                    )),
                    _ => None,
                })
                .collect()
        };
        candidates
            .into_iter()
            .filter(|(_, source, expected)| fingerprint(source) != *expected)
            .map(|(label, _, _)| label)
            .collect()
    }

    /// 当前条目树的快照（只读诊断；对账的输入之一）。
    pub fn tree_snapshot(&self) -> Vec<Entry> {
        self.tree.lock().unwrap().clone()
    }

    /// 最近一次真正执行过的对账产物（`report.reconcile` 与测试用）。
    pub fn last_outcome(&self) -> Option<ReconcileOutcome> {
        self.last.lock().unwrap().clone()
    }

    /// 最近一次热重载的结构化报告（区分 `Active` / `Pending` / 回滚 / 拒绝与旧实例清理报告）。
    pub fn last_reload(&self) -> Option<ReloadReport> {
        self.last_reload.lock().unwrap().clone()
    }

    fn record_reload(&self, report: ReloadReport) {
        *self.last_reload.lock().unwrap() = Some(report);
    }

    /// `!!js` 求值失败的累计次数（M4-4：`loader.report.interpolation.failed`）。
    pub fn interpolation_failures(&self) -> u64 {
        self.interp_failures.load(Ordering::SeqCst)
    }

    /// **声明级依赖环**（M5，A6）：每个强连通分量一条**带环路径**（首尾同名）。
    ///
    /// 图是**声明**构成的：行 A → 行 B 当" A 声明的 inject 名字" ∩ " B 声明的 provision
    /// key" 非空。运行时 `provide()` 出来的东西在挂载前不可知，不参与——
    /// 论文 Thm 73 的 acyclicity 前提也是声明级的（Lemma 77 进入 Thm 80）。
    ///
    /// **默认只报告**（本仓不做裁决）：环里的行照旧挂上，自然停在 PENDING，
    /// 由内核的依赖机制兜着。`strict_cycles` 才是本仓扩展：装载期把环升级成失败
    /// （见 [`load_with_source`]）。
    pub fn cycle_paths(&self) -> Vec<Vec<String>> {
        // 节点：已挂载的行（label、inject 名字集合、provision key 集合）。
        let mut nodes: Vec<(String, Vec<String>, Vec<String>)> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter_map(|slot| match slot {
                RowSlot::Mounted(record) => Some(record),
                _ => None,
            })
            .map(|record| {
                let mut inject: Vec<String> = record
                    .plugin
                    .inject()
                    .0
                    .iter()
                    .map(|(name, _)| name.to_string())
                    .collect();
                if let Some(declared) = &record.inject {
                    collect_inject_names(declared, &mut inject);
                }
                inject.sort();
                inject.dedup();
                let mut provision = record.plugin.provision();
                provision.sort();
                provision.dedup();
                (record.label.clone(), inject, provision)
            })
            .collect();
        nodes.sort_by(|a, b| a.0.cmp(&b.0));
        let count = nodes.len();
        // 边：A → B（A 要消费的某个名字正好是 B 声明提供的）。自环也算环。
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (from, (_, inject, _)) in nodes.iter().enumerate() {
            for (to, (_, _, provision)) in nodes.iter().enumerate() {
                if inject
                    .iter()
                    .any(|name| provision.iter().any(|key| key == name))
                {
                    edges[from].push(to);
                }
            }
        }

        // Tarjan 的迭代版：SCC 大小 > 1，或大小 1 且有自环 ⇒ 一个环。
        // 迭代而不是递归：配置里的行数没有上限，栈溢出不该是"配置写大了"的后果。
        let mut index = vec![usize::MAX; count];
        let mut low = vec![0usize; count];
        let mut on_stack = vec![false; count];
        let mut stack: Vec<usize> = Vec::new();
        let mut next = 0usize;
        let mut cycles: Vec<Vec<String>> = Vec::new();
        for root in 0..count {
            if index[root] != usize::MAX {
                continue;
            }
            let mut work: Vec<(usize, usize)> = vec![(root, 0)];
            while let Some((node, edge)) = work.pop() {
                if edge == 0 {
                    index[node] = next;
                    low[node] = next;
                    next += 1;
                    stack.push(node);
                    on_stack[node] = true;
                }
                let mut descended = false;
                for (position, &next_node) in edges[node].iter().enumerate().skip(edge) {
                    if index[next_node] == usize::MAX {
                        work.push((node, position + 1));
                        work.push((next_node, 0));
                        descended = true;
                        break;
                    } else if on_stack[next_node] {
                        low[node] = low[node].min(index[next_node]);
                    }
                }
                if descended {
                    continue;
                }
                if low[node] == index[node] {
                    let mut component = Vec::new();
                    while let Some(top) = stack.pop() {
                        on_stack[top] = false;
                        component.push(top);
                        if top == node {
                            break;
                        }
                    }
                    component.sort_unstable();
                    let self_loop =
                        component.len() == 1 && edges[component[0]].contains(&component[0]);
                    if component.len() > 1 || self_loop {
                        let mut path: Vec<String> =
                            component.iter().map(|i| nodes[*i].0.clone()).collect();
                        path.push(path[0].clone()); // 首尾同名：一眼看出环回到谁
                        cycles.push(path);
                    }
                }
                if let Some((parent, _)) = work.last() {
                    low[*parent] = low[*parent].min(low[node]);
                }
            }
        }
        cycles.sort();
        cycles
    }

    /// **能力清单**（M5，A5 的数据面）：每行 `label` / `plugin` / `uid` /
    /// `sha256` / `inject` / `provision` / `volatile` / `js`，各项都可逐字断言。
    /// `js` 是这一行的 `!!js` 表达式**静态**读到的宿主面（M9，D5）：env 名 / 宿主事实 /
    /// `ctx.get` 的服务名——只有引用没有值（凭据绝不进清单）。
    /// 想看**挂载之前**的能力面（含还没挂的行）用
    /// [`crate::probe_capabilities`]——那是 A5 的机制。
    pub fn capabilities(&self) -> Vec<serde_json::Value> {
        // 行的 `!!js` 能力面来自**条目树**（挂载记录里只有解析后的配置，标记已经求过值），
        // 且必须按 `stable_label`（`id` 或 `name#N`）建索引，避免同名无 id 的多行互相覆盖。
        let caps_by_label: HashMap<String, crate::js::JsCapabilities> = {
            fn walk(
                entries: &[Entry],
                ordinals: &mut HashMap<String, usize>,
                seen: &mut HashSet<String>,
                out: &mut HashMap<String, crate::js::JsCapabilities>,
            ) {
                for entry in entries {
                    let label = stable_label(entry, ordinals);
                    // 与 `DesiredWalk::visit` 保持一致的"首行生效"语义：
                    // 重复 label（含重复 group 及其子行）直接跳过，不覆盖首行能力面，
                    // 也不推进重复组内的匿名序号。
                    if !seen.insert(label.clone()) {
                        continue;
                    }
                    if entry.group {
                        walk(&entry.children, ordinals, seen, out);
                    } else {
                        out.insert(label, crate::patch::entry_js_capabilities(entry));
                    }
                }
            }
            let tree = self.tree.lock().unwrap();
            let mut ordinals = HashMap::new();
            let mut seen = HashSet::new();
            let mut out = HashMap::new();
            walk(&tree, &mut ordinals, &mut seen, &mut out);
            out
        };
        let mut rows: Vec<serde_json::Value> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter_map(|slot| match slot {
                RowSlot::Mounted(record) => Some(record),
                _ => None,
            })
            .map(|record| {
                let mut inject: Vec<String> = record
                    .plugin
                    .inject()
                    .0
                    .iter()
                    .map(|(name, _)| name.to_string())
                    .collect();
                if let Some(declared) = &record.inject {
                    collect_inject_names(declared, &mut inject);
                }
                inject.sort();
                inject.dedup();
                let mut provision = record.plugin.provision();
                provision.sort();
                provision.dedup();
                let js = caps_by_label
                    .get(&record.label)
                    .cloned()
                    .unwrap_or_default()
                    .to_json();
                json!({
                    "label": record.label,
                    "plugin": record.plugin.name(),
                    "uid": record
                        .fiber
                        .upgrade()
                        .map(|fiber| fiber.uid())
                        .unwrap_or(UID_DISPOSED),
                    "sha256": record.sha256.clone().unwrap_or_else(|| "builtin".to_string()),
                    "inject": inject,
                    "provision": provision,
                    "schema": record.plugin.config_schema(),
                    "volatile": record.volatile.clone().unwrap_or_default(),
                    "js": js,
                })
            })
            .collect();
        rows.sort_by(|a, b| a["label"].as_str().cmp(&b["label"].as_str()));
        rows
    }

    /// 当前已知失败行的结构化列表（按行标升序）。
    pub fn failed_rows(&self) -> Vec<FailedRow> {
        let mut rows: Vec<FailedRow> = self
            .failed_rows
            .lock()
            .unwrap()
            .values()
            .map(|record| record.row.clone())
            .collect();
        rows.sort_by(|a, b| a.label.cmp(&b.label));
        rows
    }

    /// 当前被策略降级的失败行列表（按行标升序）。
    pub fn degraded_rows(&self) -> Vec<FailedRow> {
        self.failed_rows()
            .into_iter()
            .filter(|row| row.degraded)
            .collect()
    }

    /// 当前记录的持久化状态错误（配置/patch 读取解析降级、回写冲突/落盘失败等）。
    pub fn persisted_state_errors(&self) -> Vec<FailedRow> {
        self.persisted_errors.lock().unwrap().clone()
    }

    fn effective_row_startup(&self, label: &str, entry_id: Option<&str>) -> RowStartup {
        if let Some(policy) = startup_policy_for(&self.ctx) {
            if let Some(startup) = policy.row_startup(label, entry_id) {
                return startup;
            }
        }
        self.row_startup
            .lock()
            .unwrap()
            .get(label)
            .copied()
            .unwrap_or(RowStartup::Required)
    }

    fn declared_provides_for(
        &self,
        label: &str,
        entry: &Entry,
        plugin: Option<&Arc<dyn Plugin>>,
        previous: Option<&Arc<dyn Plugin>>,
    ) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(plugin) = plugin {
            keys.extend(plugin.provision());
        }
        if let Some(previous) = previous {
            keys.extend(previous.provision());
        }
        if let Some(cached) = self.preflight_provides.lock().unwrap().get(label) {
            keys.extend(cached.iter().cloned());
        }
        for spec in &self.dependency_plan {
            if spec.id == entry.name
                || entry.id.as_deref() == Some(spec.id.as_str())
                || spec.id == label
            {
                keys.extend(spec.provides.keys().cloned());
            }
        }
        // 显式 `dependency_plan` 是覆盖模式：非空时绝不回查 sidecar manifest。
        if keys.is_empty() && self.dependency_plan.is_empty() {
            if let Ok(Some(spec)) = self.source.plan_entry(entry, &self.search) {
                keys.extend(spec.provides.keys().cloned());
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    fn record_failed_row(&self, row: FailedRow, isolate: Option<IsolateScope>) {
        self.failed_rows
            .lock()
            .unwrap()
            .insert(row.label.clone(), FailedRowRecord { row, isolate });
    }

    fn clear_failed_row(&self, label: &str) {
        self.failed_rows.lock().unwrap().remove(label);
    }

    fn clear_slot_if_ticket(&self, label: &str, ticket: u64) {
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        let matches = matches!(
            rows.get(label),
            Some(
                RowSlot::Mounting { ticket: t }
                    | RowSlot::Disposing { ticket: t }
                    | RowSlot::SelfDisposing { ticket: t }
                    | RowSlot::Draining { ticket: t, .. }
            ) if *t == ticket
        );
        if matches {
            let was_draining = matches!(rows.remove(label), Some(RowSlot::Draining { .. }));
            drop(rows);
            if was_draining {
                self.draining_changed.notify_waiters();
            }
        }
    }

    /// 原子把某行推进到 `RowSlot::Draining`（可选校验旧票号），并派发仅持 `Weak<LoaderState>`
    /// 的后台清理监听任务：清理完成后按票号摘除槽位并唤醒等待者。
    fn enter_draining_for_ticket(
        self: &Arc<Self>,
        label: &str,
        expected_ticket: Option<u64>,
        fiber: Arc<Fiber>,
        run_dispose: bool,
    ) {
        if !run_dispose && fiber.pending_background_cleanups() == 0 {
            if let Some(ticket) = expected_ticket {
                self.clear_slot_if_ticket(label, ticket);
            }
            return;
        }
        let ticket = self.next_row_ticket();
        let rt = tokio::runtime::Handle::try_current().ok();
        let wait_task = run_dispose && rt.is_some();
        let installed = {
            let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
            let allowed = match expected_ticket {
                None => true,
                Some(exp) => match rows.get(label) {
                    None => true,
                    Some(RowSlot::Mounting { ticket: t } | RowSlot::Disposing { ticket: t }) => {
                        *t == exp
                    }
                    _ => false,
                },
            };
            if allowed {
                rows.insert(
                    label.to_string(),
                    RowSlot::Draining {
                        fiber: fiber.clone(),
                        ticket,
                        wait_task,
                    },
                );
                true
            } else {
                false
            }
        };
        if !installed {
            return;
        }
        if let Some(rt) = rt {
            let weak_state = Arc::downgrade(self);
            let label_owned = label.to_string();
            rt.spawn(async move {
                if run_dispose {
                    fiber.dispose().await;
                }
                fiber.wait_background_cleanups().await;
                if let Some(state) = weak_state.upgrade() {
                    state.clear_slot_if_ticket(&label_owned, ticket);
                }
            });
        }
    }

    /// 清理已经不再有后台任务的非 `wait_task` `Draining` 槽位。
    fn prune_finished_draining(&self) -> bool {
        let mut rows = self.rows.lock().unwrap();
        let mut still_draining = false;
        let mut removed_any = false;
        rows.retain(|_, slot| match slot {
            RowSlot::Draining {
                fiber, wait_task, ..
            } => {
                if *wait_task || fiber.pending_background_cleanups() > 0 {
                    still_draining = true;
                    true
                } else {
                    removed_any = true;
                    false
                }
            }
            _ => true,
        });
        drop(rows);
        if removed_any {
            self.draining_changed.notify_waiters();
        }
        still_draining
    }

    /// 等待当前所有处于 `Draining` 的行完成后台清理（取消补偿收敛前调用）。
    async fn wait_active_draining(&self) {
        loop {
            let notified = self.draining_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.prune_finished_draining() {
                return;
            }
            notified.await;
        }
    }

    /// 检查指定行是否仍有旧实例在后台清理未完成（若已完成则当场摘除）。
    fn is_row_draining(&self, label: &str) -> bool {
        let mut rows = self.rows.lock().unwrap();
        match rows.get(label) {
            Some(RowSlot::Draining {
                fiber, wait_task, ..
            }) => {
                if *wait_task || fiber.pending_background_cleanups() > 0 {
                    true
                } else {
                    rows.remove(label);
                    drop(rows);
                    self.draining_changed.notify_waiters();
                    false
                }
            }
            _ => false,
        }
    }

    fn is_row_self_disposing(&self, label: &str) -> bool {
        matches!(
            self.rows.lock().unwrap().get(label),
            Some(RowSlot::SelfDisposing { .. })
        )
    }

    /// 在单把 `rows` 锁内原子认领一个 `Mounted` 行进入 `Disposing` 态。
    ///
    /// 若提供了 `expected_fiber`，同时校验当前挂载实例正是同一 `Arc<Fiber>`，防止在预检/暂存
    /// 窗口内发生自我卸载或替换后误拆/复活新旧实例。
    fn claim_mounted_for_dispose(
        self: &Arc<Self>,
        label: &str,
        expected_fiber: Option<&Arc<Fiber>>,
    ) -> DisposeClaim {
        let mut rows = self.rows.lock().unwrap();
        match rows.get(label) {
            Some(RowSlot::Mounted(record)) => {
                let current_fiber = record.fiber.upgrade();
                if let Some(expected) = expected_fiber {
                    let matches = current_fiber
                        .as_ref()
                        .is_some_and(|cur| Arc::ptr_eq(cur, expected));
                    if !matches {
                        return DisposeClaim::SelfDisposing;
                    }
                }
                let ticket = self.next_row_ticket();
                rows.insert(label.to_string(), RowSlot::Disposing { ticket });
                DisposeClaim::Claimed(RowTransactionGuard {
                    state: self.clone(),
                    label: label.to_string(),
                    ticket,
                    active_fiber: current_fiber,
                    armed: true,
                })
            }
            Some(RowSlot::SelfDisposing { .. }) => DisposeClaim::SelfDisposing,
            Some(RowSlot::Draining {
                fiber, wait_task, ..
            }) => {
                if *wait_task || fiber.pending_background_cleanups() > 0 {
                    DisposeClaim::Draining
                } else {
                    rows.remove(label);
                    drop(rows);
                    self.draining_changed.notify_waiters();
                    if expected_fiber.is_some() {
                        DisposeClaim::SelfDisposing
                    } else {
                        DisposeClaim::NotMounted
                    }
                }
            }
            Some(RowSlot::Mounting { .. } | RowSlot::Disposing { .. }) => DisposeClaim::Draining,
            None => {
                if expected_fiber.is_some() {
                    // 预检前看到 `expected_fiber` 在位，预检后已从 `rows` 消失 ⇒ 并发自我卸载已落盘完成。
                    DisposeClaim::SelfDisposing
                } else {
                    DisposeClaim::NotMounted
                }
            }
        }
    }

    /// 原子登记一条行进入 `Mounting` 态，并返回取消安全事务守卫。
    fn begin_mount_slot(self: &Arc<Self>, label: &str, fiber: Arc<Fiber>) -> RowTransactionGuard {
        let ticket = self.next_row_ticket();
        self.rows
            .lock()
            .unwrap()
            .insert(label.to_string(), RowSlot::Mounting { ticket });
        RowTransactionGuard {
            state: self.clone(),
            label: label.to_string(),
            ticket,
            active_fiber: Some(fiber),
            armed: true,
        }
    }

    fn record_persisted_error(&self, label: &str, plugin: &str, message: String) {
        let policy = startup_policy_for(&self.ctx);
        let degraded = should_degrade_failure(
            policy.as_ref(),
            self.strict,
            FailureCategory::PersistedState,
            RowStartup::Required,
        );
        let entry = FailedRow {
            label: label.to_string(),
            plugin: plugin.to_string(),
            category: FailureCategory::PersistedState,
            startup: RowStartup::Required,
            degraded,
            provides: Vec::new(),
            message: message.clone(),
        };
        {
            let mut list = self.persisted_errors.lock().unwrap();
            list.retain(|existing| existing.label != label);
            list.push(entry);
        }
        let level = if degraded { Level::Warn } else { Level::Error };
        self.ctx
            .log(level, &format!("{label}: 持久化状态错误: {message}"));
        self.publish_report();
    }

    fn clear_persisted_error(&self, label: &str) {
        let changed = {
            let mut list = self.persisted_errors.lock().unwrap();
            let before = list.len();
            list.retain(|existing| existing.label != label);
            list.len() != before
        };
        if changed {
            self.publish_report();
        }
    }

    /// 当前未就绪的行及其精确缺失键（阶段 3 诊断面）。
    ///
    /// 只读、不建 fiber、不分配 realm；`related` 是全局注册表候选，
    /// 不证明在当前行的 isolate 域里可见。
    pub fn pending_report(&self) -> Vec<PendingRow> {
        let rows: Vec<(String, String, Weak<Fiber>)> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter_map(|slot| match slot {
                RowSlot::Mounted(record) => Some((
                    record.label.clone(),
                    record.plugin.name().to_string(),
                    record.fiber.clone(),
                )),
                _ => None,
            })
            .collect();
        let mut pending = Vec::new();
        for (label, plugin, fiber) in rows {
            let Some(fiber) = fiber.upgrade() else {
                continue;
            };
            let missing = fiber.pending_injects();
            if missing.is_empty() {
                continue;
            }
            pending.push(PendingRow {
                label,
                plugin,
                missing,
            });
        }
        pending.sort_by(|a, b| a.label.cmp(&b.label));
        pending
    }

    /// 当前未就绪的行及其精确缺失键，并将缺失键关联到同一隔离域内已知声明提供该键的失败上游行。
    ///
    /// 若某行当前已重新挂载且处于 `FiberState::Active`（例如热重载回滚后旧实例仍在服务），
    /// 不将其误报为下游 `Pending` 的阻塞上游；跨不同 `isolate` 域的失败行亦不会误关联。
    pub fn correlated_pending_report(&self) -> Vec<CorrelatedPendingRow> {
        let mounted_rows: Vec<(String, String, Weak<Fiber>, IsolateScope)> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter_map(|slot| match slot {
                RowSlot::Mounted(record) => Some((
                    record.label.clone(),
                    record.plugin.name().to_string(),
                    record.fiber.clone(),
                    record.scope.isolate.clone(),
                )),
                _ => None,
            })
            .collect();
        let mut active_labels: HashSet<String> = HashSet::new();
        let mut pending_rows: Vec<(PendingRow, IsolateScope)> = Vec::new();
        for (label, plugin, weak_fiber, isolate) in mounted_rows {
            let Some(fiber) = weak_fiber.upgrade() else {
                continue;
            };
            if fiber.state() == FiberState::Active {
                active_labels.insert(label);
                continue;
            }
            let missing = fiber.pending_injects();
            if missing.is_empty() {
                continue;
            }
            pending_rows.push((
                PendingRow {
                    label,
                    plugin,
                    missing,
                },
                isolate,
            ));
        }
        pending_rows.sort_by(|a, b| a.0.label.cmp(&b.0.label));

        let failed: Vec<FailedRowRecord> = self
            .failed_rows
            .lock()
            .unwrap()
            .values()
            .filter(|rec| !active_labels.contains(&rec.row.label) && !rec.row.provides.is_empty())
            .cloned()
            .collect();

        pending_rows
            .into_iter()
            .map(|(row, consumer_isolate)| {
                let missing = row
                    .missing
                    .into_iter()
                    .map(|inject| {
                        let mut failed_upstream: Vec<PendingUpstreamFailure> = failed
                            .iter()
                            .filter(|candidate| candidate.row.label != row.label)
                            .filter_map(|candidate| {
                                candidate
                                    .row
                                    .provides
                                    .iter()
                                    .find(|provided| {
                                        matches_declared_provide(provided, &inject.key)
                                            && same_realm_visibility(
                                                candidate.isolate.as_deref(),
                                                &consumer_isolate,
                                                provided,
                                                &inject.key,
                                            )
                                    })
                                    .map(|matched_key| PendingUpstreamFailure {
                                        label: candidate.row.label.clone(),
                                        plugin: candidate.row.plugin.clone(),
                                        category: candidate.row.category,
                                        startup: candidate.row.startup,
                                        degraded: candidate.row.degraded,
                                        matched_key: matched_key.clone(),
                                        message: candidate.row.message.clone(),
                                    })
                            })
                            .collect();
                        failed_upstream.sort_by(|a, b| a.label.cmp(&b.label));
                        CorrelatedPendingInject {
                            inject,
                            failed_upstream,
                        }
                    })
                    .collect();
                CorrelatedPendingRow {
                    label: row.label,
                    plugin: row.plugin,
                    missing,
                }
            })
            .collect()
    }

    /// **装载诊断面**（M4-6）：`loader.report` 的值，**每次调用都现算**。
    ///
    /// 每个字段回答一个运维问题（而不是"内部状态快照"）：
    /// - `mounted` / `disabled`：现在挂着几行、哪几行在休眠；
    /// - `volatile{declared,supported}`：多少行声明了 volatile、其中多少行现在走得了原地提交；
    /// - `interpolation{failed,mode}`：`!!js` 求值失败累计（M4-4）与**生效模式**（M9，D5）；
    /// - `reconcile{applied,skipped,lagging}`：最近一次对账下手/短路/滞后各几件；
    /// - `realms{default,named}`：默认域表条数与具名域数（两张表都只增不减，P2-14）；
    /// - `stagedLeaked`：已暂存但无人认领的副本文件数（G11，M0b）——装成功归零，
    ///   失败/泄漏 >0；
    /// - `pending`：未就绪行的精确缺失键与原因（阶段 3）；
    /// - `failed`：最近一次对账的**逐行失败消息**，外加"没执行就失败"的热重载
    ///   （准入/校验/预检/打开/探活被拒）——它不是"历史累计"，历史在日志里。
    /// - `preflight`：最近一次**声明预检**的分类结论（O7）。它明确标注
    ///   `scope=declaration` / `runtimeVerified=false` / `realmModel=notModeled`，
    ///   并用 `skipped=true` 表达"整次预检被跳过"：
    ///   [ADR 0007] 的立场是声明检查不做运行时保证，这里不出现"已绑定/已满足"的口径。
    ///
    /// 预检结论只描述**已采纳**的那棵树：被 `adopt` 拒绝的候选不会覆盖它，
    /// 于是 `preflight` 与 `mounted` / `disabled` 永远描述同一棵树。
    ///
    /// [ADR 0007]: ../../docs/adr/0007-version-is-diagnostic-only.md
    pub fn report(&self) -> serde_json::Value {
        let outcome = self.last.lock().unwrap().clone().unwrap_or_default();
        let (declared, supported) = self.volatile_summary();
        let preflight = self
            .preflight
            .lock()
            .unwrap()
            .as_ref()
            .map(PreflightReport::to_json)
            .unwrap_or_else(|| {
                // 还没跑过预检（crate 内部直接构造的测试状态）：没有任何声明结论。
                PreflightReport::NotApplicable {
                    reason: PreflightNotApplicable::NoEnabledRows,
                }
                .to_json()
            });
        let pending_json: Vec<serde_json::Value> = self
            .correlated_pending_report()
            .into_iter()
            .map(|row| {
                let missing = row
                    .missing
                    .into_iter()
                    .map(|item| {
                        let mut obj = json!({
                            "key": item.inject.key,
                            "reason": item.inject.reason.as_str(),
                            "related": item.inject.related,
                        });
                        if !item.failed_upstream.is_empty() {
                            obj["failedUpstream"] = json!(item
                                .failed_upstream
                                .iter()
                                .map(PendingUpstreamFailure::to_json)
                                .collect::<Vec<_>>());
                        }
                        obj
                    })
                    .collect::<Vec<_>>();
                json!({
                    "label": row.label,
                    "plugin": row.plugin,
                    "missing": missing,
                })
            })
            .collect();
        let mut report = json!({
            "mounted": self.mounted_labels().len(),
            "disabled": self.disabled.lock().unwrap().clone(),
            "volatile": { "declared": declared, "supported": supported },
            "interpolation": {
                "failed": self.interpolation_failures(),
                "mode": self.interpolation_mode.as_str(),
            },
            "reconcile": {
                "applied": outcome.applied(),
                "skipped": outcome.skipped.len(),
                "lagging": outcome.lagging.len(),
            },
            "realms": {
                "default": self.ctx.store.default_realm_count(),
                "named": self.ctx.store.named_realm_count(),
            },
            "stagedLeaked": crate::staged_leaked(),
            "cycles": self.cycle_paths(),
            "pending": pending_json,
            "capabilities": self.capabilities(),
            "failed": outcome.failures,
            "preflight": preflight,
        });
        let failed_rows = self.failed_rows();
        if !failed_rows.is_empty() {
            report["failedRows"] = json!(failed_rows
                .iter()
                .map(FailedRow::to_json)
                .collect::<Vec<_>>());
            let degraded: Vec<String> = failed_rows
                .iter()
                .filter(|row| row.degraded)
                .map(|row| format!("{}: {}", row.label, row.message))
                .collect();
            if !degraded.is_empty() {
                report["degraded"] = json!(degraded);
            }
        }
        let persisted_errors = self.persisted_state_errors();
        if !persisted_errors.is_empty() {
            report["persistedStateErrors"] = json!(persisted_errors
                .iter()
                .map(FailedRow::to_json)
                .collect::<Vec<_>>());
        }
        if let Some(last_reload) = self.last_reload.lock().unwrap().as_ref() {
            report["lastReload"] = last_reload.to_json();
        }
        report
    }

    /// volatile 的两个计数（M4-6）：`declared` = 有声明的**已挂载行数**；
    /// `supported` = 其中**实例还活着**的行数（现在就走得了原地提交的那种）。
    ///
    /// 两者会分开，正是要看的信号：有声明却没有活实例 ⇒ 下次对账会重挂它。
    fn volatile_summary(&self) -> (usize, usize) {
        let table = self.rows.lock().unwrap();
        let declared = table
            .values()
            .filter(|slot| matches!(slot, RowSlot::Mounted(record) if record.volatile.is_some()))
            .count();
        let supported = table
            .values()
            .filter(|slot| {
                matches!(
                    slot,
                    RowSlot::Mounted(record)
                        if record.volatile.is_some() && record.fiber.upgrade().is_some()
                )
            })
            .count();
        (declared, supported)
    }

    /// 把报告推给 `loader.report`（M4-6）：**每次对账后刷新**，让它是"现在的样子"。
    ///
    /// 装载期的第一次对账发生在 `provide` 之前，用 `published` 挡住；
    /// 之后用 `ctx.set`（M2-2）——`set` 换的是值、不通知依赖者，所以刷新不会引起重载风暴。
    fn publish_report(&self) {
        if !self.published.load(Ordering::SeqCst) {
            return;
        }
        // 收尾期不刷新、也不算失败：`loader.report` 的**提供者就是装载器自己**
        // （`load_with_source` 在 provide 之后才 `mark_report_published`），
        // 拆到一半（Unloading/Disposed）或依赖没齐（Pending）时这一写必然失败——
        // 那是正常时序，不是异常。只有"装载器还 ACTIVE 却写不进自己的报告"才是真问题。
        //
        // 与 `attach_self_dispose_observer` 的第 2 条判定同源：装载器自己不在 Active，
        // 整棵树就在关（或还没开），任何"发布"都没有意义。
        if self
            .ctx
            .fiber_opt()
            .is_none_or(|fiber| fiber.state() != cordis_core::FiberState::Active)
        {
            return;
        }
        if let Err(err) = self
            .ctx
            .set("loader.report", Dyn::new(ExternValue(self.report())))
        {
            self.ctx
                .log(Level::Warn, &format!("刷新 loader.report 失败: {err}"));
        }
    }

    /// 标记报告已出版（`load_with_source` 在 `provide` 之后调用）。
    pub fn mark_report_published(&self) {
        self.published.store(true, Ordering::SeqCst);
    }

    fn next_revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn label_of_fiber(&self, fiber: &Arc<Fiber>) -> Option<String> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find_map(|(label, slot)| match slot {
                RowSlot::Mounted(record)
                    if record
                        .fiber
                        .upgrade()
                        .is_some_and(|own| Arc::ptr_eq(&own, fiber)) =>
                {
                    Some(label.clone())
                }
                _ => None,
            })
    }

    /// 在单把 `rows` 锁内原子识别并认领插件自我卸载（`Mounted` → `SelfDisposing`）。
    ///
    /// 若该行当前已处于 `Disposing`（装载器有意卸载/换实例/HMR）或其他非 `Mounted` 态，
    /// 直接返回 `false`，绝不把有意卸载误判为自我卸载，也消除 `disposing` 与 `mounted` 之间的跨锁竞态。
    fn try_claim_self_dispose(&self, fiber: &Arc<Fiber>) -> bool {
        let claimed = {
            let mut rows = self.rows.lock().unwrap();
            let label = rows.iter().find_map(|(label, slot)| match slot {
                RowSlot::Mounted(record)
                    if record
                        .fiber
                        .upgrade()
                        .is_some_and(|own| Arc::ptr_eq(&own, fiber)) =>
                {
                    Some(label.clone())
                }
                _ => None,
            });
            let Some(label) = label else {
                return false;
            };
            let ticket = self.next_row_ticket();
            let Some(RowSlot::Mounted(record)) =
                rows.insert(label.clone(), RowSlot::SelfDisposing { ticket })
            else {
                unreachable!("validated RowSlot::Mounted under the same rows lock");
            };
            (label, record, ticket)
        };
        let (label, record, ticket) = claimed;
        self.pending_self_disposes.lock().unwrap().push((
            fiber.clone(),
            label,
            Some(*record),
            ticket,
        ));
        true
    }

    /// 运行期启停一行：`Some(true)` 禁用、`Some(false)` 显式启用、`None` 清掉选项。
    ///
    /// 语义与上游 `loader.update(entry, { disabled })` 一致
    /// （`loader/tests/group.spec.ts:52-80`）：目标是**组行**时整棵子树一起启停——
    /// 派生 disabled 沿祖先链上溯，不需要逐个子行去改。
    ///
    /// 只改内存条目树、**不落盘**：文件是真相，持久化纪律属于对账（`reconcile_once` 的
    /// 拒绝回滚 + 提交观察者的 `no_save` 分流）。
    pub async fn set_disabled(
        self: &Arc<Self>,
        label: &str,
        value: Option<bool>,
    ) -> std::result::Result<ReconcileOutcome, String> {
        // 整段"改树 → 对账"必须在互斥闸门内：否则并发的对账会中途覆盖树，
        // 刚禁用的行在下一轮里复活（或被挂成第二份）。
        let _guard = self.mutation.lock().await;
        let previous_last = self.last.lock().unwrap().clone();
        let (old_disabled, old_expr) = {
            let _gate = self.config_gate.lock().unwrap();
            let mut tree = self.tree.lock().unwrap();
            let entry = find_by_label(&mut tree, label)
                .ok_or_else(|| format!("没有 label 为 \"{label}\" 的配置行"))?;
            let old = (entry.disabled, entry.disabled_expr.clone());
            entry.disabled = value.unwrap_or(false);
            // 静态选项覆盖表达式：上游把选项设成布尔值时，表达式形态被整体替换。
            entry.disabled_expr = None;
            self.bump_writeback();
            old
        };
        let outcome = self.sync_inner().await;
        if !outcome.failures.is_empty() {
            // 若该行或其子行（目标为组行时）已在本次对账中被实际拆卸（例如停用时触发 `dispose_timeout_ms` 超时），
            // fiber 已进入不可逆的 Disposed 态且从 `mounted` 摘除，不能把 `tree` 回滚回启用态。
            let subtree_labels = collect_subtree_labels(&self.tree_snapshot(), label);
            let subtree_disposed = outcome
                .disposed
                .iter()
                .any(|disposed| subtree_labels.contains(disposed));
            if !subtree_disposed {
                // 组启用部分失败回滚时，先卸载本轮刚挂载成功的子行，防止回滚后在运行时遗留孤儿活实例。
                for newly_mounted in &outcome.mounted {
                    let _ = self.dispose_row(newly_mounted).await;
                }
                {
                    let _gate = self.config_gate.lock().unwrap();
                    if let Some(entry) = find_by_label(&mut self.tree.lock().unwrap(), label) {
                        entry.disabled = old_disabled;
                        entry.disabled_expr = old_expr;
                    }
                    self.bump_writeback();
                }
                // 回滚后对齐 `disabled`、`failed_rows` 与 `loader.report`，
                // 避免报告仍残留已被撤回的启停状态或已回滚为禁用行的失败台账。
                let restored = self.desired(&self.tree_snapshot());
                *self.disabled.lock().unwrap() = restored.disabled;
                let enabled_labels: HashSet<String> =
                    restored.rows.into_iter().map(|row| row.label).collect();
                let restored_issues = assign_issue_keys(restored.issues);
                let restored_issue_keys: HashSet<&str> = restored_issues
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect();
                self.failed_rows.lock().unwrap().retain(|row_label, _| {
                    enabled_labels.contains(row_label)
                        || restored_issue_keys.contains(row_label.as_str())
                });
                *self.last.lock().unwrap() = previous_last;
                self.publish_report();
            }
            return Err(format!(
                "启停 {label} 未生效（对账失败）: {}",
                outcome.failures.join("；")
            ));
        }
        Ok(outcome)
    }

    /// **纯差异**：不下手，只列动作（`plan_reaches_fresh_load_state` 与可视化用它）。
    ///
    /// 它也是"目标要求挂载、当前却没有实例"的**诊断入口**（O5b）：换实例失败一轮之后
    /// （先卸后装 ⇒ 实例没了、目标不回滚），`plan(&state.tree_snapshot())` 会明确给出
    /// `Action::Mount`，配合 `report().failed` / `mounted == 0` 就足够判断"它该在、但它不在"。
    /// 所以这里不需要额外的公开字段。
    pub fn plan(&self, entries: &[Entry]) -> Vec<Action> {
        let desired = self.desired(entries);
        self.diff(&desired.rows)
    }

    /// 以**当前条目树**为目标对账一次（运行期启停走它）。
    pub async fn sync(self: &Arc<Self>) -> ReconcileOutcome {
        let _guard = self.mutation.lock().await;
        self.sync_inner().await
    }

    /// [`sync`] 的无锁内核：调用方已经持有 `mutation`。
    async fn sync_inner(self: &Arc<Self>) -> ReconcileOutcome {
        // 本地候选：在同一 `config_gate` 临界区内原子快照 `(tree, writeback)`。
        // 若在预检/对账期间发生并发回写（`fiber.update` / `mark_self_disposed`），
        // `adopt` 会返回 `StaleWriteback`，此处按最新树重采重试（有界），绝不用旧快照覆盖回写。
        for attempt_idx in 0..=MAX_STALE_REBUILDS {
            let (entries, base_writeback) = {
                let _gate = self.config_gate.lock().unwrap();
                (self.tree.lock().unwrap().clone(), self.writeback())
            };
            let candidate = Candidate::local(entries, base_writeback);
            let attempt = self.reconcile_attempt(&candidate).await;
            if matches!(attempt.rejection, Some(AdoptRejection::StaleWriteback(_)))
                && attempt_idx < MAX_STALE_REBUILDS
            {
                continue;
            }
            return attempt.outcome;
        }
        unreachable!("loop returns on final attempt")
    }

    /// 以**给定的新树**为目标对账一次（文件驱动的唯一驱动器）。
    ///
    /// # 依赖预检
    /// P1-1 的 sidecar manifest 计划（以及显式 `dependencyPlan`）在初装、对账和
    /// `reload_one` 前都会重新求解。预检失败时对账不采纳新树、不卸载已有实例，
    /// 只把问题写进对账产物。
    pub async fn reconcile_once(self: &Arc<Self>, entries: Vec<Entry>) -> ReconcileOutcome {
        let _guard = self.mutation.lock().await;
        let base_writeback = self.writeback();
        let candidate = Candidate::local(entries, base_writeback);
        self.reconcile_once_inner(&candidate).await
    }

    /// [`reconcile_once`] 的无锁内核：调用方已经持有 `mutation`。
    async fn reconcile_once_inner(self: &Arc<Self>, candidate: &Candidate) -> ReconcileOutcome {
        self.reconcile_attempt(candidate).await.outcome
    }

    /// [`reconcile_once_inner`] 的完整形态：多交出**采纳被拒的原因**。
    ///
    /// 只有驱动器需要它——"代次过期"直接丢弃，而"回写版本过期"要重建候选，
    /// 光看 `ReconcileOutcome` 里那条失败消息无法可靠地区分两者（字符串不是契约）。
    #[allow(clippy::too_many_arguments)]
    fn handle_row_failure(
        &self,
        label: &str,
        entry: &Entry,
        isolate: Option<IsolateScope>,
        category: FailureCategory,
        message: String,
        provides: Vec<String>,
        outcome: &mut ReconcileOutcome,
        log_prefix: &str,
        level_override: Option<Level>,
    ) {
        let startup = self.effective_row_startup(label, entry.id.as_deref());
        let policy = startup_policy_for(&self.ctx);
        let degraded = should_degrade_failure(policy.as_ref(), self.strict, category, startup);
        let level = level_override.unwrap_or(if degraded { Level::Warn } else { Level::Error });
        self.ctx
            .log(level, &format!("{label}: {log_prefix}{message}"));
        self.record_failed_row(
            FailedRow {
                label: label.to_string(),
                plugin: entry.name.clone(),
                category,
                startup,
                degraded,
                provides,
                message: message.clone(),
            },
            isolate,
        );
        let record_in_outcome = !degraded
            || (policy.is_none()
                && !self.strict
                && (startup == RowStartup::Required
                    || category != FailureCategory::RuntimeUnavailable));
        if record_in_outcome {
            outcome.failures.push(format!("{label}: {message}"));
        }
    }

    async fn reconcile_attempt(self: &Arc<Self>, candidate: &Candidate) -> ReconcileAttempt {
        self.reconcile_attempt_with_flag(candidate, None).await
    }

    async fn reconcile_attempt_with_flag(
        self: &Arc<Self>,
        candidate: &Candidate,
        adopted_flag: Option<&AtomicBool>,
    ) -> ReconcileAttempt {
        // P1-1：HMR/patch/启停触发的对账也要在挂载前重跑依赖预检。
        // 初装已经在创建状态前预检过，第一次 reconcile 用标记跳过，避免重复访问 source。
        //
        // O7：结论**先扣着**，等 `adopt` 真的采纳了这棵树才写进报告。候选可能随后被判
        // 过期拒绝，那时树还是旧的——把被丢弃候选的结论写进 `report.preflight`，会让
        // 报告里"声明走到了哪一步"与 `mounted`/`disabled`（当前树）描述两棵不同的树。
        let mut accepted_preflight = None;
        if !self.skip_next_preflight.swap(false, Ordering::SeqCst) {
            let row_startup = if candidate.adopt_metadata {
                candidate.row_startup.clone()
            } else {
                self.row_startup.lock().unwrap().clone()
            };
            match self.preflight_dependencies_with(&candidate.entries, &row_startup) {
                Ok(pair) => accepted_preflight = Some(pair),
                Err(err) => {
                    // 与"采纳被拒"同构：门禁失败也要进 `last` 并刷新报告，
                    // 否则 `report.failed` 只表现为"什么都没有"，答案只剩日志里一句。
                    let failure = format!("依赖预检失败: {err}");
                    self.ctx
                        .log(Level::Error, &format!("依赖预检失败（对账未执行）: {err}"));
                    let outcome = ReconcileOutcome {
                        failures: vec![failure],
                        ..ReconcileOutcome::default()
                    };
                    *self.last.lock().unwrap() = Some(outcome.clone());
                    self.publish_report();
                    return ReconcileAttempt {
                        outcome,
                        rejection: None,
                    };
                }
            }
        }
        #[cfg(test)]
        self.hooks.pass_before_adopt().await;
        // 期望态与动作表**先算出来**：这一步会求值 `!!js`、派生上下文（可能分配私有域）。
        let desired = self.desired(&candidate.entries);
        let actions = self.diff(&desired.rows);
        // 采纳：**同一同步临界区**内校验并提交整份候选（无 await、不调用户代码）。
        // 采纳的是**意图**（文件是真相）；动作执行失败不回滚树——下一轮对账会带着同一份
        // 目标重试，把它慢慢推到位；回滚树反而会让"文件说的"和"树说的"长期分叉。
        if let Err(rejection) = self.adopt(candidate, &desired.disabled) {
            // 不采纳就必须**说出来**：静默丢弃候选等于让配置悄悄撒谎。
            let failure = rejection.message().to_string();
            self.ctx.log(Level::Error, &format!("对账：{failure}"));
            let outcome = ReconcileOutcome {
                failures: vec![failure],
                ..ReconcileOutcome::default()
            };
            *self.last.lock().unwrap() = Some(outcome.clone());
            self.publish_report();
            return ReconcileAttempt {
                outcome,
                rejection: Some(rejection),
            };
        }
        if let Some(flag) = adopted_flag {
            flag.store(true, Ordering::SeqCst);
        }
        // 采纳成功：这一刻候选才成为当前树，这一轮的预检结论才配进报告（O7）。
        if let Some((report, provides)) = accepted_preflight {
            self.record_preflight(report, provides);
        }
        let mut rows: HashMap<String, FlatRow> = desired
            .rows
            .into_iter()
            .map(|row| (row.label.clone(), row))
            .collect();
        let issue_items = assign_issue_keys(desired.issues);
        // 清理过期的失败台账：
        // 1) 已从配置移除或已被禁用的行；
        // 2) 本轮已与期望态完全收敛且无变更动作的已挂载行（例如上轮 Replace/Update 失败后配置回退）。
        {
            let active_issue_keys: HashSet<&str> =
                issue_items.iter().map(|(key, _)| key.as_str()).collect();
            let action_labels: HashSet<&str> = actions
                .iter()
                .map(|action| match action {
                    Action::Dispose(label)
                    | Action::Mount(label)
                    | Action::Replace(label)
                    | Action::Update { label, .. }
                    | Action::Volatile { label, .. } => label.as_str(),
                })
                .collect();
            let mounted_labels: HashSet<String> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, slot)| matches!(slot, RowSlot::Mounted(_)))
                .map(|(label, _)| label.clone())
                .collect();
            self.failed_rows.lock().unwrap().retain(|label, _| {
                if active_issue_keys.contains(label.as_str()) {
                    return true;
                }
                if !rows.contains_key(label) {
                    return false;
                }
                !(mounted_labels.contains(label) && !action_labels.contains(label.as_str()))
            });
        }

        let mut outcome = ReconcileOutcome::default();
        // 决策失败（`!!js` 求值失败 / 重复 label）：
        // - `Interpolation` 属运行不可用，按行级启动策略处置并保留 `provides` 供下游 Pending 关联；
        // - `DuplicateLabel` 属配置结构冲突，使用独立台账键（不被首行 Mount 误清），且绝不因首行 `optional: true` 被吞掉。
        for (issue_key, issue) in issue_items {
            match issue.kind {
                DesiredIssueKind::Interpolation => {
                    let provides =
                        self.declared_provides_for(&issue.label, &issue.entry, None, None);
                    self.handle_row_failure(
                        &issue.label,
                        &issue.entry,
                        issue.isolate,
                        FailureCategory::RuntimeUnavailable,
                        issue.issue,
                        provides,
                        &mut outcome,
                        "对账：",
                        None,
                    );
                }
                DesiredIssueKind::DuplicateLabel => {
                    let policy = startup_policy_for(&self.ctx);
                    let degraded = match policy.as_ref() {
                        Some(policy) => policy.persisted_state() == FailureDisposition::Degrade,
                        None => !self.strict,
                    };
                    let level = if degraded { Level::Warn } else { Level::Error };
                    self.ctx
                        .log(level, &format!("{}: 对账：{}", issue.label, issue.issue));
                    self.record_failed_row(
                        FailedRow {
                            label: issue_key,
                            plugin: issue.entry.name.clone(),
                            category: FailureCategory::PersistedState,
                            startup: RowStartup::Required,
                            degraded,
                            provides: Vec::new(),
                            message: issue.issue.clone(),
                        },
                        None,
                    );
                    if !degraded || (policy.is_none() && !self.strict) {
                        outcome
                            .failures
                            .push(format!("{}: {}", issue.label, issue.issue));
                    }
                }
            }
        }
        for action in actions {
            match action {
                Action::Dispose(label) => {
                    self.exec_dispose_action(&rows, label, &mut outcome).await;
                }
                Action::Mount(label) => {
                    if let Some(row) = rows.remove(&label) {
                        self.exec_mount_action(row, &mut outcome).await;
                    }
                }
                Action::Replace(label) => {
                    if let Some(row) = rows.remove(&label) {
                        self.exec_replace_action(row, &mut outcome).await;
                    }
                }
                Action::Update { label, config } => {
                    if let Some(row) = rows.remove(&label) {
                        self.exec_update_action(row, config, &mut outcome).await;
                    }
                }
                Action::Volatile { label, config } => {
                    if let Some(row) = rows.remove(&label) {
                        self.exec_volatile_action(row, config, &mut outcome).await;
                    }
                }
            }
        }
        *self.last.lock().unwrap() = Some(outcome.clone());
        // M4-6：报告是**实时**的——每轮对账后刷新（`reconcile.*`/`failed`/`disabled` 才有意义）。
        self.publish_report();
        ReconcileAttempt {
            outcome,
            rejection: None,
        }
    }

    async fn exec_dispose_action(
        self: &Arc<Self>,
        rows: &HashMap<String, FlatRow>,
        label: String,
        outcome: &mut ReconcileOutcome,
    ) {
        // 移除路径只有 emit（上游 `EntryGroup.remove` 不走 `patch-context`）。
        self.emit_partial_dispose(rows.get(&label), &label, false)
            .await;
        let close_report = self.dispose_row(&label).await;
        self.clear_failed_row(&label);
        if let Some(report) = close_report {
            if report.still_running {
                let msg = format!(
                    "卸载未在预算内完成（状态={}），后台清理仍在进行（清理完成前禁止重挂）",
                    report.status.as_str()
                );
                self.ctx.log(Level::Error, &format!("{label}: {msg}"));
                outcome.failures.push(format!("{label}: {msg}"));
            } else if !report.errors.is_empty() {
                self.ctx.log(
                    Level::Warn,
                    &format!("{label}: 已卸载（清理产生 {} 条错误）", report.errors.len()),
                );
            } else {
                self.ctx
                    .log(Level::Info, &format!("{label}: 已卸载（不再期望挂载）"));
            }
        } else {
            self.ctx
                .log(Level::Info, &format!("{label}: 已卸载（不再期望挂载）"));
        }
        outcome.disposed.push(label);
    }

    async fn exec_mount_action(self: &Arc<Self>, row: FlatRow, outcome: &mut ReconcileOutcome) {
        let label = row.label.clone();
        // 该行正处于插件自我卸载异步收尾期，绝不允许旁路重挂复活。
        if self.is_row_self_disposing(&label) {
            outcome.skipped.push(label);
            return;
        }
        // 旧实例仍在后台排空/清理时绝不允许旁路重挂，防止新旧实例在同一行上并发冲突。
        if self.is_row_draining(&label) {
            let provides = self.declared_provides_for(&label, &row.entry, None, None);
            self.handle_row_failure(
                &label,
                &row.entry,
                Some(row.scope.isolate.clone()),
                FailureCategory::RuntimeUnavailable,
                "旧实例后台清理尚未完成，拒绝重新挂载新实例".to_string(),
                provides,
                outcome,
                "挂载被阻断：",
                None,
            );
            return;
        }
        match self.patch_context(&row, PatchWork::Mount, None).await {
            Some(ActionOutcome::Done) => {
                self.clear_failed_row(&label);
                self.ctx.log(Level::Info, &format!("{label}: 已挂载"));
                outcome.mounted.push(label);
            }
            Some(ActionOutcome::Vetoed) | None => outcome.skipped.push(label),
            Some(ActionOutcome::Failed { failure, provides }) => {
                self.handle_row_failure(
                    &label,
                    &row.entry,
                    Some(row.scope.isolate.clone()),
                    failure.category,
                    failure.message,
                    provides,
                    outcome,
                    "挂载失败 ",
                    None,
                );
            }
        }
    }

    async fn exec_replace_action(self: &Arc<Self>, row: FlatRow, outcome: &mut ReconcileOutcome) {
        let label = row.label.clone();
        let (expected_fiber, previous_plugin) = self
            .mounted_record_clone(&label)
            .map(|record| (record.fiber.upgrade(), Some(record.plugin)))
            .unwrap_or((None, None));
        // O5b-2：**先决策、再前移**——artifact 的预检（resolve → stage → open → ping）
        // 必须在拆卸之前跑完。写错 `name` / artifact 缺失时旧实例仍在服务：
        // 这一行本轮到此为止（不拆卸、不回滚、更不重建旧实例）。
        let admission = self.source.admission();
        let preflight =
            self.source
                .load_classified(&row.entry, &self.search, self.next_revision(), &admission);
        let loaded = match preflight {
            Ok(loaded) => loaded,
            Err(err) => {
                let issue = format!("换实例预检失败（保留当前实例）：{}", err.message);
                let provides =
                    self.declared_provides_for(&label, &row.entry, None, previous_plugin.as_ref());
                self.handle_row_failure(
                    &label,
                    &row.entry,
                    Some(row.scope.isolate.clone()),
                    err.category,
                    issue,
                    provides,
                    outcome,
                    "",
                    None,
                );
                return;
            }
        };
        // 在单把 `rows` 锁内原子核验并认领旧实例进入 `Disposing` 态：
        // 若预检期间插件发生自我卸载（`SelfDisposing` 或旧实例已消失）或处于 `Draining`，
        // 立即丢弃预加载的 `loaded` 并终止替换，杜绝并发双实例或复活已自卸载的行。
        let tx_guard = match self.claim_mounted_for_dispose(&label, expected_fiber.as_ref()) {
            DisposeClaim::Claimed(guard) => Some(guard),
            DisposeClaim::SelfDisposing => {
                drop(loaded);
                outcome.skipped.push(label);
                return;
            }
            DisposeClaim::Draining => {
                drop(loaded);
                let provides =
                    self.declared_provides_for(&label, &row.entry, None, previous_plugin.as_ref());
                self.handle_row_failure(
                    &label,
                    &row.entry,
                    Some(row.scope.isolate.clone()),
                    FailureCategory::RuntimeUnavailable,
                    "旧实例后台清理尚未完成，拒绝换实例".to_string(),
                    provides,
                    outcome,
                    "",
                    None,
                );
                return;
            }
            DisposeClaim::NotMounted => None,
        };
        // 先卸后装：同一隔离域里一个服务名只能有一个实现（内核不变量）。
        self.emit_partial_dispose(Some(&row), &label, true).await;
        let close_report = match tx_guard {
            Some(guard) => self.finish_claimed_dispose(guard).await,
            None => None,
        };
        if let Some(report) = &close_report {
            if report.still_running {
                // 丢弃预加载的 artifact（立即回收暂存副本），绝不在旧实例尚未清完时挂入新实例。
                drop(loaded);
                let provides =
                    self.declared_provides_for(&label, &row.entry, None, previous_plugin.as_ref());
                let issue = format!(
                    "换实例失败（旧实例卸载未在预算内完成，状态={}，后台清理仍在进行，拒绝挂载新实例）",
                    report.status.as_str()
                );
                self.handle_row_failure(
                    &label,
                    &row.entry,
                    Some(row.scope.isolate.clone()),
                    FailureCategory::RuntimeUnavailable,
                    issue,
                    provides,
                    outcome,
                    "",
                    None,
                );
                return;
            }
        }
        // 预检拿到的 artifact 交给尾链**复用**：绝不二次 `load`（那会再跑一次
        // stage/open/ping，把前移的收益抵消掉）。
        match self
            .patch_context(&row, PatchWork::Mount, Some(loaded))
            .await
        {
            Some(ActionOutcome::Done) => {
                self.clear_failed_row(&label);
                self.ctx.log(Level::Info, &format!("{label}: 已换实例"));
                outcome.replaced.push(label);
            }
            Some(ActionOutcome::Vetoed) | None => {
                let issue =
                    "换实例被 loader/patch-context 短路（旧实例已卸下、新实例未挂）".to_string();
                let provides =
                    self.declared_provides_for(&label, &row.entry, None, previous_plugin.as_ref());
                self.handle_row_failure(
                    &label,
                    &row.entry,
                    Some(row.scope.isolate.clone()),
                    FailureCategory::RuntimeUnavailable,
                    issue,
                    provides,
                    outcome,
                    "",
                    Some(Level::Warn),
                );
            }
            Some(ActionOutcome::Failed {
                failure,
                mut provides,
            }) => {
                if let Some(prev) = previous_plugin.as_ref() {
                    provides.extend(prev.provision());
                    provides.sort();
                    provides.dedup();
                }
                let message = format!("换实例失败（旧实例已卸下、未恢复）：{}", failure.message);
                self.handle_row_failure(
                    &label,
                    &row.entry,
                    Some(row.scope.isolate.clone()),
                    failure.category,
                    message,
                    provides,
                    outcome,
                    "",
                    None,
                );
            }
        }
    }

    async fn exec_update_action(
        self: &Arc<Self>,
        row: FlatRow,
        config: Config,
        outcome: &mut ReconcileOutcome,
    ) {
        let label = row.label.clone();
        // 上游 `entry.ts:130`：更新路径同样先发 partial-dispose（`by_update=true`），
        // 再走 `patch-context` 的尾链去执行 `fiber.update`。
        self.emit_partial_dispose(Some(&row), &label, true).await;
        match self
            .patch_context(&row, PatchWork::Update(config), None)
            .await
        {
            Some(ActionOutcome::Done) => {
                self.clear_failed_row(&label);
                outcome.updated.push(label);
            }
            // 否决：内存记录照旧推进，实例上没落地 ⇒ 记进 lagging（M4-6 报告）。
            Some(ActionOutcome::Vetoed) => outcome.lagging.push(label),
            None => outcome.skipped.push(label),
            Some(ActionOutcome::Failed { failure, provides }) => {
                self.handle_row_failure(
                    &label,
                    &row.entry,
                    Some(row.scope.isolate.clone()),
                    failure.category,
                    failure.message,
                    provides,
                    outcome,
                    "配置更新失败 ",
                    None,
                );
            }
        }
    }

    async fn exec_volatile_action(
        self: &Arc<Self>,
        row: FlatRow,
        config: Config,
        outcome: &mut ReconcileOutcome,
    ) {
        let label = row.label.clone();
        // M4-5：这条路径**不发** `loader/partial-dispose`、**不走** `patch-context`
        // （上游 `entry.ts:150-153` 把它整个跳过）——实例不重挂，就没什么"半拆"可言。
        match self.commit_volatile(&label, &config).await {
            Ok(Some(changed)) => {
                self.clear_failed_row(&label);
                self.ctx.log(
                    Level::Info,
                    &format!("{label}: volatile 值已原地提交（{}）", changed.join(", ")),
                );
                outcome.volatile.push(label);
            }
            // 运行期前置条件不成立（实例已不在 ACTIVE / 候选期间重载过 /
            // 普通值也变了）：交回普通更新（上游"`_commitVolatile` 返回 false"）。
            Ok(None) => {
                self.ctx.log(
                    Level::Info,
                    &format!("{label}: volatile 前置条件不成立，改走普通更新"),
                );
                self.exec_update_action(row, config, outcome).await;
            }
            // 候选无效 / 推送失败：**保留运行值**，实例落后于配置（上游 warn + 不重挂）。
            // raw 已经推进（下次激活用它），所以这里不是失败、是 `lagging`。
            Err(issue) => {
                self.ctx.log(
                    Level::Warn,
                    &format!("{label}: volatile 提交未生效，保留运行值：{issue}"),
                );
                outcome.lagging.push(label);
            }
        }
    }

    /// 请求一次对账（watcher 每轮只调它）：**合并 + 重入保护**。
    ///
    /// 已经有一轮在飞时直接返回 `None`——那一轮结束前会顺手把**最新**的那份候选带走，
    /// 于是"排队期间来的旧树"根本不会被应用（上游 `_pendingTree` 的同一语义）。
    pub async fn request_reconcile(
        self: &Arc<Self>,
        entries: Vec<Entry>,
    ) -> Option<ReconcileOutcome> {
        let gen = self.next_candidate_gen();
        let base_writeback = self.writeback();
        self.request_candidate(Candidate::queued(entries, gen, base_writeback))
            .await
    }

    /// 候选代次取号：**请求登记时**取（`reload` 在解析之前就取），所以"谁是最新意图"
    /// 由请求先后决定。晚到的旧解析因此拿不到更大的代次，也就顶不掉新请求。
    fn next_candidate_gen(&self) -> u64 {
        self.next_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 当前回写版本：候选构造时快照它，采纳前再比一次。
    fn writeback(&self) -> u64 {
        self.writeback.load(Ordering::SeqCst)
    }

    /// 推进回写版本：已采纳的目标（树 / 主树 / 基线 / 磁盘）被运行时回写改过了。
    ///
    /// 推进点只有三处：`persist` 成功写盘、提交观察者改 `tree`/`main_only`
    /// （含 `no_save` 纯内存分支）、插件自我卸载改 `tree`/`main_only`。
    /// 后两处必须与它所改的 `tree` 落在**同一个** `config_gate` 临界区里（见 [`Self::adopt`]）。
    fn bump_writeback(&self) {
        self.writeback.fetch_add(1, Ordering::SeqCst);
    }

    /// 登记候选：在**同一个临界区**里比较代次并写入合并槽。
    ///
    /// - 代次 ≤ 已采纳代次 ⇒ 迟到的旧解析，丢弃（不得复活）；
    /// - 合并槽里已有更新的候选 ⇒ 保留新的（旧的不覆盖新请求）；
    /// - 否则替换合并槽（"只有最新的一份会被应用"）。
    ///
    /// 返回 `false` 表示候选已过期：调用方不得再驱动它。
    fn register_candidate(&self, candidate: Candidate) -> bool {
        let gen = candidate.gen;
        let stale = {
            let mut pending = self.pending.lock().unwrap();
            let superseded = if gen == 0 {
                false
            } else {
                // 与 `adopt` 的已采纳代次比较：adopt 在 `config_gate` 下推进它，
                // 这里读到的值可能稍旧——最坏情况是候选被登记后又在对账里被判过期，
                // 那条路同样会报出可诊断的失败（不会静默采纳）。
                gen <= self.adopted_gen.load(Ordering::SeqCst)
            };
            let newer_queued = pending
                .as_ref()
                .is_some_and(|existing| existing.gen > candidate.gen);
            if superseded || newer_queued {
                true
            } else {
                *pending = Some(candidate);
                false
            }
        };
        if stale {
            self.ctx.log(
                Level::Warn,
                &format!(
                    "配置候选已过期（代次 {gen} ≤ 已采纳 {} 或合并槽里已有更新的请求）：丢弃这次解析结果",
                    self.adopted_gen.load(Ordering::SeqCst)
                ),
            );
        }
        !stale
    }

    /// 登记候选并（必要时）驱动它：合并槽只保留**最新**的一份。
    async fn request_candidate(self: &Arc<Self>, candidate: Candidate) -> Option<ReconcileOutcome> {
        if !self.register_candidate(candidate) {
            return None;
        }
        self.drive_pending().await
    }

    /// 把"已 take 但未落定"的候选还回合并槽（取消/超时路径）。
    ///
    /// 只有它的代次仍是最新时才回填：更新的请求已经在槽里，旧候选不该复活。
    /// 返回是否真的回填了（诊断/测试用）。
    fn restore_candidate(&self, candidate: Candidate) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if candidate.gen != 0 && candidate.gen <= self.adopted_gen.load(Ordering::SeqCst) {
            return false;
        }
        if pending
            .as_ref()
            .is_some_and(|existing| existing.gen > candidate.gen)
        {
            return false;
        }
        *pending = Some(candidate);
        true
    }

    /// 合并槽里是否有一份**不是** `held_gen` 那份的候选。
    ///
    /// [`ReconcileDriver::drop`] 清位之后用它复查"这一窗口里有没有别人登记的候选"：
    /// `held_gen == None`（本 driver 还没 take 到候选就被取消）时，槽里的任何一份都算
    /// "别人的"——它登记时单飞位被我们占着，它的驱动请求只会拿到 `None`。
    ///
    /// 只按代次辨认（合并槽里只会有排队候选，`gen >= 1` 且每次请求唯一），
    /// 于是"还是我们刚回填的那份"能被准确认出来，既有语义（不自动驱动）不受影响。
    fn has_foreign_pending(&self, held_gen: Option<u64>) -> bool {
        self.pending
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|queued| Some(queued.gen) != held_gen)
    }

    /// 安排一次**补偿驱动**：`Drop` 里发现槽里有一份没人处理的候选时用。
    ///
    /// 不能 await（`Drop` 是同步的），所以 spawn 一个**只做 [`Self::drive_pending`]** 的
    /// 任务：它照常 `reconciling.swap(true)` 抢单飞位，抢不到就安静退出——于是不会与
    /// 别的驱动竞争出两份采纳，也不绕过任何既有协议（回填/代次/回写版本照旧生效）。
    ///
    /// 边界：拿不到 runtime 句柄（非 runtime 上下文，例如 runtime 正在关闭时丢弃驱动）
    /// 就**不安排**——补偿不保证执行；调用方下一次 `request_reconcile` / `reload` 仍会驱动。
    fn schedule_compensation(self: &Arc<Self>) {
        if self.pending.lock().unwrap().is_none() {
            return;
        }
        // 整棵树正在关：没有"用户的下一次编辑"要救，别在关停途中挂新东西。
        if self.ctx.fiber_opt().is_none_or(|fiber| fiber.is_disposed()) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = self.clone();
        handle.spawn(async move {
            state.drive_pending().await;
        });
    }

    /// 安排一次**已采纳树的补偿收敛**：当 `drive_pending` 在 `adopt()` 已经提交新树之后、
    /// 动作循环跑完之前被取消时，等待本次取消产生的 `Draining` 清理归零后自动跑一次 `sync()`
    /// （若期间又来了更新候选，则改由 `drive_pending` 一并驱动）。
    fn schedule_adopted_convergence(self: &Arc<Self>) {
        if self.ctx.fiber_opt().is_none_or(|fiber| fiber.is_disposed()) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = self.clone();
        handle.spawn(async move {
            state.wait_active_draining().await;
            if state
                .ctx
                .fiber_opt()
                .is_none_or(|fiber| fiber.is_disposed())
            {
                return;
            }
            if state.pending.lock().unwrap().is_some() {
                state.drive_pending().await;
            } else {
                state.sync().await;
            }
        });
    }

    /// 驱动 `pending` 里的候选（单飞）：直到槽空为止，每次只应用"当下最新"的那份。
    async fn drive_pending(self: &Arc<Self>) -> Option<ReconcileOutcome> {
        if self.reconciling.swap(true, Ordering::SeqCst) {
            return None;
        }
        // 单飞位由这个 future 持有；被 timeout/abort 丢弃时必须复位。
        let mut driver = ReconcileDriver::new(self.clone());
        let mut last = None;
        // 同一代次的候选因**回写版本过期**而重建的次数：有限重试，绝不无限循环。
        // 换了代次（新的请求）就从 0 开始——新的意图有自己的预算。
        let mut stale_rebuilds: Option<(u64, u32)> = None;
        loop {
            // 先把要应用的候选取出来（guard 只活在这一句）：`match` 的临时量会活到整个
            // match 结束，那样 guard 就跨过了下面的 `.await`，整个 future 不再是 `Send`。
            let next = self.pending.lock().unwrap().take();
            match next {
                Some(candidate) => {
                    // 重建只对**文件派生**的候选成立：`adopt_metadata` 的候选带着
                    // 从磁盘读出来的回写元数据（`reload` 的产物），重新读一遍磁盘得到的
                    // 就是同一个意图的最新版本。`request_reconcile` 登记的候选可能来自
                    // 调用方自己算的条目树，从磁盘重建会替换掉调用方给的内容。
                    let rebuildable = candidate.adopt_metadata;
                    let gen = candidate.gen;
                    // 候选先进**驱动器**（不是循环变量）：future 被取消/超时丢弃时，
                    // `ReconcileDriver::drop` 才能把它回填，而不是跟着栈一起蒸发。
                    driver.adopted_in_progress.store(false, Ordering::SeqCst);
                    driver.taken = Some(candidate);
                    // 合并逻辑归 `request_candidate`，但**落地**必须与其它变更序列互斥：
                    // 否则运行期启停会与这一轮对账交错，把刚卸下的行挂回去。
                    let attempt = {
                        let _guard = self.mutation.lock().await;
                        let candidate = driver.taken.as_ref().expect("刚填进去的候选");
                        self.reconcile_attempt_with_flag(
                            candidate,
                            Some(&driver.adopted_in_progress),
                        )
                        .await
                    };
                    driver.adopted_in_progress.store(false, Ordering::SeqCst);
                    let ReconcileAttempt {
                        mut outcome,
                        rejection,
                    } = attempt;
                    // 回写版本过期（**只有**这一种原因）⇒ 重新读取/构造并预检，
                    // 把新候选交给**同一个**单飞位继续处理（不另起一份采纳）。
                    // 代次更旧仍然直接丢弃：更新的意图已经存在，重建只会顶掉它。
                    let stale = matches!(rejection, Some(AdoptRejection::StaleWriteback(_)));
                    if stale && rebuildable {
                        // 重建期间保留 `driver.taken`：若在 `rebuild_stale_candidate().await` 中途被取消，
                        // `ReconcileDriver::drop` 会把候选回填到合并槽（或触发补偿驱动），避免文件重载丢失。
                        if let StaleRebuild::Stopped(note) =
                            self.rebuild_stale_candidate(gen, &mut stale_rebuilds).await
                        {
                            self.ctx.log(Level::Error, &format!("对账：{note}"));
                            outcome.failures.push(note);
                            *self.last.lock().unwrap() = Some(outcome.clone());
                            self.publish_report();
                        }
                        driver.taken = None;
                        last = Some(outcome);
                        continue;
                    }
                    // 正常返回：候选已经落定（采纳，或明确拒绝并报出失败），不再回填。
                    driver.taken = None;
                    last = Some(outcome);
                }
                None => {
                    self.reconciling.store(false, Ordering::SeqCst);
                    // 关门前再确认一次：刚好在 take 与关门之间放进来的那份不能被落下，
                    // 否则它要等到下一次文件变动才会被应用（幽灵滞后）。
                    if self.pending.lock().unwrap().is_some()
                        && !self.reconciling.swap(true, Ordering::SeqCst)
                    {
                        continue;
                    }
                    break;
                }
            }
        }
        driver.finish();
        last
    }

    /// 过期候选的重建：**重新稳读 → 解析 → 预检 → 构造新候选（快照新的回写版本）→ 登记**。
    ///
    /// 为什么必须有这条路：`adopt` 因回写版本过期拒绝候选之后，如果只记日志 + 进
    /// `failures`，用户那次编辑就再也没人接手——watcher 的指纹早已对齐磁盘，
    /// 文件不变就不会再来一次 `reload`（规格 §O5：失效候选必须重新读取/构造并预检）。
    ///
    /// 分寸：
    /// - 读取/解析/预检**复用** [`Self::build_candidate`]（`reload` 的同一条路径），
    ///   不复制第二份解析实现；
    /// - 代次沿用原候选（这是**同一个请求**的重建，不是新的意图），于是登记照旧过
    ///   [`Self::register_candidate`] 的代次/合并槽判定、采纳照旧过 [`Self::adopt`] 的
    ///   代次与回写版本校验——既不绕过校验，也不会与刚登记的外部请求争抢出两份采纳；
    /// - 同一代次最多重建 [`MAX_STALE_REBUILDS`] 次：超过就停在这里，让那条过期失败
    ///   （日志 + `failures`）成为可诊断的终态，而不是无限循环。
    async fn rebuild_stale_candidate(
        self: &Arc<Self>,
        gen: u64,
        retries: &mut Option<(u64, u32)>,
    ) -> StaleRebuild {
        let used = match *retries {
            Some((attempted_gen, count)) if attempted_gen == gen => count,
            _ => 0,
        };
        if used >= MAX_STALE_REBUILDS {
            return StaleRebuild::Stopped(format!(
                "配置候选重建已达上限（{MAX_STALE_REBUILDS} 次）：运行时回写在重建期间仍在推进，停止重建"
            ));
        }
        *retries = Some((gen, used + 1));
        #[cfg(test)]
        self.stale_rebuilds.fetch_add(1, Ordering::SeqCst);
        let rebuilt = self.build_candidate(gen).await;
        let candidate = match rebuilt {
            Ok(Some(candidate)) => candidate,
            // 文件正在被写：这一轮放弃。下一次请求 / 补偿驱动仍会读到它。
            Ok(None) => {
                return StaleRebuild::Stopped(
                    "配置候选重建未执行：文件正在被写（稳读不通过），等下一轮再看".to_string(),
                )
            }
            Err(err) => {
                return StaleRebuild::Stopped(format!("配置候选重建失败: {}", err.into_message()))
            }
        };
        if !self.register_candidate(candidate) {
            // 合并槽里已经有更新的请求（它的代次更大）：重建的这份不覆盖它。
            // 那份更新请求会自己走一轮对账，过期时也会走自己的重建。
            return StaleRebuild::Queued;
        }
        self.ctx.log(
            Level::Warn,
            &format!(
                "配置候选因运行时回写过期而失效：已按磁盘重建第 {}/{MAX_STALE_REBUILDS} 次，继续采纳",
                used + 1
            ),
        );
        StaleRebuild::Queued
    }

    /// 采纳候选：在**已经持有 `mutation` 的同步段**里完成校验与提交。
    ///
    /// 锁序固定：`mutation`（调用方已持）→ `config_gate` → `main_only` / `owners` /
    /// `main_baseline` / `tree`。全程不含 `.await`、不调用来源或用户代码——于是
    /// "校验"与"写字段"之间没有任何窗口能让回写插进来。校验不过则**一个字段都不写**。
    ///
    /// 互斥的另一半在回写方：它们必须把"改 `tree` + 推进回写版本 + 写 `main_only` +
    /// 落盘"整段放进**同一个** `config_gate` 临界区（[`Self::mark_self_disposed`] 与
    /// 提交观察者）。否则下面"校验通过 → 整份覆盖树"仍会被一次并发的回写覆盖掉，
    /// 而 watcher 的指纹早已对齐磁盘、不会补偿（内存树回退，记录与磁盘却是新值）。
    fn adopt(
        &self,
        candidate: &Candidate,
        disabled: &[String],
    ) -> std::result::Result<(), AdoptRejection> {
        let _gate = self.config_gate.lock().unwrap();
        if candidate.gen != 0 {
            // 代次：旧候选不得覆盖新状态，也不得让迟到的旧解析复活。
            let adopted = self.adopted_gen.load(Ordering::SeqCst);
            if candidate.gen <= adopted {
                return Err(AdoptRejection::StaleGeneration(format!(
                    "配置候选已过期（代次 {} ≤ 已采纳 {}）：不采纳，等待更新的请求",
                    candidate.gen, adopted
                )));
            }
        }
        // 回写版本（对排队候选与本地候选一律生效）：运行时回写已经改过已采纳目标
        // ⇒ 本候选基于一棵过时的树，绝不用旧快照整份覆盖并发回写或自我卸载。
        let current = self.writeback.load(Ordering::SeqCst);
        if candidate.base_writeback != current {
            return Err(AdoptRejection::StaleWriteback(format!(
                "配置候选已过期（构造时回写版本 {} ≠ 当前 {}）：运行时回写已修改已采纳目标，不采纳旧候选",
                candidate.base_writeback, current
            )));
        }
        // 测试闸门：钉住"校验通过、还没写树"这个瞬间（此刻已持 `config_gate`）。
        // 回写方若不在闸门内改树，就能从这里插进来，让下面的整份覆盖把它退回去。
        #[cfg(test)]
        self.hooks.pass_inside_gate();
        if candidate.adopt_metadata {
            // 元数据先落地：它决定之后的回写落到哪一层、拿哪份文本做比较。
            self.with_main_only(|main| *main = candidate.main.clone());
            *self.main_startup.lock().unwrap() = candidate.main_startup.clone();
            *self.row_startup.lock().unwrap() = candidate.row_startup.clone();
            *self.owners.lock().unwrap() = candidate.owners.clone();
            *self.main_baseline.lock().unwrap() = candidate.baseline.clone();
            self.main_baseline_valid.store(true, Ordering::SeqCst);
            let main_label = self.files.main.display().to_string();
            let patch_labels: HashSet<String> = self
                .files
                .patches
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            self.persisted_errors
                .lock()
                .unwrap()
                .retain(|err| err.label != main_label && !patch_labels.contains(&err.label));
        }
        // 树随后：对账的目标态。
        *self.tree.lock().unwrap() = candidate.entries.clone();
        // 休眠行进报告（M4-6）：用户问的"为什么没生效"，答案通常就在这张表里。
        *self.disabled.lock().unwrap() = disabled.to_vec();
        if candidate.gen != 0 {
            self.adopted_gen.store(candidate.gen, Ordering::SeqCst);
        }
        Ok(())
    }

    /// `main_only` 的**唯一写入口**：锁序纪律的落点（调用方必须已持 `config_gate`）。
    fn with_main_only<T>(&self, f: impl FnOnce(&mut Vec<Entry>) -> T) -> T {
        // `try_lock` 失败 = 有人持有闸门。唯一合法的持有人就是当前这层调用栈
        // （锁序要求写入点已在 `config_gate` 里），所以这就是"是否守纪律"的判据。
        #[cfg(test)]
        if self.config_gate.try_lock().is_ok() {
            self.ungated_main_writes.fetch_add(1, Ordering::SeqCst);
        }
        let mut main = self.main_only.lock().unwrap();
        f(&mut main)
    }

    /// 一条龙：稳读文件 → 解析（含 patch 层）→ 预检 → 登记候选。
    ///
    /// 失败路径**一个字节都不动已采纳状态**：解析失败保留上一快照（编辑中的半截文件
    /// 是常态），稳读不通过就下一轮再看；预检失败时连候选都不登记，元数据也不推进。
    ///
    /// 可能返回 [`ReloadOutcome::Coalesced`] **而不应用这次编辑**：已经有一轮在飞
    /// （那一轮会带走最新的一份），或登记/采纳时发现这份候选因代次更旧、回写版本过期
    /// 被丢弃——文件派生的候选过期时会按磁盘重建（[`Self::rebuild_stale_candidate`]），
    /// 重建也给不出可驱动的候选时才以失败报出。
    pub async fn reload(self: &Arc<Self>) -> ReloadOutcome {
        // 代次按**请求顺序**取（解析之前）：解析更慢的旧请求不会在之后顶掉新请求。
        let gen = self.next_candidate_gen();
        let candidate = match self.build_candidate(gen).await {
            Ok(Some(candidate)) => candidate,
            Ok(None) => return ReloadOutcome::Unsettled,
            Err(BuildCandidateError::Persisted { label, message }) => {
                self.record_persisted_error(&label, "", message.clone());
                return ReloadOutcome::Failed(message);
            }
            Err(BuildCandidateError::Preflight(message)) => {
                return ReloadOutcome::Failed(message);
            }
        };
        match self.request_candidate(candidate).await {
            Some(outcome) => ReloadOutcome::Reconciled(outcome),
            None => ReloadOutcome::Coalesced,
        }
    }

    /// 从磁盘构造一份**完整的**候选（读 → 解析 → 预检 → 快照回写版本），不登记。
    ///
    /// `reload` 与"过期候选重建"（[`Self::rebuild_stale_candidate`]）共用这一条路径：
    /// 重建因此不可能绕过任何一道关（稳读、解析、预检、回写版本快照）。
    ///
    /// 返回 `Ok(None)` = 文件正在被写（稳读不通过，这一轮什么都不做）。
    async fn build_candidate(
        &self,
        gen: u64,
    ) -> std::result::Result<Option<Candidate>, BuildCandidateError> {
        // 回写版本必须在读盘之前快照：否则读盘之后、解析/预检期间发生的运行时回写会被
        // 误记进候选的新回写版本，使旧磁盘快照逃过 `adopt` 的 `StaleWriteback` 校验。
        let base_writeback = self.writeback();
        let snapshot = match self.read_settled().await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Ok(None),
            Err((path, err)) => {
                let label = path.display().to_string();
                return Err(BuildCandidateError::Persisted {
                    message: format!("读取 {label} 失败: {err}"),
                    label,
                });
            }
        };
        // 解析是**纯**的：结果只装进候选，不碰任何共享字段。
        let parsed = self.parse_snapshot(&snapshot)?;
        #[cfg(test)]
        self.hooks.pass_after_parse().await;
        // 预检是候选的第一道关：失败时连登记都不做，已采纳的元数据一个字节不动。
        // 结论**不进报告**：候选还没被采纳（可能被判过期拒绝），报告只能描述当前树。
        if let Err(err) = self.preflight_dependencies_with(&parsed.entries, &parsed.row_startup) {
            return Err(BuildCandidateError::Preflight(format!(
                "依赖预检失败: {err}"
            )));
        }
        Ok(Some(Candidate {
            gen,
            base_writeback,
            entries: parsed.entries,
            adopt_metadata: true,
            main: parsed.main,
            main_startup: parsed.main_startup,
            row_startup: parsed.row_startup,
            owners: parsed.owners,
            baseline: parsed.baseline,
        }))
    }

    /// 期望态：以 `entries` 为目标、用**这次的**求值器答案算出来的"应当挂着的行"。
    fn desired(&self, entries: &[Entry]) -> Desired {
        let mut scopes = self.scopes.lock().unwrap();
        let decision = RowDecision {
            evaluator: self.evaluator.as_ref(),
        };
        desired_rows(entries, &decision, &self.ctx, &mut scopes)
    }

    /// 现状 vs 期望态 ⇒ 动作表。
    fn diff(&self, desired: &[FlatRow]) -> Vec<Action> {
        let rows = self.rows.lock().unwrap();
        let wanted: HashSet<&str> = desired.iter().map(|row| row.label.as_str()).collect();
        let mut removed: Vec<String> = rows
            .iter()
            .filter(|(label, slot)| {
                matches!(slot, RowSlot::Mounted(_)) && !wanted.contains(label.as_str())
            })
            .map(|(label, _)| label.clone())
            .collect();
        removed.sort();
        let mut actions: Vec<Action> = removed.into_iter().map(Action::Dispose).collect();
        for row in desired {
            let record = match rows.get(&row.label) {
                Some(RowSlot::Mounted(record)) => record,
                Some(RowSlot::SelfDisposing { .. }) => continue,
                _ => {
                    actions.push(Action::Mount(row.label.clone()));
                    continue;
                }
            };
            let config = row.entry.config.clone().unwrap_or(Config::Null);
            if record.scope_inputs != row.scope_inputs
                || record.declared.name != row.entry.name
                || record.declared.inject != row.entry.inject
                || record.declared.isolate != row.entry.isolate
                || record.declared.intercept != row.entry.intercept
            {
                // 身份或上下文变了：本仓走 replace（正确性超集；上游不重挂，附录 A）。
                actions.push(Action::Replace(row.label.clone()));
            } else if record.config != config {
                // M4-5：**只差 volatile 路径** ⇒ 原地提交，不重挂。
                //
                // 这里的比较用**原始**配置（对应上游 `equalExceptVolatile(legacy.config,
                // this.options.config, schema)`）：声明是唯一依据——没声明的插件永远走
                // 普通更新。`!!js` 与默认值带来的差别留到执行期的"候选比较"再判。
                let volatile_only = record.volatile.as_ref().is_some_and(|paths| {
                    let segments: Vec<Vec<String>> =
                        paths.iter().map(|path| split_path(path)).collect();
                    equal_except_volatile(&record.config, &config, &segments)
                });
                if volatile_only {
                    actions.push(Action::Volatile {
                        label: row.label.clone(),
                        config,
                    });
                } else {
                    actions.push(Action::Update {
                        label: row.label.clone(),
                        config,
                    });
                }
            }
        }
        actions
    }

    /// 发 `loader/partial-dispose`（`emit`，不阻塞）：监听者用来做可视化/清缓存。
    async fn emit_partial_dispose(&self, row: Option<&FlatRow>, label: &str, by_update: bool) {
        let args = vec![Dyn::new(ExternValue(json!({
            "label": label,
            "by_update": by_update,
            "entry": row.map(|row| row.entry.to_value()),
        })))];
        match row {
            Some(row) => row.scope.emit("loader/partial-dispose", args).await,
            None => self.ctx.emit("loader/partial-dispose", args).await,
        }
    }

    /// 在**行的上下文**上跑 `loader/patch-context` 瀑布，尾链就是要执行的动作。
    ///
    /// 返回 `None` 表示**尾链没被跑到**：监听者短路了整个动作。
    /// `Mount`/`Update` 把它当作"什么都没发生"（记账为 `skipped`）；**`Replace` 是例外**——
    /// 它在调用这里之前已经把旧实例卸下，所以短路意味着"旧实例已卸下、新实例未挂"，
    /// 调用方把它记进 [`ReconcileOutcome::failures`]（O5b）。
    ///
    /// 这正是 waterfall "tail=动作" 的意思（上游 `entry.ts:102-109` 把
    /// `fiber.update`/`init` 放在 `waterfall('loader/patch-context', …)` 的尾链里）。
    ///
    /// `preloaded` 是**调用方已经替这一行 `load` 过**的 artifact（O5b-2：换实例的预检前移，
    /// 只有 `PatchWork::Mount` 用得上）。尾链复用它、绝不二次 `load`；尾链没跑到时
    /// 由本函数就地丢弃（暂存副本随之回收，见 [`crate::LoadedPlugin`] 的字段声明序）。
    ///
    /// 代价是权限不对称：对 `Replace`，监听者短路时 artifact 已经 load 过（`stage` +
    /// `open` + `ping`），短路只能拦住"挂载"而拦不住"加载"；新建行的 `Mount` 仍然
    /// 可以在 `load` 之前被否决。要拦加载本身请用来源准入（`PluginSource::admission`）。
    async fn patch_context(
        self: &Arc<Self>,
        row: &FlatRow,
        work: PatchWork,
        preloaded: Option<LoadedFile>,
    ) -> Option<ActionOutcome> {
        let action = match &work {
            PatchWork::Mount => "mount",
            PatchWork::Update(_) => "update",
        };
        let slot: Arc<Mutex<Option<ActionOutcome>>> = Arc::new(Mutex::new(None));
        let tail_slot = slot.clone();
        // 前移的 artifact 单独放进槽里（而不是只留在尾链的捕获环境里）：短路时尾链
        // 一行都不跑，甚至可能被监听者留着 `next` 换个时机跑——回收责任必须归本函数。
        let preloaded = Arc::new(Mutex::new(preloaded));
        let tail_preloaded = preloaded.clone();
        let state = self.clone();
        let owned = row.clone();
        let tail: BoxFuture<Ret> = Box::pin(async move {
            let outcome = match work {
                PatchWork::Mount => {
                    let loaded = tail_preloaded.lock().unwrap().take();
                    let mounted = match loaded {
                        Some(loaded) => state.mount_loaded(&owned, loaded).await,
                        None => state.mount_row(&owned).await,
                    };
                    match mounted {
                        Ok(_record) => ActionOutcome::Done,
                        Err((failure, provides)) => ActionOutcome::Failed { failure, provides },
                    }
                }
                PatchWork::Update(config) => match state.fiber_of(&owned.label) {
                    Some(fiber) if fiber.pending_background_cleanups() > 0 => {
                        let prev = state
                            .mounted_record_clone(&owned.label)
                            .map(|record| record.plugin);
                        let provides = state.declared_provides_for(
                            &owned.label,
                            &owned.entry,
                            prev.as_ref(),
                            None,
                        );
                        ActionOutcome::Failed {
                            failure: LoadFailure::runtime(
                                "旧代次后台清理尚未完成，拒绝并发更新重启",
                            ),
                            provides,
                        }
                    }
                    Some(fiber) => match fiber.update_with(config, true).await {
                        Ok(UpdateOutcome::Restarted) => {
                            // 记录跟着走：`no_save=true` 的提交已经由观察者推进过内存记录，
                            // 这里再对齐一次，免得 diff 下一轮又看到"配置变了"。
                            state.with_mounted_mut(&owned.label, |record| {
                                record.config = owned.entry.config.clone().unwrap_or(Config::Null);
                            });
                            ActionOutcome::Done
                        }
                        Ok(UpdateOutcome::Vetoed | UpdateOutcome::Superseded) => {
                            ActionOutcome::Vetoed
                        }
                        Err(err) => {
                            let prev = state
                                .mounted_record_clone(&owned.label)
                                .map(|record| record.plugin);
                            let provides = state.declared_provides_for(
                                &owned.label,
                                &owned.entry,
                                prev.as_ref(),
                                None,
                            );
                            ActionOutcome::Failed {
                                failure: LoadFailure::runtime(err.to_string()),
                                provides,
                            }
                        }
                    },
                    None => ActionOutcome::Failed {
                        failure: LoadFailure::runtime("装载表里没有可用实例"),
                        provides: state.declared_provides_for(
                            &owned.label,
                            &owned.entry,
                            None,
                            None,
                        ),
                    },
                },
            };
            *tail_slot.lock().unwrap() = Some(outcome);
            Some(Dyn::new(ExternValue(
                json!({ "label": owned.label, "action": action }),
            )))
        });
        let args = vec![Dyn::new(ExternValue(json!({
            "label": row.label,
            "action": action,
            "entry": row.entry.to_value(),
        })))];
        row.scope
            .waterfall("loader/patch-context", args, Some(tail), None)
            .await;
        // 短路（或监听者收下 `next` 却没跑）时，前移的那份 artifact 还在这里：
        // 就地丢弃——`LoadedFile` 一 drop，暂存副本随 `LoadedPlugin` 的字段声明序
        // 先卸载库、后删文件，不会留下无人认领的副本。跑到过尾链时这里是 `None`。
        let discarded = preloaded.lock().unwrap().take();
        drop(discarded);
        let outcome = slot.lock().unwrap().take();
        outcome
    }

    /// 一次 **volatile 原地提交**（M4-5）。三态：
    ///
    /// - `Ok(Some(changed))`：提交完成（`changed` = 值真的变了的路径）；
    /// - `Ok(None)`：**前置条件不成立** ⇒ 调用方走普通更新；
    /// - `Err(issue)`：**候选无效**（解析失败）⇒ 保留运行值，实例落后于配置。
    ///
    /// 顺序逐条对齐上游 `entry.ts:149-153` + `_commitVolatile`（`:164-197`）：
    /// ① raw 先推进（"下次激活用新的"，即使候选无效也推进）；② 候选 = 在行的 ctx 上
    /// 重新解析（`!!js` 会跟着当前上下文再算一次）；③ 实例必须还活着（ACTIVE）且
    /// **解析期间没有重载过**（单飞 + 陈旧判定：候选算完才发现实例换了代，
    /// 那就不是"原地提交"而是"给旧实例写值"）；④ 普通值必须没变；⑤ 推给 ABI 侧
    /// （原生插件是空操作）⇒ ⑥ 写引用 ⇒ ⑦ 发 `loader/volatile-update`（owner-filter）。
    async fn commit_volatile(
        self: &Arc<Self>,
        label: &str,
        config: &Config,
    ) -> std::result::Result<Option<Vec<String>>, String> {
        let Some(fiber) = self.fiber_of(label) else {
            return Ok(None);
        };
        // 声明与插件对象一起取（一次锁）：两者都是这次提交要用的。
        let Some((paths, plugin)) = self
            .mounted_record_clone(label)
            .and_then(|record| record.volatile.map(|paths| (paths, record.plugin)))
        else {
            return Ok(None);
        };
        if fiber.state() != cordis_core::FiberState::Active {
            return Ok(None);
        }
        // ① raw 推进 + 记录推进：文件是真相，下一次激活拿新的。
        fiber.set_raw_config(config.clone());
        self.with_mounted_mut(label, |record| {
            record.config = config.clone();
        });

        // ② 候选（会走 `internal/config` 链 + 插件的校验器）。
        let epoch_before = fiber.running_epoch();
        let candidate = fiber
            .resolve_raw(config)
            .await
            .map_err(|err| format!("候选解析失败: {err}"))?;

        // ③ 陈旧判定：解析期间实例换过代（依赖变了 / 被更新了）⇒ 这次候选不适用于"原地提交"。
        if fiber.state() != cordis_core::FiberState::Active || fiber.running_epoch() != epoch_before
        {
            return Ok(None);
        }

        // ④ 普通值必须没变（对应 `deepEqual(fiber.config, candidate, true)` 里
        //    "volatile 引用恒相等"那条规则：我们按声明路径把两边都抹平再比）。
        let segments: Vec<Vec<String>> = paths.iter().map(|path| split_path(path)).collect();
        if !equal_except_volatile(&fiber.resolved(), &candidate, &segments) {
            return Ok(None);
        }

        // ⑤ 先推给插件（ABI 侧的真值在插件里：推不动就**什么也不提交**）。
        let values = values_at(&candidate, &paths);
        plugin
            .apply_volatile(&fiber.ctx(), &values)
            .map_err(|err| format!("推送失败: {err}"))?;

        // ⑥ 写引用（身份不变、值变）。⑦ 有变化才发事件。
        let changed = fiber.commit_volatile(candidate);
        if !changed.is_empty() {
            self.emit_volatile_update(&fiber, &changed).await;
        }
        Ok(Some(changed))
    }

    /// 发 `loader/volatile-update`（M4-5）：载荷 = **变化的路径**，
    /// 且只到这一行的 fiber 子树（上游 `entry.ts:188-191` 的动态 filter）。
    async fn emit_volatile_update(self: &Arc<Self>, fiber: &Arc<Fiber>, changed: &[String]) {
        let args = vec![Dyn::new(ExternValue(json!(changed)))];
        let filter = self.ctx.store.fiber_filter(fiber);
        self.ctx
            .store
            .dispatch(
                Mode::Emit,
                "loader/volatile-update",
                &fiber.ctx(),
                args,
                None,
                filter.as_ref(),
            )
            .await;
    }

    /// 有意卸载一行：**单把 `rows` 锁原子跃迁至 `Disposing`**，再按 App 生命周期预算执行
    /// `dispose_with_report`。若卸载超时或 future 在 `.await` 期间被取消，均自动落入 `Draining`
    /// 直到后台清理归零，阻止后续 `Action::Mount` / `Action::Replace` 旁路重挂。
    async fn dispose_row(self: &Arc<Self>, label: &str) -> Option<CloseReport> {
        let guard = self.claim_mounted_for_dispose(label, None).into_claimed()?;
        self.finish_claimed_dispose(guard).await
    }

    async fn finish_claimed_dispose(
        self: &Arc<Self>,
        tx_guard: RowTransactionGuard,
    ) -> Option<CloseReport> {
        let Some(fiber) = tx_guard.active_fiber() else {
            tx_guard.finish_empty();
            return None;
        };
        let report = fiber.dispose_with_report(None).await;
        if report.still_running {
            tx_guard.finish_draining(fiber);
        } else {
            tx_guard.finish_empty();
        }
        Some(report)
    }

    /// 在阻塞线程池（或无运行时时的同步路径）里完成自我卸载的树更新与磁盘落盘。
    ///
    /// # 落盘必须落到行**真正所在的那一层**（P1）
    /// `self.tree` 是主配置 + patch 的合并结果。把它写回主配置会把 patch 插入的行
    /// 固化进主配置（下次读取时 patch 再插一遍 ⇒ 重复），还会把 patch 的改动
    /// 写进错误的文件。所以：主配置里的行走 [`persist`]；patch 插入的行走
    /// [`persist_overlay`]（追加一条 overlay 到最后一个 patch 文件）。
    fn persist_self_disposed(&self, label: &str, record: Option<Mounted>, ticket: u64) {
        // 闸门**先**取：从"改树"到"落盘"整段必须在同一个临界区里。与 [`adopt`] 的
        // "校验 + 整份写树"互斥——否则采纳可以在两者之间插进来，把这次回写盖回去
        // （树退回旧内容，而记录与磁盘是新值；watcher 指纹已对齐，不会补偿）。
        //
        // 闸门里**不跑任何用户代码**：`ctx.log` 是宿主/应用注入的 sink（可能 panic），
        // 在这里调它会让 std `Mutex` 中毒、把整个装载器闸门一起带走。所以结果先攒着，
        // 出了这个块再打日志。
        struct SelfDisposingSlotGuard<'a> {
            state: &'a LoaderState,
            label: &'a str,
            ticket: u64,
        }
        impl Drop for SelfDisposingSlotGuard<'_> {
            fn drop(&mut self) {
                self.state.clear_slot_if_ticket(self.label, self.ticket);
            }
        }
        let plugin_name = record
            .as_ref()
            .map(|record| record.declared.name.clone())
            .unwrap_or_default();
        let result = {
            let _gate = self.config_gate.lock().unwrap();
            // 声明在 `_gate` 之后：按逆序 drop，确保释放 `RowSlot::SelfDisposing` 必然发生在
            // `tree` 已标 `disabled = true` 且 `bump_writeback()` 生效之后、且仍在 `config_gate` 内。
            let _self_disposing = SelfDisposingSlotGuard {
                state: self,
                label,
                ticket,
            };
            // 合并树（内存）先标上：下一轮对账不会把它再挂回来。
            let marked = {
                let mut tree = self.tree.lock().unwrap();
                match find_by_label(&mut tree, label) {
                    Some(entry) => {
                        entry.disabled = true;
                        entry.disabled_expr = None;
                        entry.clone()
                    }
                    None => return,
                }
            };
            // 已采纳的树被改了：在飞/排队的旧候选必须以旧树为基准失效（否则会把这一行复活）。
            self.bump_writeback();
            let origin = record
                .map(|record| record.origin)
                .unwrap_or(RowOrigin::Main);
            // 字段级归属优先：主配置行也可能被 patch 的 `disabled` 覆盖。
            let owner_layer = marked
                .id
                .as_deref()
                .and_then(|id| self.owners.lock().unwrap().disabled_layer(id));
            // 所有回写路径（含 overlay）均在同一个 `config_gate` 临界区内落盘，
            // 防止并发 `adopt` 或过期候选重建读到尚未落盘的旧 overlay 并覆盖内存树。
            match owner_layer {
                Some(layer) => self.persist_overlay(layer, label, &marked, None, Some(true)),
                None => match origin {
                    RowOrigin::Main => {
                        // `main_only` 的读改写与落盘落在上面取的同一个 `config_gate` 里
                        // （`persist` 要求调用方已持闸门，这里**不再**重复取锁）。
                        let found = self.with_main_only(|main| match find_by_label(main, label) {
                            Some(entry) => {
                                entry.disabled = true;
                                entry.disabled_expr = None;
                                true
                            }
                            // 记录说它是主配置行、主树里却找不到（label 漂移等）：
                            // 退到 patch 层，**绝不**把合并树写进主配置。
                            None => false,
                        });
                        if found {
                            self.persist()
                        } else {
                            self.persist_overlay_last(label, &marked, None, Some(true))
                        }
                    }
                    RowOrigin::Patch => self.persist_overlay_last(label, &marked, None, Some(true)),
                },
            }
        };
        match result {
            Ok(()) => {
                self.clear_persisted_error(label);
                self.ctx.log(
                    Level::Info,
                    &format!("{label}: 插件自我卸载，已在配置里标为 disabled 并回写"),
                );
            }
            Err(err) => self.record_persisted_error(
                label,
                &plugin_name,
                format!("插件自我卸载，但配置回写失败: {err}"),
            ),
        }
    }

    /// 这一行来自主配置还是 patch 层（按"主树里有没有同一个 label"判定）。
    ///
    /// label 只由结构决定（[`stable_label`]），而 patch 的 `insert` 只会**追加**
    /// （根末尾 / 组末尾），不影响既有行的编号——所以主树上的同一 label
    /// 就是"这一行来自主配置"的可靠判据。
    fn origin_of(&self, label: &str) -> RowOrigin {
        let mut main = self.main_only.lock().unwrap();
        match find_by_label(&mut main, label) {
            Some(_) => RowOrigin::Main,
            None => RowOrigin::Patch,
        }
    }

    /// 把**主配置树**落盘（配置回写统一走这里）。
    ///
    /// **绝不**写 `self.tree`：它是主配置 + patch 合并后的结果（P1 的修复点）。
    ///
    /// 调用方**必须**已持 `config_gate`：整个"dump → 比较基线 → 原子替换"要和对候选的
    /// 采纳、以及其它 `main_only` 写入落在同一个临界区里，否则采纳可以插在中间，
    /// 把刚写的这一行整份盖回去。
    fn persist(&self) -> std::result::Result<(), String> {
        if !self.main_baseline_valid.load(Ordering::SeqCst) {
            return Err(format!(
                "{} 初始解析已降级，拒绝在未修复前覆写磁盘配置",
                self.files.main.display()
            ));
        }
        let text = {
            let main = self.main_only.lock().unwrap();
            let startup = self.main_startup.lock().unwrap();
            crate::patch::dump_entries_with_startup(&main, &startup)?
        };
        let baseline = self.main_baseline.lock().unwrap().clone();
        write_atomically_if_unchanged(&self.files.main, &text, &baseline)?;
        // 只有写成功才推进基线；失败时磁盘上的外部编辑原样保留。
        *self.main_baseline.lock().unwrap() = text;
        // 已采纳目标（磁盘 + 基线）变了：在飞/排队的旧候选基于旧版本，必须失效。
        self.bump_writeback();
        self.refresh_config_fingerprint();
        Ok(())
    }

    /// 把一条 overlay 追加到**指定的 patch 层**。
    ///
    /// - 有 `id`：追加 `- id: <id>` + `disabled`/`config`，下次解析时由该层生效；
    /// - 无 `id`：无从定位，**拒绝写盘**并报错（调用方记 Error 日志）。
    fn persist_overlay(
        &self,
        layer: usize,
        label: &str,
        row: &Entry,
        config: Option<&Config>,
        disabled: Option<bool>,
    ) -> std::result::Result<(), String> {
        let Some(id) = row.id.as_deref() else {
            return Err(format!(
                "行 \"{label}\" 来自 patch 层且没有 id，无法定位回写目标（拒绝写盘）"
            ));
        };
        let Some(patch) = self.files.patches.get(layer) else {
            return Err(format!(
                "行 \"{label}\" 的 patch 归属层 {layer} 不存在（配置层数可能在运行期变化，拒绝写盘）"
            ));
        };
        let text = crate::patch::dump_overlay(id, config, disabled)?;
        append_atomically(patch, &text)?;
        // 回写落到了 patch 层：已采纳目标变了，旧候选必须失效。
        self.bump_writeback();
        self.refresh_config_fingerprint();
        Ok(())
    }

    /// 兼容没有字段归属信息的旧路径：写到最后一个 patch 层。
    fn persist_overlay_last(
        &self,
        label: &str,
        row: &Entry,
        config: Option<&Config>,
        disabled: Option<bool>,
    ) -> std::result::Result<(), String> {
        let Some(layer) = self.files.patches.len().checked_sub(1) else {
            return Err(format!(
                "行 \"{label}\" 不在主配置里，且没有 patch 层可写（拒绝写盘）"
            ));
        };
        self.persist_overlay(layer, label, row, config, disabled)
    }

    /// 写盘之后同步文件指纹：否则 watcher 会把**我们自己写的内容**当成外部变更，
    /// 下一轮再触发一次对账。
    fn refresh_config_fingerprint(&self) {
        *self.config_fingerprint.lock().unwrap() = config_fingerprint(&self.files);
    }

    /// **稳读**：同一组文件读两遍、内容一致才采纳（编辑中的半截文件不采纳）。
    async fn read_settled(
        &self,
    ) -> std::result::Result<Option<FileSnapshot>, (PathBuf, std::io::Error)> {
        let mut read = |path: &Path| std::fs::read_to_string(path);
        read_settled_with(&self.files, self.settle, &mut read).await
    }

    /// 从快照解析条目树：主配置 + 依次叠 patch 层（语义与装载时**完全同一条**路径）。
    ///
    /// **纯函数**：结果只装进 [`ParsedSnapshot`]，一个共享字段都不写。落地归
    /// [`LoaderState::adopt`]（在 `mutation` + `config_gate` 下整份提交），于是
    /// "解析成功"与"配置被采纳"不再是同一个时刻——预检失败/候选过期时，
    /// 已采纳的回写元数据不会指向一份永远不会生效的配置。
    fn parse_snapshot(
        &self,
        snapshot: &FileSnapshot,
    ) -> std::result::Result<ParsedSnapshot, BuildCandidateError> {
        let (main, main_startup) = parse_entries_with_startup(&snapshot.main).map_err(|err| {
            BuildCandidateError::Persisted {
                label: self.files.main.display().to_string(),
                message: err,
            }
        })?;
        let mut entries = main.clone();
        let mut merged_startup = main_startup.clone();
        let mut owners = PatchOwners::default();
        for (index, text) in snapshot.patches.iter().enumerate() {
            let patch_label = self.files.patches[index].display().to_string();
            let patches =
                parse_patches_with_startup(text).map_err(|err| BuildCandidateError::Persisted {
                    message: format!("解析 patch {patch_label} 失败: {err}"),
                    label: patch_label,
                })?;
            let ctx = self.ctx.clone();
            let mut warn = |message: &str| ctx.log(Level::Warn, message);
            let (next_entries, next_startup) = apply_entry_patches_full(
                entries,
                merged_startup,
                &patches,
                index,
                &mut owners,
                &mut warn,
            );
            entries = next_entries;
            merged_startup = next_startup;
        }
        let row_startup = flatten_startup_table(&entries, &merged_startup);
        Ok(ParsedSnapshot {
            entries,
            main,
            main_startup,
            row_startup,
            owners,
            baseline: snapshot.main.clone(),
        })
    }

    /// 挂载一条展平后的行：加载（经 [`PluginSource`]）→ 挂载 → 收敛。
    ///
    /// `row.scope` 是这一行的**派生上下文**（M4-1：隔离域与 intercept 已按祖先链叠加）；
    /// 用根 ctx 挂等于把行悄悄搬回默认域。
    ///
    /// 这里是**首次挂载**的入口：`load` 与挂载相邻。换实例路径不走这里——
    /// 它先在拆卸之前 `load`（O5b-2），再调 [`Self::mount_loaded`] 复用那份 artifact。
    async fn mount_row(
        self: &Arc<Self>,
        row: &FlatRow,
    ) -> std::result::Result<Mounted, (LoadFailure, Vec<String>)> {
        let admission = self.source.admission();
        let loaded = self
            .source
            .load_classified(&row.entry, &self.search, self.next_revision(), &admission)
            .map_err(|failure| {
                let provides = self.declared_provides_for(&row.label, &row.entry, None, None);
                (failure, provides)
            })?;
        self.mount_loaded(row, loaded).await
    }

    /// 用**已经加载好**的 artifact 挂载一条展平后的行（O5b-2 的复用点）。
    ///
    /// 与 [`Self::mount_row`] 的唯一差别是"谁 `load`"：换实例路径要让
    /// resolve → stage → open → ping 全部发生在拆卸旧实例**之前**，所以 artifact
    /// 由调用方先拿到，这里只负责挂载与失败自清理——绝不二次加载。
    async fn mount_loaded(
        self: &Arc<Self>,
        row: &FlatRow,
        loaded: LoadedFile,
    ) -> std::result::Result<Mounted, (LoadFailure, Vec<String>)> {
        let fiber = mount_with_label(
            &row.scope,
            loaded.plugin.clone(),
            row.entry.config.clone().unwrap_or(Config::Null),
            row.entry.inject.clone(),
            &row.label,
        );
        // 挂入后立刻受 `RowTransactionGuard` 保护：若 `settle().await` 或失败自清理期间
        // future 被取消/drop，`Drop` 自动把槽位推进到 `Draining` 并后台 `dispose()` 孤儿 fiber。
        let tx_guard = self.begin_mount_slot(&row.label, fiber.clone());
        fiber.settle().await;
        if let Some(error) = fiber.error() {
            // `apply` 失败留下的 fiber 仍然占着 runtime 表（`fibers_by_plugin_name` 看得见），
            // 必须按 App 的生命周期等待预算做结构化清理；若后台清理仍在运行（例如 T2 `OP_APPLY`
            // 超时或半途 disposer 超时），推进到 `Draining` 阻止后续对账旁路重挂。
            let provides =
                self.declared_provides_for(&row.label, &row.entry, Some(&loaded.plugin), None);
            let close_report = fiber.dispose_with_report(None).await;
            if close_report.still_running {
                tx_guard.finish_draining(fiber);
                let detail = format!(
                    "{error}（失败清理未在预算内完成，状态={}，后台清理仍在进行，清理完成前禁止重挂）",
                    close_report.status.as_str()
                );
                return Err((LoadFailure::runtime(detail), provides));
            }
            tx_guard.finish_empty();
            return Err((LoadFailure::runtime(error), provides));
        }
        // volatile 声明在**装载成功之后**取（对应 §8 M4-5 的"paths 在挂载成功后查询"）：
        // 对 ABI 插件这是一次跨边界查询，早于 apply 问也可能得到"还没准备好"的答案。
        let volatile = volatile_of(fiber.volatile_paths());
        let origin = self.origin_of(&row.label);
        // 变体身份（WP2）：文件来源在挂载时对**已暂存副本**取一次 SHA-256；
        // 非文件来源（内建/假源）没有稳定内容，报告里填 "builtin"。
        let file_source = loaded.fingerprint.is_some();
        let sha256 = staged_sha256(&loaded.staged, file_source);
        let record = Mounted {
            label: row.label.clone(),
            origin,
            source: loaded.source,
            staged: loaded.staged,
            fingerprint: loaded.fingerprint,
            config: row.entry.config.clone().unwrap_or(Config::Null),
            inject: row.entry.inject.clone(),
            fiber: Arc::downgrade(&fiber),
            plugin: loaded.plugin,
            scope: row.scope.clone(),
            declared: row.entry.clone(),
            scope_inputs: row.scope_inputs.clone(),
            volatile,
            sha256,
            file_source,
        };
        tx_guard.commit_mounted(record.clone());
        Ok(record)
    }
}

/// `loader/patch-context` 尾链要干的事（M4-3 规格 4）。
///
/// 换实例路径会在调用 [`LoaderState::patch_context`] 时**额外**带上已加载的
/// `LoadedFile`（O5b-2 的预检前移）：动作仍是 `Mount`，只是 artifact 不在这里 load。
enum PatchWork {
    Mount,
    Update(Config),
}

/// 一次挂载决策（M4-2）：一行的"该不该挂" = **求值过的**派生 disabled。
///
/// 上游 `Entry.disabled` 是 getter：沿祖先链上溯、每次读都重新求值
/// （`entry.ts:70-85`）。这里保留同样的形状——**没有缓存**，
/// 于是上下文变了，下一次决策就会给出新的答案。
///
/// M4-4 起求值器**恒存在**（宿主引擎或内置子集，外面套着计数代理）：
/// "没有求值器"这个状态没有了，"表达式算不出来"成了唯一的失败态。
struct RowDecision<'a> {
    evaluator: &'a dyn JsEvaluator,
}

impl RowDecision<'_> {
    /// 派生 disabled：自身或**任一祖先**为真即禁用。
    ///
    /// 链上的 `!!js` 表达式用**这一行**的派生 ctx 求值（上游 `this.evaluate(...)` 同理）：
    /// `disabled: !!js "!ctx.get('profileContext')"` 读的是行自己的上下文。
    /// 求值失败 ⇒ `Err`（fail loud，绝不静默当成 `false`）。
    fn disabled(
        &self,
        chain: &[(String, Entry)],
        scope: &Ctx,
    ) -> std::result::Result<bool, String> {
        for (label, entry) in chain {
            if entry.disabled {
                return Ok(true);
            }
            if let Some(expr) = &entry.disabled_expr {
                let source = expr
                    .expression()
                    .map_err(|err| format!("{err} (at {label}.disabled)"))?;
                let value = self.evaluator.evaluate(&source, scope).map_err(|err| {
                    format!("!!js 求值失败: {source} (at {label}.disabled): {err}")
                })?;
                if js_truthy(&value) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// 创建装载器插件。
pub fn loader() -> Arc<dyn Plugin> {
    Arc::new(FnPlugin {
        name: LOADER_ID,
        key: cordis_core::unique_key(LOADER_ID),
        inject: Inject::default(),
        validator: Some(Arc::new(validate)),
        f: Arc::new(|ctx: Ctx, config: Config| {
            Box::pin(async move {
                let config: LoaderConfig =
                    serde_json::from_value(config).map_err(|err| err.to_string())?;
                load(ctx, config).await
            })
        }),
    })
}

/// 一次成功装载的产物（[`PluginSource`] 的返回）。
pub struct LoadedFile {
    pub plugin: Arc<dyn Plugin>,
    /// 用户提供的插件文件（HMR 指纹用；假实现可以给一个假路径）。
    pub source: PathBuf,
    /// 实际加载的副本（staged `.rN`；假实现同 `source`）。
    pub staged: PathBuf,
    pub fingerprint: Option<(u64, u64)>,
}

/// "一行配置 → 一个可用插件实例"这条链的抽象（M4-1 的 seam）。
///
/// 真实实现是磁盘上的动态库（resolve → stage → open → ping，见 [`DiskPluginSource`]）；
/// 测试用假实现直接给 `Arc<dyn Plugin>`——于是隔离域、intercept、label 稳定化
/// 这些**装载层语义**都能在没有动态库的情况下端到端测试。
pub trait PluginSource: Send + Sync {
    fn load(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
    ) -> std::result::Result<LoadedFile, String>;

    /// P1-1：尝试从来源自带的清单（sidecar manifest）收集依赖声明。
    ///
    /// **缺省返回 `Ok(None)`**：不发布清单的来源（假源、内置插件）不参与依赖预检。
    /// 装载器只把这里返回的 `Some` 转成 [`crate::plan::PlanEntry`] 并在挂载前求解；
    /// 它**不是**运行时注入，也不证明插件真的提供了这些服务。
    ///
    /// 准入判定**先于**清单结论：既没有显式来源策略、也没开开发模式时，凡是要读
    /// sidecar 的预检都会以「未配置插件来源策略」失败——manifest 无效/不合法都轮不到报。
    fn plan_entry(
        &self,
        _entry: &Entry,
        _search: &[PathBuf],
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        Ok(None)
    }

    /// O9：**这个来源对象**的准入结论。
    ///
    /// 缺省是 [`SourceAdmission::fallback`]——逐字等于今天的进程级语义，
    /// 所以既有实现（含测试里的假源）零改动、行为零变化。想要局部策略就把来源
    /// 包进 [`PolicyPluginSource`]：策略跟着对象走，而不是跟着全局量走。
    fn admission(&self) -> SourceAdmission {
        SourceAdmission::fallback()
    }

    /// O9：用**调用方给定的**准入结论装载。
    ///
    /// 为什么必须有它：`load` 里读的是 `self.admission()`，而包装层的 `self` 是
    /// **内层对象**——策略传不下去。`PolicyPluginSource` 因此调这一条，把
    /// 自己的结论显式交进来。
    ///
    /// **缺省实现忽略参数、逐字等于 [`Self::load`]**：不读全局量的来源（测试假源、
    /// 内置插件）的准入结论本来就与参数无关，所以行为不变。
    ///
    /// # 实现要求
    /// 1. **`load` 会读全局准入的实现必须同时覆写本方法**，并使用**传入的**
    ///    `admission`（而不是自己再读一次 `self.admission()` / 进程级全局）。否则
    ///    包装层、探测（[`crate::probe_capabilities`]）与热重载交进来的结论都会被丢掉，
    ///    同一次操作里"校验按局部策略、打开按全局"又变回两份快照。
    /// 2. **调用方显式传入的结论优先于来源自身的声明**：`load` 传的是
    ///    `self.admission()`；包装层转发的是**外层**的。嵌套包装时最外层的显式
    ///    声明有效，内层不得用自己的结论覆盖它。
    fn load_admitted(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        _admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, String> {
        self.load(entry, search, revision)
    }

    /// 带故障分类的装载入口：默认把 [`Self::load_admitted`] 的错误归类为
    /// [`FailureCategory::RuntimeUnavailable`]。
    ///
    /// [`DiskPluginSource`] 与 [`PolicyPluginSource`] 覆写本方法以保留精确的
    /// `Source` / `Integrity` / `RuntimeUnavailable` 分类。
    fn load_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, LoadFailure> {
        self.load_admitted(entry, search, revision, admission)
            .map_err(LoadFailure::runtime)
    }

    /// O9：`plan_entry` 的准入版本，语义与 [`Self::load_admitted`] 相同。
    ///
    /// 依赖声明也要过**同一个**准入结论：否则包装源会一边用局部策略装载、
    /// 一边用别的结论做依赖预检。
    ///
    /// # 实现要求
    /// 与 [`Self::load_admitted`] 的两条一致：`plan_entry` 里会读全局准入的实现
    /// 必须覆写本方法并用传入的 `admission`；调用方显式传入的结论优先于来源自身的
    /// 声明。`plan_entry` 与 `load` 必须**同源**，否则"预检通过、装载被拒"（或反之）
    /// 会变成配置层面的谜题。
    ///
    /// 准入判定同样**先于**清单结论：既没有显式来源策略、也没开开发模式时，凡是要读
    /// sidecar 的预检都会以「未配置插件来源策略」失败——manifest 无效/不合法都轮不到报。
    fn plan_entry_admitted(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        _admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        self.plan_entry(entry, search)
    }

    /// 带故障分类的依赖声明读取入口：默认将 [`Self::plan_entry_admitted`] 的错误归类为
    /// [`FailureCategory::RuntimeUnavailable`]。
    ///
    /// [`DiskPluginSource`] 与 [`PolicyPluginSource`] 覆写本方法以保留精确的
    /// `Source` / `Integrity` / `RuntimeUnavailable` 分类，使 `strict_sidecars = true`
    /// 依赖预检与行级 `Optional` 及 [`StartupPolicy`] 的独立处置开关保持一致。
    fn plan_entry_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, LoadFailure> {
        self.plan_entry_admitted(entry, search, admission)
            .map_err(LoadFailure::runtime)
    }
}

/// 给一个来源对象套上**局部准入结论**（O9）。
///
/// # 为什么是包装而不是配置项
/// 策略必须跟着**来源对象**走：`PluginSource::load` 的签名里没有 `ctx`，来源本来就
/// 不知道自己在哪个 App 里；而给 `LoaderConfig` 加字段会破坏所有既有字面量构造。
/// 包装层两头都躲开了：`load` / `plan_entry` 转发时把自己的结论经
/// [`PluginSource::load_admitted`] / [`PluginSource::plan_entry_admitted`] 交给内层，
/// 并覆写 [`PluginSource::admission`] 让装载器（`reload_one`、探测）看到同一份结论。
///
/// # 三条硬语义（见 [`SourceAdmission`]）
/// - [`Self::strict`]：显式严格，**忽略**进程级开发标志；
/// - [`Self::dev_unverified`]：只对这个来源放行，**不写**全局标志；
/// - 没有包装的来源仍然是 fallback，逐字等于今天的全局语义。
///
/// # 覆盖面
/// 包装只对**覆写了 `*_admitted`** 的来源有效。只实现 `load` / `plan_entry`
/// 且自己读进程级全局的第三方来源，包装层收紧不了它——转发进去的结论会被它
/// 忽略，`admission()` 也只是多摆一个没人看的声明。
pub struct PolicyPluginSource {
    inner: Arc<dyn PluginSource>,
    admission: SourceAdmission,
}

impl PolicyPluginSource {
    /// 显式严格：artifact 必须过这份 allow-list，全局开发标志不得放宽它。
    pub fn strict(
        inner: Arc<dyn PluginSource>,
        policy: impl Into<Arc<PluginSourcePolicy>>,
    ) -> Self {
        Self {
            inner,
            admission: SourceAdmission::strict(policy),
        }
    }

    /// 显式开发放行：只对这个来源跳过校验，不碰进程级标志。
    pub fn dev_unverified(inner: Arc<dyn PluginSource>) -> Self {
        Self {
            inner,
            admission: SourceAdmission::dev_unverified(),
        }
    }
}

impl PluginSource for PolicyPluginSource {
    fn load(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        self.load_admitted(entry, search, revision, &self.admission)
    }

    fn load_admitted(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, String> {
        // 谁发起这次调用、谁的结论说了算：`load` 传的是自己的，外层包装转发的是
        // 外层的。这一层不再拿自己的覆盖它——嵌套包装时最外层的显式声明有效。
        self.inner.load_admitted(entry, search, revision, admission)
    }

    fn load_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, LoadFailure> {
        self.inner
            .load_classified(entry, search, revision, admission)
    }

    fn plan_entry(
        &self,
        entry: &Entry,
        search: &[PathBuf],
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        self.plan_entry_admitted(entry, search, &self.admission)
    }

    fn plan_entry_admitted(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        self.inner.plan_entry_admitted(entry, search, admission)
    }

    fn plan_entry_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, LoadFailure> {
        self.inner.plan_entry_classified(entry, search, admission)
    }

    fn admission(&self) -> SourceAdmission {
        self.admission.clone()
    }
}

/// 真实实现：磁盘上的动态库。预检（ABI 版本、结构尺寸、入口可调用性）全在
/// 动到正在跑的实例之前完成。来源带策略（缺省回落到进程级
/// [`crate::source::active_policy`]）时，artifact 还要先通过 sidecar manifest +
/// allow-list 校验才会被暂存。
pub struct DiskPluginSource;

impl DiskPluginSource {
    /// 装载流水线：准入结论由调用方给定。
    ///
    /// 单独抽出来是因为包装层（[`PolicyPluginSource`]）必须能把**自己的**结论交给
    /// 它——`PluginSource::load` 里的 `self` 在内层对象上，读不到外层的策略。
    fn load_with(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, String> {
        self.load_with_classified(entry, search, revision, admission)
            .map_err(|err| err.message)
    }

    fn load_with_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, LoadFailure> {
        let path = resolve_plugin_path(&entry.name, search)
            .map_err(|err| LoadFailure::runtime(err.to_string()))?;
        // P0-1a：有生效的来源策略时，**暂存之前**先验 sidecar manifest 与
        // allow-list。这样未登记 / 被篡改的 artifact 不会产生暂存副本，更不会
        // 走到 `LoadLibrary`/`dlopen`。
        //
        // O9：结论来自来源对象本身（裸源 ⇒ fallback ⇒ 读进程级全局，行为不变）；
        // 包在 `PolicyPluginSource` 里时则是**这个来源**自己的策略，且只读这一次
        // ——暂存与二次校验共用同一份快照。
        let active_policy = admission.resolve().map_err(LoadFailure::source)?;
        if let Some(policy) = &active_policy {
            policy
                .verify_classified(&path)
                .map_err(|err| LoadFailure::new(err.category, err.message))?;
        }
        let source_fingerprint = fingerprint(&path);
        let staged =
            stage_plugin(&path, revision).map_err(|err| LoadFailure::runtime(err.to_string()))?;
        // 二次校验 + 暂存副本 hash：防的是 verify 与 stage 之间的替换窗口。
        let opened = match &active_policy {
            Some(policy) => LoadedPlugin::open_staged_verified_classified(&path, &staged, policy),
            None => LoadedPlugin::open_staged_classified(&staged),
        };
        let plugin = match opened {
            Ok(plugin) => plugin,
            Err(err) => {
                // 打不开的副本永远不会被认领（与 `reload_one` 的预检失败分支同口径）：
                // 立刻销账并删除，否则每次坏 artifact 都在泄漏台账里留下一个 `.rN`
                // （`stagedLeaked` 只增不减）——预检失败必须零副作用。
                drop(StagedFile::new(&staged));
                return Err(LoadFailure::new(err.category, err.message));
            }
        };
        // 预检：ABI 版本、结构尺寸、插件入口可调用性，全在动到正在跑的实例之前完成。
        // （探活失败时 `plugin` 在此 drop：先 FreeLibrary、后删暂存文件——不会泄漏。）
        if !plugin.ping() {
            return Err(LoadFailure::runtime(
                "ABI 探活失败（版本不匹配，或入口无法调用）",
            ));
        }
        Ok(LoadedFile {
            plugin: Arc::new(plugin),
            source: path,
            staged,
            fingerprint: source_fingerprint,
        })
    }

    /// 依赖声明的准入版本（见 [`Self::load_with`]）。
    ///
    /// 与 [`PluginSource::load`] 共用 [`crate::resolve_plugin_path`]，只读 sidecar
    /// manifest，不暂存、不打开动态库。
    ///
    /// 找不到 artifact 时返回 `Ok(None)`：没有 artifact 就不可能有 sidecar，
    /// 这里不该替换 `load` 的逐行错误（`strict=false` 的降级语义保持不变）。
    fn plan_entry_with(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        self.plan_entry_with_classified(entry, search, admission)
            .map_err(|err| err.message)
    }

    fn plan_entry_with_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, LoadFailure> {
        let Ok(path) = resolve_plugin_path(&entry.name, search) else {
            return Ok(None);
        };
        let manifest_path = crate::source::manifest_path(&path);
        if !manifest_path.is_file() {
            return Ok(None);
        }
        // O9：**先解析准入结论，再读 sidecar**。同一条配置在两个环境里必须给出同一个
        // 首错——"未配置策略 / 被策略拒绝"优先于"manifest 无效"，否则运维看到的
        // 究竟是"准入没配"还是"发布方写坏了清单"，会随机器漂移。
        //
        // 与 [`Self::load_with`] 共用**同一个读口**（同一份 `SourceAdmission`），
        // 否则"依赖预检按一种准入、真正装载按另一种"会让结论互相矛盾。
        let active_policy = admission.resolve().map_err(LoadFailure::source)?;
        let manifest = crate::source::read_manifest(&path).map_err(|err| {
            LoadFailure::integrity(format!(
                "插件 {} (id={}) 的 sidecar manifest {} 无效: {err}",
                entry.name,
                entry.id.as_deref().unwrap_or("<无 id>"),
                manifest_path.display()
            ))
        })?;
        crate::source::validate_manifest(&manifest).map_err(|err| {
            LoadFailure::integrity(format!(
                "插件 {} (id={}) 的 sidecar manifest 身份无效: {err}",
                entry.name,
                entry.id.as_deref().unwrap_or("<无 id>")
            ))
        })?;
        // 有生效来源策略时，依赖声明也必须先过 artifact/manifest 准入，
        // 不能从未验证的 sidecar 直接做依赖决策。
        if let Some(policy) = &active_policy {
            policy.verify_classified(&path).map_err(|err| {
                LoadFailure::new(
                    err.category,
                    format!(
                        "插件 {} (id={}) 的 sidecar manifest 未通过来源准入: {}",
                        entry.name,
                        entry.id.as_deref().unwrap_or("<无 id>"),
                        err.message
                    ),
                )
            })?;
        }
        Ok(Some(PlanEntrySpec {
            id: manifest.id,
            version: manifest.version,
            provides: manifest.provides,
            requires: manifest.requires,
        }))
    }
}

impl PluginSource for DiskPluginSource {
    fn load(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        self.load_with(entry, search, revision, &self.admission())
    }

    fn load_admitted(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, String> {
        self.load_with(entry, search, revision, admission)
    }

    fn load_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
        admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, LoadFailure> {
        self.load_with_classified(entry, search, revision, admission)
    }

    fn plan_entry(
        &self,
        entry: &Entry,
        search: &[PathBuf],
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        self.plan_entry_with(entry, search, &self.admission())
    }

    fn plan_entry_admitted(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        self.plan_entry_with(entry, search, admission)
    }

    fn plan_entry_classified(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        admission: &SourceAdmission,
    ) -> std::result::Result<Option<PlanEntrySpec>, LoadFailure> {
        self.plan_entry_with_classified(entry, search, admission)
    }
}

/// 一次声明预检的**分类结论**（O7）。
///
/// [ADR 0007]：这是**声明**的结论，不是运行时事实——`Checked` 不代表绑定成功、
/// 不代表插件 Active、也不代表服务在目标 realm 里可见。报告只做诊断：任何分类都
/// **不改变**预检的门禁结果（Ok/Err 与 O7 之前逐字一致）。
///
/// [ADR 0007]: ../../docs/adr/0007-version-is-diagnostic-only.md
#[derive(Clone, Debug, PartialEq, Eq)]
enum PreflightReport {
    /// 本次预检不适用：显式 `dependencyPlan` 覆盖 manifest，或没有可检查的启用行。
    NotApplicable { reason: PreflightNotApplicable },
    /// 有行没有声明（`Ok(None)`）⇒ 整次预检保守跳过（混合来源）。
    Incomplete { rows: Vec<PreflightRow> },
    /// 宽松模式（`strict_sidecars=false`）下有行 `Err` ⇒ 整次预检保守跳过，
    /// 但每一行的**原始错误文本**必须留在报告里。
    Downgraded { rows: Vec<PreflightRow> },
    /// 声明完整且求解通过：`providers` 是求解器的结论，**不是**运行期绑定。
    Checked {
        entries: usize,
        providers: Vec<PreflightProvider>,
    },
}

impl PreflightReport {
    fn to_json(&self) -> serde_json::Value {
        // 三枚适用边界标记对每一种分类都一样，写一次以免某个分支漏标。
        // `skipped` 显式表达"整次预检被跳过"：`incomplete`/`downgraded` 两种判定
        // 都意味着这一轮**什么都没检查**，读报告的人不必从 verdict 名去推。
        let mut value = json!({
            "scope": "declaration",
            "runtimeVerified": false,
            "realmModel": "notModeled",
        });
        let object = value.as_object_mut().expect("json! 造的是对象");
        match self {
            Self::NotApplicable { reason } => {
                object.insert("verdict".into(), json!("notApplicable"));
                object.insert("declaration".into(), json!(reason.declaration_kind()));
                object.insert("reason".into(), json!(reason.as_str()));
                object.insert("skipped".into(), json!(false));
            }
            Self::Incomplete { rows } => {
                object.insert("verdict".into(), json!("incomplete"));
                object.insert("declaration".into(), json!("sidecarManifest"));
                object.insert("rows".into(), json!(rows_json(rows)));
                object.insert("skipped".into(), json!(true));
            }
            Self::Downgraded { rows } => {
                object.insert("verdict".into(), json!("downgraded"));
                object.insert("declaration".into(), json!("sidecarManifest"));
                object.insert("rows".into(), json!(rows_json(rows)));
                object.insert("skipped".into(), json!(true));
            }
            Self::Checked { entries, providers } => {
                object.insert("verdict".into(), json!("checked"));
                object.insert("declaration".into(), json!("sidecarManifest"));
                object.insert("entries".into(), json!(entries));
                // `declaredProviders`：求解器按**声明**选出的提供者，不是运行期绑定。
                object.insert(
                    "declaredProviders".into(),
                    json!(providers
                        .iter()
                        .map(PreflightProvider::to_json)
                        .collect::<Vec<_>>()),
                );
                object.insert("skipped".into(), json!(false));
            }
        }
        value
    }
}

fn rows_json(rows: &[PreflightRow]) -> Vec<serde_json::Value> {
    rows.iter().map(PreflightRow::to_json).collect()
}

/// `NotApplicable` 的两种原因。名字是报告的一部分，测试与运维都按它取值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreflightNotApplicable {
    /// 显式 `LoaderConfig.dependencyPlan` 覆盖 manifest ⇒ `plan_entry` 根本不会被调用。
    ExplicitPlan,
    /// 没有任何可检查的启用行（全静态 `disabled` / 只有 group 行）。
    NoEnabledRows,
}

impl PreflightNotApplicable {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitPlan => "explicitPlan",
            Self::NoEnabledRows => "noEnabledRows",
        }
    }

    /// 报告里的 `declaration` 字段：这次结论的声明来源。
    ///
    /// `NoEnabledRows` 是 `none`：没有可检查的启用行，这一轮**根本没读** sidecar
    /// manifest，报 `sidecarManifest` 会让人以为读过它。
    fn declaration_kind(self) -> &'static str {
        match self {
            Self::ExplicitPlan => "explicitPlan",
            Self::NoEnabledRows => "none",
        }
    }
}

/// 预检里的一行（non-group 且静态启用）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct PreflightRow {
    /// 非 group 行的遍历序号（与 `CollectedPlanEntry::index` 同一套编号）。
    index: usize,
    label: String,
    plugin: String,
    verdict: PreflightRowVerdict,
}

impl PreflightRow {
    fn to_json(&self) -> serde_json::Value {
        let mut value = json!({
            "index": self.index,
            "label": self.label,
            "plugin": self.plugin,
        });
        let object = value.as_object_mut().expect("json! 造的是对象");
        match &self.verdict {
            PreflightRowVerdict::Declared => object.insert("verdict".into(), json!("declared")),
            PreflightRowVerdict::NoDeclaration => {
                object.insert("verdict".into(), json!("noDeclaration"))
            }
            PreflightRowVerdict::Downgraded { reason } => {
                object.insert("verdict".into(), json!("downgraded"));
                object.insert("reason".into(), json!(reason))
            }
        };
        value
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreflightRowVerdict {
    /// 有 sidecar manifest 计划。
    Declared,
    /// `Ok(None)`：这一行没有声明。
    NoDeclaration,
    /// 宽松模式下的 `Err`：保留原始错误文本（降级不等于放行，见 `LoaderConfig::strict_sidecars`）。
    Downgraded { reason: String },
}

/// 求解器选出的一个服务键提供者（声明版本，不是运行期绑定的证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct PreflightProvider {
    key: String,
    plugin: String,
    version: String,
}

impl PreflightProvider {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "key": self.key,
            "plugin": self.plugin,
            "version": self.version,
        })
    }
}

/// 收集阶段的记账：既喂求解器（`entries`），也喂报告（`collection`）。
#[derive(Default)]
struct PreflightCollection {
    rows: Vec<PreflightRow>,
    missing_plan: bool,
    downgraded: bool,
}

/// P1-1：从来源清单收集依赖计划，并在**任何 `source.load` / 挂载之前**求解。
///
/// 只遍历非 group 行：
/// - 静态 `disabled: true` 跳过；`disabled: !!js …` 保守纳入计划（运行时求值后才知
///   是否挂载，不能在预检里把可能的提供者误判为缺失）；
/// - 只要有一行没有 sidecar/manifest 计划，整次 manifest 预检跳过：混合来源下
///   不能把“没声明”当成“没有提供者”；
/// - group 行只下钻到子行。
///
/// 有生效来源策略时，读取 sidecar 前会先过 admission；版本/范围解析失败或
/// [`crate::plan::resolve`] 报错都会让本次装载整体失败。`strict_sidecars` 只决定
/// [`PluginSource::plan_entry`] 报错时是整体失败，还是当作"该行没有计划"并跳过整次预检。
///
/// 返回值是**分类报告**：`Err` 仍是同一个门禁失败（错误文本不变），`Ok` 则说明这次
/// 预检走到了哪一步（[`PreflightReport`]）。
fn preflight_source_dependency_plan(
    entries: &[Entry],
    search: &[PathBuf],
    source: &dyn PluginSource,
    strict_sidecars: bool,
    startup_policy: Option<&StartupPolicy>,
    row_startup: &HashMap<String, RowStartup>,
) -> std::result::Result<(PreflightReport, HashMap<String, Vec<String>>), String> {
    let mut collector =
        PlanCollector::new(search, source, strict_sidecars, startup_policy, row_startup);
    collector.walk(entries, false)?;
    let mut declared_provides: HashMap<String, Vec<String>> = HashMap::new();
    for row in &collector.entries {
        let mut keys: Vec<String> = row.spec.provides.keys().cloned().collect();
        keys.sort();
        keys.dedup();
        if !keys.is_empty() {
            declared_provides
                .entry(row.stable_label.clone())
                .or_insert(keys);
        }
    }
    if collector.collection.rows.is_empty() {
        // 一行可检查的都没有：没有声明可谈（全部静态 disabled / 只有组行）。
        return Ok((
            PreflightReport::NotApplicable {
                reason: PreflightNotApplicable::NoEnabledRows,
            },
            declared_provides,
        ));
    }
    // 混合来源（部分插件没有 sidecar）下 manifest 计划不完整；不能把
    // “没声明”误判成“没有提供者”。此时跳过预检，保留既有运行时语义。
    if collector.collection.missing_plan {
        let downgraded = collector.collection.downgraded;
        let rows = std::mem::take(&mut collector.collection.rows);
        return Ok((
            if downgraded {
                PreflightReport::Downgraded { rows }
            } else {
                PreflightReport::Incomplete { rows }
            },
            declared_provides,
        ));
    }

    let mut plan = Vec::with_capacity(collector.entries.len());
    for row in collector.entries {
        let plugin_id = row.spec.id.clone();
        let entry = row.spec.into_entry().map_err(|err| {
            format!(
                "依赖计划（来源 manifest）第 {} 行 {label} (id={plugin_id}) 无效: {err}",
                row.index,
                label = row.label,
            )
        })?;
        // 同一插件挂多行是合法配置；完全相同的 manifest 计划只应算一次。
        // 同 id/version 但声明不同仍交给 resolve 报歧义。
        if plan.iter().any(|existing| existing == &entry) {
            continue;
        }
        plan.push(entry);
    }
    let resolved = crate::plan::resolve(&plan).map_err(|findings| {
        format!(
            "依赖计划预检失败（来源 sidecar manifest；{} 个问题）:\n  - {}\n（声明预检不建模 realm：跨私有域的同键同版本会被判为歧义）",
            findings.len(),
            findings.join("\n  - ")
        )
    })?;
    // 求解器**只**按声明选提供者：它不知道 realm（报告里 `realmModel=notModeled`），
    // 也不知道运行期到底谁注册成功（ADR 0007）。
    let providers = resolved
        .providers
        .into_iter()
        .map(|(key, provider)| PreflightProvider {
            key,
            plugin: provider.plugin,
            version: provider.version.to_string(),
        })
        .collect();
    Ok((
        PreflightReport::Checked {
            entries: plan.len(),
            providers,
        },
        declared_provides,
    ))
}

struct CollectedPlanEntry {
    /// 非 group 行的遍历序号（与跳过/收集无关，便于错误定位）。
    index: usize,
    label: String,
    /// 与装载器一致的稳定结构行标（`id` 或 `name#N`），用于缓存 `preflight_provides`。
    stable_label: String,
    spec: PlanEntrySpec,
}

/// 遍历一棵条目树的收集器：把"查 source、记账、编号行号"三个输入收进一个对象，
/// 于是递归只是 `walk(children, disabled)`。
struct PlanCollector<'a> {
    search: &'a [PathBuf],
    source: &'a dyn PluginSource,
    strict_sidecars: bool,
    startup_policy: Option<&'a StartupPolicy>,
    row_startup: &'a HashMap<String, RowStartup>,
    /// 非 group 行的遍历序号（与跳过/收集无关，便于错误定位）。
    next_row: usize,
    /// 与 [`DesiredWalk`] / [`find_by_label`] 保持同一走法的 DFS 匿名行序号表。
    ordinals: HashMap<String, usize>,
    /// 收集到的计划条目（喂 [`crate::plan::resolve`]）。
    entries: Vec<CollectedPlanEntry>,
    /// 逐行判定与"整次跳过"的原因（喂 [`PreflightReport`]）。
    collection: PreflightCollection,
}

impl<'a> PlanCollector<'a> {
    fn new(
        search: &'a [PathBuf],
        source: &'a dyn PluginSource,
        strict_sidecars: bool,
        startup_policy: Option<&'a StartupPolicy>,
        row_startup: &'a HashMap<String, RowStartup>,
    ) -> Self {
        Self {
            search,
            source,
            strict_sidecars,
            startup_policy,
            row_startup,
            next_row: 0,
            ordinals: HashMap::new(),
            entries: Vec::new(),
            collection: PreflightCollection::default(),
        }
    }

    fn allow_preflight_degrade(
        &self,
        row_stable_label: &str,
        entry_id: Option<&str>,
        category: FailureCategory,
    ) -> bool {
        if !self.strict_sidecars {
            return true;
        }
        let effective_startup = self
            .startup_policy
            .and_then(|policy| policy.row_startup(row_stable_label, entry_id))
            .or_else(|| self.row_startup.get(row_stable_label).copied())
            .unwrap_or(RowStartup::Required);
        match category {
            FailureCategory::RuntimeUnavailable => effective_startup == RowStartup::Optional,
            FailureCategory::Source => self
                .startup_policy
                .is_some_and(|policy| policy.source() == FailureDisposition::Degrade),
            FailureCategory::Integrity => self
                .startup_policy
                .is_some_and(|policy| policy.integrity() == FailureDisposition::Degrade),
            FailureCategory::PersistedState => self
                .startup_policy
                .is_some_and(|policy| policy.persisted_state() == FailureDisposition::Degrade),
        }
    }

    fn walk(
        &mut self,
        entries: &[Entry],
        disabled_by_ancestor: bool,
    ) -> std::result::Result<(), String> {
        let admission = self.source.admission();
        for entry in entries {
            // 先按整棵树的结构走法计算稳定行标，保证即使存在组行或静态 disabled 行，
            // 匿名行的 `name#N` 编号也与 `LoaderState` 完全一致。
            let row_stable_label = stable_label(entry, &mut self.ordinals);
            // 静态 disabled 可确定跳过；`disabled: !!js` 的求值需要上下文，
            // 预检阶段保守地把它纳入计划，避免把运行时 false 的提供者误判为缺失。
            let disabled = disabled_by_ancestor || entry.disabled;
            if entry.group {
                self.walk(&entry.children, disabled)?;
                continue;
            }
            let index = self.next_row;
            self.next_row += 1;
            if disabled {
                continue;
            }
            let label = entry.id.clone().unwrap_or_else(|| entry.name.clone());
            let verdict = match self
                .source
                .plan_entry_classified(entry, self.search, &admission)
            {
                Ok(Some(spec)) => {
                    self.entries.push(CollectedPlanEntry {
                        index,
                        label: label.clone(),
                        stable_label: row_stable_label,
                        spec,
                    });
                    PreflightRowVerdict::Declared
                }
                Ok(None) => {
                    self.collection.missing_plan = true;
                    PreflightRowVerdict::NoDeclaration
                }
                Err(failure)
                    if !self.allow_preflight_degrade(
                        &row_stable_label,
                        entry.id.as_deref(),
                        failure.category,
                    ) =>
                {
                    return Err(format!(
                        "第 {index} 行 {label}（插件 {}）的依赖声明不可用: {}",
                        entry.name, failure.message
                    ));
                }
                // 宽松模式或已按行级 Optional / StartupPolicy 允许降级：把该行记为降级并跳过全局预检，
                // 留待对账装载期落入 `failedRows`/`degraded` 并保留原始错误文本。
                Err(failure) => {
                    self.collection.missing_plan = true;
                    self.collection.downgraded = true;
                    PreflightRowVerdict::Downgraded {
                        reason: failure.message,
                    }
                }
            };
            self.collection.rows.push(PreflightRow {
                index,
                label,
                plugin: entry.name.clone(),
                verdict,
            });
        }
        Ok(())
    }
}

async fn load(ctx: Ctx, config: LoaderConfig) -> Result<()> {
    load_with_source(ctx, config, Arc::new(DiskPluginSource))
        .await
        .map(|_| ())
}

/// [`load`] 的可注入版本：测试用假 [`PluginSource`] 顶替动态库加载。
///
/// 返回 [`LoaderState`]：装载器不是"装完就定死"——运行期启停（[`LoaderState::set_disabled`]）
/// 与对账（[`LoaderState::reconcile_once`] / [`LoaderState::reload`]）都要在这个状态上继续做。
///
/// # 依赖预检（P1-1）
/// `dependencyPlan` 非空时只求解它（覆盖、不合并）；为空时逐行调用
/// [`PluginSource::plan_entry`] 收集 sidecar manifest 的 `provides` / `requires`。
/// 两条路径都在**创建 [`LoaderState`]、调用 `source.load` 或挂载之前**完成；
/// 失败会直接返回错误，不会留下半棵树。对账与 `reload_one` 会重跑同一预检。
pub async fn load_with_source(
    ctx: Ctx,
    config: LoaderConfig,
    source: Arc<dyn PluginSource>,
) -> Result<Arc<LoaderState>> {
    // P0-2：先登记本装载器所在 App 的看门狗，再做任何挂载。实例在
    // `register_instance_with_keepalive` 里取快照，所以这必须发生在 `sync()` 之前。
    // 键是 `ctx.store` 的同一性：只覆盖这个 App，不影响同进程的其他 App。
    if let Some(policy) = config.dispatch_policy {
        set_dispatch_policy_for(&ctx, policy);
    }
    // P0-3：容量上限同样必须在任何挂载之前登记——实例在注册时取快照。
    // 0 是非法配置：这里直接让装载失败，绝不把“忘记填”解释成“永不接纳”。
    if let Some(limits) = config.host_limits {
        set_host_limits_for(&ctx, limits)
            .map_err(|err| format!("LoaderConfig.hostLimits 无效: {err}"))?;
    }
    // P1-1：显式依赖计划在**任何挂载之前**求解。失败即整体失败：预检之后
    // 才轮得到读配置、解析插件路径与 `source.load`，所以不会有半棵树被挂上。
    if !config.dependency_plan.is_empty() {
        let mut plan = Vec::with_capacity(config.dependency_plan.len());
        for (index, spec) in config.dependency_plan.iter().enumerate() {
            let entry = spec.clone().into_entry().map_err(|err| {
                format!(
                    "LoaderConfig.dependencyPlan[{index}] (id={}) 无效: {err}",
                    spec.id
                )
            })?;
            plan.push(entry);
        }
        if let Err(findings) = crate::plan::resolve(&plan) {
            return Err(format!(
                "依赖计划预检失败（LoaderConfig.dependencyPlan；{} 个问题）:\n  - {}",
                findings.len(),
                findings.join("\n  - ")
            )
            .into());
        }
    }
    let main_path = PathBuf::from(&config.path);
    let base_dir = main_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let startup_policy = startup_policy_for(&ctx);
    let degrade_persisted = should_degrade_failure(
        startup_policy.as_ref(),
        config.strict,
        FailureCategory::PersistedState,
        RowStartup::Required,
    );
    let mut initial_persisted_errors: Vec<FailedRow> = Vec::new();

    // 1) 读主配置。主树单独留一份：配置回写只能写它（patch 合并结果写回主配置会把
    //    patch 插入的行固化下来，见 P1）。
    //    若主配置读取或解析失败，属于 PersistedState 错误：默认 fail-closed，
    //    仅在宿主显式配置 `StartupPolicy::with_persisted_state(Degrade)` 时降级为空树
    //    并标记 `main_baseline_valid = false`（防止运行时回写覆盖损坏的原文件）。
    let (text, main_entries, main_startup, main_baseline_valid) =
        match std::fs::read_to_string(&main_path)
            .map_err(|err| format!("读取 {} 失败: {err}", main_path.display()))
            .and_then(|text| {
                parse_entries_with_startup(&text).map(|(entries, startup)| (text, entries, startup))
            }) {
            Ok((text, entries, startup)) => (text, entries, startup, true),
            Err(err) if degrade_persisted => {
                let label = main_path.display().to_string();
                ctx.log(
                    Level::Warn,
                    &format!("{label}: 持久化状态错误（已按策略降级为空配置）: {err}"),
                );
                initial_persisted_errors.push(FailedRow {
                    label,
                    plugin: String::new(),
                    category: FailureCategory::PersistedState,
                    startup: RowStartup::Required,
                    degraded: true,
                    provides: Vec::new(),
                    message: err,
                });
                (String::new(), Vec::new(), Vec::new(), false)
            }
            Err(err) => return Err(err.into()),
        };
    let mut entries = main_entries.clone();
    let mut merged_startup = main_startup.clone();

    // 2) 依次叠加 patch 层。语义与 `applyEntryPatches` 完全一致，纯函数、可单测。
    //    路径在这里就解析成**绝对路径**：watcher 与对账之后每一轮都要按同一份清单读文件。
    let patches: Vec<PathBuf> = config
        .patches
        .iter()
        .map(|patch| {
            if PathBuf::from(patch).is_absolute() {
                PathBuf::from(patch)
            } else {
                base_dir.join(patch)
            }
        })
        .collect();
    let mut owners = PatchOwners::default();
    for (layer, patch_path) in patches.iter().enumerate() {
        let parsed_layer = std::fs::read_to_string(patch_path)
            .map_err(|err| format!("读取 patch {} 失败: {err}", patch_path.display()))
            .and_then(|text| {
                parse_patches_with_startup(&text)
                    .map_err(|err| format!("解析 patch {} 失败: {err}", patch_path.display()))
            });
        let layer_patches = match parsed_layer {
            Ok(patches) => patches,
            Err(err) if degrade_persisted => {
                let label = patch_path.display().to_string();
                ctx.log(
                    Level::Warn,
                    &format!("{label}: 持久化状态错误（已按策略跳过该 patch 层）: {err}"),
                );
                initial_persisted_errors.push(FailedRow {
                    label,
                    plugin: String::new(),
                    category: FailureCategory::PersistedState,
                    startup: RowStartup::Required,
                    degraded: true,
                    provides: Vec::new(),
                    message: err,
                });
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let mut warn = |message: &str| ctx.log(Level::Warn, message);
        let (next_entries, next_startup) = apply_entry_patches_full(
            entries,
            merged_startup,
            &layer_patches,
            layer,
            &mut owners,
            &mut warn,
        );
        entries = next_entries;
        merged_startup = next_startup;
    }
    let row_startup = flatten_startup_table(&entries, &merged_startup);

    // 3) 运行时状态。插件默认从主配置所在目录解析，可被 pluginDir 覆盖。
    let search: Vec<PathBuf> = match &config.plugin_dir {
        Some(dir) => vec![PathBuf::from(dir)],
        None => vec![base_dir.clone()],
    };
    // P1-1：没有显式 dependencyPlan 时，从每个静态启用行的 sidecar manifest 收集
    // provides / requires，并在创建 LoaderState、调用 source.load 之前求解。
    // 显式计划是**覆盖**：非空时上面的预检已经跑过，这里既不合并也不读 manifest。
    // 结论进 `LoaderState`（O7 报告）：首轮预检的分类与对账/重载时同一套口径。
    let (preflight, preflight_provides) = if config.dependency_plan.is_empty() {
        preflight_source_dependency_plan(
            &entries,
            &search,
            source.as_ref(),
            config.strict_sidecars,
            startup_policy.as_ref(),
            &row_startup,
        )?
    } else {
        (
            PreflightReport::NotApplicable {
                reason: PreflightNotApplicable::ExplicitPlan,
            },
            HashMap::new(),
        )
    };
    // 稳读窗口取轮询周期的四分之一（下限 10ms）：编辑器的"写临时文件再 rename"
    // 通常比这快得多，于是既不会误采纳半截文件，也不会让人等一个完整周期。
    let settle = config
        .hmr
        .as_ref()
        .map(|hmr| Duration::from_millis((hmr.interval_ms / 4).max(10)))
        .unwrap_or_else(|| Duration::from_millis(50));
    // `!!js` 的生效引擎（M4-4 + M9）：先看模式，再看宿主注入了什么。
    // - `builtin`（缺省）：注入的引擎**覆盖**内置子集；没注入就用内置子集。
    // - `injected`：只用注入的引擎；没注入 ⇒ `RejectInjected`（每个表达式 fail loud）。
    // 外面统一套一层计数代理：三次失败点（决策期 `disabled`、决策期 `config` 预检、
    // 链上真求值）都走同一个数字出口。
    let interp_failures = Arc::new(AtomicU64::new(0));
    let injected = config.js_evaluator.clone();
    let inner: Arc<dyn JsEvaluator> = match (config.interpolation, injected) {
        (InterpolationMode::Builtin, Some(evaluator))
        | (InterpolationMode::Injected, Some(evaluator)) => evaluator,
        (InterpolationMode::Builtin, None) => Arc::new(crate::js::BuiltinJs),
        (InterpolationMode::Injected, None) => Arc::new(crate::js::RejectInjected),
    };
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(CountedEvaluator {
        inner,
        failures: interp_failures.clone(),
    });
    let interpolation_mode = config.interpolation;
    let state = Arc::new(LoaderState::new(
        ctx.clone(),
        entries,
        main_entries,
        main_startup,
        row_startup,
        main_baseline_valid,
        ConfigFiles {
            main: main_path,
            patches,
        },
        search,
        settle,
        source,
        evaluator,
        interp_failures,
        interpolation_mode,
        owners,
        text,
        config.dispatch_policy,
        config.host_limits,
        config.dependency_plan.clone(),
        config.strict,
        config.strict_sidecars,
    ));
    if !initial_persisted_errors.is_empty() {
        *state.persisted_errors.lock().unwrap() = initial_persisted_errors;
    }
    // 初装预检已经跑过：第一次 reconcile 不重复访问 source。
    state.skip_next_preflight.store(true, Ordering::SeqCst);
    // O7：首轮预检的分类结论在 state 建好之后、出版报告之前落位。
    state.record_preflight(preflight, preflight_provides);

    // 4) 绑定 per-App PluginSource（确保本 App 内插件调用 ctx_use 始终走同一来源与策略，
    //    不被同进程其他 App 的全局源绕过），并把 `!!js` 插值挂上 `internal/config` 链（M4-4b）——
    //    两者都**必须在第一次对账之前**完成。
    crate::set_plugin_source_for(&ctx, state.source.clone());
    attach_interpolation(&ctx, state.clone());

    // 5) 挂载 = **对账一次**：初次装载与热重载/运行期启停走同一条码路，不存在两套语义。
    //    `!!js` 的处置也归这里：`disabled` 与 `config` 两个位置都**现算**（内置子集或
    //    宿主注入的引擎）；算不出来的行不挂，并逐行报出（fail loud）。
    //    静默把表达式当默认值用，等于让配置悄悄撒谎——这是 M1-4 起的红线。
    let outcome = state.sync().await;
    let failures: Vec<String> = outcome.failures;

    // 6) 三条链路挂在同一份状态上：配置回写、自我卸载识别、配置/插件文件监听。
    attach_commit_writer(&ctx, state.clone());
    attach_self_dispose_observer(&ctx, state.clone());
    if let Some(hmr) = &config.hmr {
        attach_watcher(&ctx, state.clone(), hmr.interval_ms);
    }

    // 7) 暴露装载结果，便于上层做启动审计（对应 harness 的 `auditStartupEntries`）。
    //    报告的形状与刷新纪律见 [`LoaderState::report`]（M4-6）：装载时出版一次，
    //    之后**每轮对账**都用 `ctx.set` 刷新——读它的人永远看到"现在的样子"。
    ctx.provide("loader.report", Dyn::new(ExternValue(state.report())))?;
    state.mark_report_published();

    // fail loud：阻断性失败（含必需行失败，以及被独立处置策略判为 Fail 的来源/完整性/持久化错误）
    // 必须让装载失败；已降级的可选行或策略允许降级的错误只写警告并留在 `loader.report` 中。
    let blocking: Vec<FailedRow> = state
        .failed_rows()
        .into_iter()
        .filter(|row| !row.degraded)
        .collect();
    if !blocking.is_empty() {
        let lines: Vec<String> = blocking
            .iter()
            .map(|row| {
                format!(
                    "{} [category={}, startup={}]: {}",
                    row.label,
                    row.category.as_str(),
                    row.startup.as_str(),
                    row.message
                )
            })
            .collect();
        return Err(format!(
            "{} 个条目装载失败:\n  - {}",
            lines.len(),
            lines.join("\n  - ")
        )
        .into());
    }
    if !failures.is_empty() {
        if config.strict {
            return Err(format!(
                "{} 个条目装载失败（strict 模式；把 strict 设为 false 可降级为警告）:\n  - {}",
                failures.len(),
                failures.join("\n  - ")
            )
            .into());
        }
        for failure in &failures {
            ctx.log(Level::Warn, &format!("降级（strict=false）：{failure}"));
        }
    }
    for row in state.degraded_rows() {
        ctx.log(
            Level::Warn,
            &format!(
                "降级（category={}, startup={}）：{}: {}",
                row.category.as_str(),
                row.startup.as_str(),
                row.label,
                row.message
            ),
        );
    }

    // 声明级依赖环（M5）：**默认只报告**（环里的行照旧挂上，自然停在 PENDING）；
    // `strict_cycles` 才是本仓扩展——把环升级成装载失败，错误里带**环路径**。
    // 注意这条与上面的"逐行失败"是两件事：行都挂上了，是**声明**互相咬住了。
    let cycles = state.cycle_paths();
    if config.strict_cycles && !cycles.is_empty() {
        let paths: Vec<String> = cycles.iter().map(|path| path.join(" → ")).collect();
        return Err(format!(
            "声明级依赖环（strictCycles 模式；把 strictCycles 设为 false 可降级为报告）:\n  - {}",
            paths.join("\n  - ")
        )
        .into());
    }
    for path in &cycles {
        ctx.log(
            Level::Warn,
            &format!("声明级依赖环（只报告，不拒挂）：{}", path.join(" → ")),
        );
    }
    Ok(state)
}

/// 从行级 `inject` 声明里收集**必需依赖名**。
///
/// 可选依赖不参与声明级依赖环：它们不阻塞装载，也不该把一条软依赖变成
/// 装载期的硬环。形状解析与 [`crate::patch::parse_inject`] 共用一份，避免
/// `{required, optional}` 被当成两个服务名。
fn collect_inject_names(declared: &Config, out: &mut Vec<String>) {
    let Ok(parsed) = crate::patch::parse_inject(declared) else {
        return;
    };
    for (name, _) in parsed.required {
        out.push(name);
    }
}

/// 装载层的隔离域记账（M4-1）。
///
/// - **行私有域**（`isolate: {svc: true}`）：按**声明行的稳定 label** 分配一次并缓存，
///   跨 remount 稳定（上游把它挂在条目身份上，`isolate.ts:49-57` 的 LocalRealm）；
/// - **具名共享域**（`isolate: {svc: "<label>"}`）：委托 [`Store::named_realm`]
///   ——跨行共享、只增不减；
/// - realm 只存在这里与 `Mounted`，**绝不写进 `Entry`**：否则 `to_value` 会把内部编号
///   泄进 yml，`PartialEq` 也会被它触发成"配置变了"。
#[derive(Default)]
pub struct RowScopes {
    private: HashMap<(String, String), RealmId>,
}

impl RowScopes {
    /// 为一行的**祖先链（含自身，根在前）**派生挂载上下文：
    /// 从根往叶子逐层叠加 `isolate` 与 `intercept`；后代的同名条目覆盖祖先。
    pub fn scope_ctx(&mut self, parent: &Ctx, chain: &[(String, Entry)]) -> Ctx {
        let mut ctx = parent.clone();
        for (label, entry) in chain {
            if let Some(Config::Object(isolate)) = &entry.isolate {
                for (name, value) in isolate {
                    let realm = match value {
                        // `true` = 行私有域（按声明行缓存，跨 remount 稳定）。
                        Config::Bool(true) => self.private_realm(&ctx, label, name),
                        // 字符串 = 具名共享域（跨行共享，不回收）。
                        Config::String(shared) => ctx.store.named_realm(name, shared),
                        // 其余形态不是合法 isolate 声明（规格只有 `true` 与字符串标签）。
                        _ => continue,
                    };
                    ctx = ctx.isolate(name, Some(realm));
                }
            }
            if let Some(Config::Object(intercept)) = &entry.intercept {
                for (name, config) in intercept {
                    ctx = ctx.intercept(name, config.clone());
                }
            }
        }
        ctx
    }

    fn private_realm(&mut self, ctx: &Ctx, label: &str, name: &str) -> RealmId {
        let key = (label.to_string(), name.to_string());
        if let Some(realm) = self.private.get(&key) {
            return *realm;
        }
        // 用 `alloc_realm` 而不是 `named_realm`：私有域的键是**行身份**，
        // 且不应与具名域共享命名空间。
        let realm = ctx.store.alloc_realm();
        self.private.insert(key, realm);
        realm
    }
}

/// 递归插值：把配置树里的 `!!js` 标记换成**真实值**（上游 `config/utils.ts:12-22`）。
///
/// 两处调用（同一个函数，判据因此只有一套）：
/// - **决策期预检**（[`DesiredWalk::visit`]）：只为"能不能算"与"错误里带路径"，
///   结果不落地——真正的值由下一处给出；
/// - **`internal/config` 链**（[`attach_interpolation`]）：每行 fiber 解析配置时现算，
///   算出来的才是插件真正收到的值。于是"上下文变了，表达式就重新答一次"这条性质
///   与 `disabled` 位完全一致。
///
/// `path` 是**给人看的路径**（`a.config.b[2].k`）：配置里出错的地方和出错的原因一样重要。
pub(crate) fn interpolate_value(
    value: &Config,
    path: &str,
    ctx: &Ctx,
    evaluator: &dyn JsEvaluator,
) -> std::result::Result<Config, String> {
    if let Some(expr) = crate::patch::JsExpr::from_value(value) {
        let source = expr
            .expression()
            .map_err(|err| format!("{err} (at {path})"))?;
        return evaluator
            .evaluate(&source, ctx)
            .map_err(|err| format!("!!js 求值失败: {source} (at {path}): {err}"));
    }
    match value {
        Config::Array(list) => {
            let mut out = Vec::with_capacity(list.len());
            for (index, item) in list.iter().enumerate() {
                out.push(interpolate_value(
                    item,
                    &format!("{path}[{index}]"),
                    ctx,
                    evaluator,
                )?);
            }
            Ok(Config::Array(out))
        }
        Config::Object(object) => {
            let mut out = serde_json::Map::with_capacity(object.len());
            for (key, item) in object {
                out.insert(
                    key.clone(),
                    interpolate_value(item, &format!("{path}.{key}"), ctx, evaluator)?,
                );
            }
            Ok(Config::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// 这次配置解析是不是**我的一条配置行**（M4-4b：插值只对行生效）。
///
/// 上游的条件是 `this.entry && this.parent.fiber?.entry === this.entry`
/// （`loader/src/index.ts:104-113`）：行 fiber 才插值，行**里面**再挂出来的子插件
/// 保留调用方的配置身份（harness 的 `app-boot`/`cmdline` 就是靠这一条把自己的
/// 配置块原样传给子插件的）。
///
/// 本仓没有"挂在 fiber 上的 Entry"，用的是 ctx 的**结构**判据：
/// 从这条 fiber 的父 ctx 上溯，**跳过没有 fiber 的派生 ctx**（行 ctx 的隔离域/intercept
/// 链都是派生出来的、不带 fiber），第一个带 fiber 的祖先必须是装载器自己的 ctx。
///
/// 为什么不用"挂载时登记 fiber → label 表"：fiber 的首次配置解析跑在一个被 spawn 的
/// 任务里（`Store::plugin`），多线程运行时上它和 `mount_row` 的下一行没有先后关系——
/// 登记晚一步，插值就被跳过，配置里会**留下原始标记**。结构判据没有这个窗口：
/// 父链在 fiber 诞生的那一刻就是确定的。
fn mounted_on_loader(loader_ctx: &Ctx, fiber: &Arc<Fiber>) -> bool {
    let Some(loader_fiber) = loader_ctx.fiber_opt() else {
        return false;
    };
    let mut cursor = fiber.parent_ctx();
    while let Some(ctx) = cursor {
        if let Some(owner) = ctx.fiber_opt() {
            return Arc::ptr_eq(&owner, &loader_fiber);
        }
        cursor = ctx.parent.clone();
    }
    false
}

/// `internal/config` 链上的插值（M4-4b）：**每行 fiber 解析配置时现算**。
///
/// 顺序与上游一致（`loader/src/index.ts:104-113`）：先让内层（含 tail = raw）跑完，
/// 再对**结果**插值——这样别的 `internal/config` 监听者改过的值也在插值范围内。
///
/// 求值失败 ⇒ 返回 `None`：`Store::resolve_config` 把"瀑布被中断"当**错误**
/// （M4-4 同批收紧；原来会静默回落 raw，让原始标记流进插件）。失败细节（路径 + 原文）
/// 已经写进错误日志，并计入 `loader.report.interpolation.failed`。
fn attach_interpolation(ctx: &Ctx, state: Arc<LoaderState>) {
    let loader_ctx = ctx.clone();
    ctx.on_with(
        "internal/config",
        move |call: Call| {
            let state = state.clone();
            let loader_ctx = loader_ctx.clone();
            Box::pin(async move {
                let value = call.next.call(call.args.clone()).await;
                let Some(config) = value
                    .as_ref()
                    .and_then(|value| value.downcast::<Config>())
                    .map(|config| (*config).clone())
                else {
                    return value;
                };
                let Some(fiber) = call.args.first().and_then(|arg| arg.downcast::<Fiber>()) else {
                    return Some(Dyn::new(config));
                };
                if !mounted_on_loader(&loader_ctx, &fiber) {
                    return Some(Dyn::new(config));
                }
                // 路径用**行标**（已在装载表里时）；首挂那一刻记录还没建，退回"这一行的配置"。
                let root = state
                    .label_of_fiber(&fiber)
                    .map(|label| format!("{label}.config"))
                    .unwrap_or_else(|| "config".to_string());
                match interpolate_value(&config, &root, &call.ctx, state.evaluator.as_ref()) {
                    Ok(interpolated) => Some(Dyn::new(interpolated)),
                    Err(issue) => {
                        state
                            .ctx
                            .log(Level::Error, &format!("{}: {issue}", fiber.name()));
                        None
                    }
                }
            })
        },
        false,
        true,
    );
}

/// 配置回写：插件调用 `fiber.update(config)` 时把新配置落回文件。
///
/// 这条链路就是文档里"配置即稳定 ABI"的落点——用户改的是 **patch 层**，
/// 所以升级宿主时用户的改动天然存活。
///
/// M3-1：`internal/commit` **总是**发出，`no_save` 作为载荷到达——
/// 无论哪种情况都先推进内存记录（`mounted` 表 + 条目树），
/// 只有 `no_save == false` 才落盘（上游是干脆不发 commit，见附录 A）。
///
/// # 锁序（与 [`LoaderState::adopt`] 互斥的那一半）
/// `mounted`（先取先放）→ `config_gate` → `owners` / `tree` / `main_only` /
/// `main_baseline`。"改树 + 推进回写版本 + 写主树 + 落盘"整段在**同一个**
/// `config_gate` 临界区里：否则采纳的"校验通过 → 整份写树"会被一次并发回写覆盖，
/// 内存树退回旧内容而记录与磁盘是新值（watcher 指纹已对齐，不会补偿）。
fn attach_commit_writer(ctx: &Ctx, state: Arc<LoaderState>) {
    ctx.store
        .clone()
        .on_commit_async(ctx, move |fiber, config, no_save| {
            let state = state.clone();
            Box::pin(async move {
                let found = {
                    let table = state.rows.lock().unwrap();
                    table.values().find_map(|slot| match slot {
                        RowSlot::Mounted(record)
                            if record
                                .fiber
                                .upgrade()
                                .is_some_and(|f| Arc::ptr_eq(&f, &fiber)) =>
                        {
                            Some((
                                record.label.clone(),
                                record.origin,
                                record.declared.name.clone(),
                            ))
                        }
                        _ => None,
                    })
                };
                let Some((label, origin, plugin_name)) = found else {
                    return;
                };
                // 测试报到点：这一行确实有一次提交进了观察者，**还没**取 `config_gate`。
                #[cfg(test)]
                state.hooks.pass_commit_entered();

                // 阻塞磁盘 I/O（`ConfigLock::acquire` 自旋等待 + `std::fs` + 原子替换）
                // 移交 `spawn_blocking`，避免堵死 Tokio async worker 线程。
                // `record.config`、内存树、回写版本与落盘（含 overlay）全部落在同一个
                // `config_gate` 临界区内，并二次核验当前挂载的 fiber 仍是本次提交者，
                // 避免被 HMR/replace 顶掉的陈旧 fiber 写坏新实例记录或覆盖磁盘。
                let write_job = {
                    let state = state.clone();
                    let label = label.clone();
                    let config = config.clone();
                    let fiber = fiber.clone();
                    move || -> Option<std::result::Result<(), String>> {
                        let _gate = state.config_gate.lock().unwrap();
                        {
                            let mut rows = state.rows.lock().unwrap();
                            let Some(RowSlot::Mounted(record)) = rows.get_mut(&label) else {
                                return None;
                            };
                            if !record
                                .fiber
                                .upgrade()
                                .is_some_and(|f| Arc::ptr_eq(&f, &fiber))
                            {
                                return None;
                            }
                            record.config = config.clone();
                        }
                        let tree_entry = {
                            let mut tree = state.tree.lock().unwrap();
                            match find_by_label(&mut tree, &label) {
                                Some(entry) => {
                                    entry.config = Some(config.clone());
                                    Some(entry.clone())
                                }
                                None => None,
                            }
                        };
                        // 记录与树都被这次提交改了：在飞/排队的旧候选基于旧树，必须失效
                        // （no_save 的纯内存提交同样要算——它照样改了已采纳目标）。
                        state.bump_writeback();
                        if no_save {
                            // 只推进记录，不落盘（文件驱动的更新是这条路：文件已经是真相）。
                            return None;
                        }
                        let tree_entry = tree_entry?;
                        // 字段级归属优先：主配置行若被 patch 覆盖 config，必须写回那一层，
                        // 否则下一次 reload 会被 patch 再覆盖回去。
                        let owner_layer = tree_entry
                            .id
                            .as_deref()
                            .and_then(|id| state.owners.lock().unwrap().config_layer(id));
                        let res = match owner_layer {
                            Some(layer) => state.persist_overlay(
                                layer,
                                &label,
                                &tree_entry,
                                Some(&config),
                                None,
                            ),
                            None => match origin {
                                RowOrigin::Main => {
                                    // `main_only` 的读改写 + 落盘落在上面取的同一个 `config_gate` 里
                                    // （`persist` 要求调用方已持闸门，这里**不再**重复取锁）。
                                    let found = state.with_main_only(|main| {
                                        match find_by_label(main, &label) {
                                            Some(entry) => {
                                                entry.config = Some(config.clone());
                                                true
                                            }
                                            None => false,
                                        }
                                    });
                                    if found {
                                        state.persist()
                                    } else {
                                        state.persist_overlay_last(
                                            &label,
                                            &tree_entry,
                                            Some(&config),
                                            None,
                                        )
                                    }
                                }
                                RowOrigin::Patch => state.persist_overlay_last(
                                    &label,
                                    &tree_entry,
                                    Some(&config),
                                    None,
                                ),
                            },
                        };
                        Some(res)
                    }
                };

                let result = if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    match handle.spawn_blocking(write_job).await {
                        Ok(res) => res,
                        Err(err) => Some(Err(format!("配置回写后台线程异常: {err}"))),
                    }
                } else {
                    write_job()
                };

                if let Some(result) = result {
                    match result {
                        Ok(()) => state.clear_persisted_error(&label),
                        Err(err) => state.record_persisted_error(
                            &label,
                            &plugin_name,
                            format!("配置回写失败: {err}"),
                        ),
                    }
                }
            })
        });
}

/// 观察 fiber 的诞生与处置：识别"插件把自己卸载了"（M4-3 规格 7）。
///
/// # 判定链（每一环都在挡一类误报）
/// 1. `!fiber.is_disposed()` ⇒ 诞生事件，不看；
/// 2. 装载器自己的 fiber 已处置 ⇒ 整棵树正在关，不看（否则每行都会被标 disabled）；
/// 3. 这个 fiber 必须是**某条已挂载配置行的根 fiber**（子插件、别人的 fiber 直接放过）；
/// 4. 这一行正在被装载器**有意**卸载（`disposing`：禁用/移除/换实例/HMR）⇒ 不看。
///
/// # 为什么必须"有意/无意"分得这么细
/// 误判的代价是不对称的：漏判 ⇒ 用户手动卸载的插件重启后又活了（可容忍）；
/// 误判 ⇒ **一次正常的配置更新把插件永久标成 disabled 并写进文件**（灾难，且自我强化：
/// 下次读取时它还是禁用的）。
fn attach_self_dispose_observer(ctx: &Ctx, state: Arc<LoaderState>) {
    let cleanup_state = state.clone();
    let _ = ctx.effect(move |_| {
        let cleanup_state = cleanup_state.clone();
        Disposable::sync(move || {
            cleanup_state
                .pending_self_disposes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            cleanup_state
                .rows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, slot| !matches!(slot, RowSlot::SelfDisposing { .. }));
        })
    });
    let loader_ctx = ctx.clone();
    let sync_state = state.clone();
    ctx.store.clone().on_plugin(ctx, move |fiber| {
        if !fiber.is_disposed() {
            return;
        }
        if loader_ctx
            .fiber_opt()
            .is_none_or(|fiber| fiber.is_disposed())
        {
            return;
        }
        // 在单把 `rows` 锁内原子判定并从 `Mounted` 跃迁到 `SelfDisposing`：
        // 若该行已处于 `Disposing`（有意卸载/换实例/HMR）则直接忽略。
        let _ = sync_state.try_claim_self_dispose(fiber);
    });
    ctx.store
        .clone()
        .on_plugin_dispose_async(ctx, move |fiber| {
            let state = state.clone();
            Box::pin(async move {
                let pending = {
                    let mut list = state.pending_self_disposes.lock().unwrap();
                    list.iter()
                        .position(|(f, _, _, _)| Arc::ptr_eq(f, &fiber))
                        .map(|idx| {
                            let (_, label, record, ticket) = list.swap_remove(idx);
                            (label, record, ticket)
                        })
                };
                let Some((label, record, ticket)) = pending else {
                    return;
                };
                let write_state = state.clone();
                let write_label = label.clone();
                let write_job = move || {
                    write_state.persist_self_disposed(&write_label, record, ticket);
                };
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    if let Err(err) = handle.spawn_blocking(write_job).await {
                        state.clear_slot_if_ticket(&label, ticket);
                        state.record_persisted_error(
                            &label,
                            "",
                            format!("插件自我卸载后台回写线程异常: {err}"),
                        );
                    }
                } else {
                    write_job();
                }
            })
        });
}

/// 热重载监听：一轮里看两件事——**配置文件**变了没有、**插件文件**变了没有。
///
/// 配置这一侧：指纹变了才去稳读（`reload` 自己会再读两遍）→ 解析失败**保留上一快照**
/// （编辑中的半截文件是常态，只有警告）→ 成功则请求一次对账（`request_reconcile`
/// 自带合并与重入保护，所以这里可能会 Coalesced）。
///
/// 插件这一侧：比对每个已挂载行的 `source` 指纹 → 变了就"换名重挂"（M1-2 的事务语义）。
///
/// 用 `Weak<Context>` 而不是 `Arc<Context>`，卸载后任务会在下一轮自然退出，
/// 不会把整棵上下文树吊住（内核靠清空 `Fiber.ctx` 打破引用环，外部持强引用会抵消它）。
fn attach_watcher(ctx: &Ctx, state: Arc<LoaderState>, interval_ms: u64) {
    ctx.effect(move |ctx| {
        let weak = ctx.downgrade();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let state = state.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(interval_ms.max(50))).await;
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Some(ctx) = weak.upgrade() else { break };
                    if ctx.fiber_opt().is_none_or(|fiber| fiber.is_disposed()) {
                        break;
                    }

                    // ① 配置文件变了？
                    let current = config_fingerprint(&state.files);
                    let changed = current != *state.config_fingerprint.lock().unwrap();
                    if changed {
                        match state.reload().await {
                            // 文件正在被写：**不推进指纹**，下一轮再看同一份文件。
                            ReloadOutcome::Unsettled => {}
                            ReloadOutcome::Failed(err) => {
                                // 解析失败保留上一快照；推进指纹避免每轮刷同一条警告
                                // （文件不变就不会再解析，文件一变指纹就又会不同）。
                                *state.config_fingerprint.lock().unwrap() = current;
                                ctx.log(Level::Warn, &format!("配置文件未采纳（保留上一快照）: {err}"));
                            }
                            ReloadOutcome::Coalesced => {
                                *state.config_fingerprint.lock().unwrap() = current;
                            }
                            ReloadOutcome::Reconciled(outcome) => {
                                *state.config_fingerprint.lock().unwrap() = current;
                                if !outcome.is_noop() {
                                    ctx.log(
                                        Level::Info,
                                        &format!(
                                            "配置对账：应用 {} 项（挂 {} 卸 {} 换 {} 更新 {}）跳过 {} 滞后 {} 失败 {}",
                                            outcome.applied(),
                                            outcome.mounted.len(),
                                            outcome.disposed.len(),
                                            outcome.replaced.len(),
                                            outcome.updated.len(),
                                            outcome.skipped.len(),
                                            outcome.lagging.len(),
                                            outcome.failures.len(),
                                        ),
                                    );
                                }
                            }
                        }
                    }

                    // ② 插件文件变了？
                    let stale = state.stale_labels();
                    for label in stale {
                        if let Err(err) = reload_one(&ctx, &state, &label).await {
                            ctx.log(Level::Error, &format!("热重载 {label} 失败: {err}"));
                        }
                    }
                }
            });
        }
        Disposable::sync(move || stop.store(true, Ordering::SeqCst))
    });
}

/// 一次单行热重载的收敛状态分类。
///
/// 明确区分「新实例已真正 Active」与「新实例已挂载但必需依赖未就绪（Pending）」，
/// 避免把未满足依赖的 `Pending` 静默上报成“已激活上线”。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReloadStatus {
    /// 新实例已挂载并收敛到 `FiberState::Active`。
    Active,
    /// 新实例已挂载，但必需依赖尚未齐备（`FiberState::Pending`），待依赖就绪后自动激活。
    Pending,
    /// 新实例装载失败，已回滚到旧实例。
    RolledBack,
    /// 新旧实例均不可用，或旧实例卸载超时导致无法安全换入新实例（条目已从装载表移除）。
    Lost,
    /// 预检 / 来源准入 / 暂存 / 打开 / 探活失败，未触碰当前运行中的实例。
    Rejected,
    /// 目标行标当前不在装载表中（无需重载）。
    NotMounted,
}

impl ReloadStatus {
    /// 稳定的机器可读状态名（用于 `loader/reload` 事件与 `loader.report["lastReload"]`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::RolledBack => "rolled_back",
            Self::Lost => "lost",
            Self::Rejected => "rejected",
            Self::NotMounted => "not_mounted",
        }
    }
}

/// 一次单行热重载的结构化报告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReloadReport {
    /// 被重载的行标。
    pub label: String,
    /// 重载收敛结果（显式区分 `Active` 与 `Pending`）。
    pub status: ReloadStatus,
    /// 停留在 `Pending` 时的未满足依赖诊断列表；`Active` 时为空。
    pub pending: Vec<PendingInject>,
    /// 卸载旧实例时的结构化关闭报告（汇总原生 disposer panic、T2 `OP_CLEANUP`/`OP_DISPOSE` 与超时）。
    pub dispose_report: Option<CloseReport>,
    /// 失败、回滚或拒绝时的可读错误消息。
    pub error: Option<String>,
}

impl ReloadReport {
    /// 转成稳定的 JSON 诊断载荷（供 `loader.report["lastReload"]` 与 `loader/reload` 使用）。
    pub fn to_json(&self) -> serde_json::Value {
        let dispose = self.dispose_report.as_ref().map(|report| {
            json!({
                "status": report.status.as_str(),
                "stillRunning": report.still_running,
                "elapsedMs": u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
                "errors": report.errors.iter().map(|err| json!({
                    "fiber": err.fiber,
                    "uid": err.uid,
                    "kind": err.kind.as_str(),
                    "message": err.message,
                })).collect::<Vec<_>>(),
            })
        });
        let pending: Vec<serde_json::Value> = self
            .pending
            .iter()
            .map(|item| {
                json!({
                    "key": item.key,
                    "reason": item.reason.as_str(),
                    "related": item.related,
                })
            })
            .collect();
        json!({
            "label": self.label,
            "status": self.status.as_str(),
            "pending": pending,
            "dispose": dispose,
            "error": self.error,
        })
    }
}

/// 一次"换名重挂"的结果（M1-2）。
///
/// 三态让"非法状态不可表达"：回滚成功与"新旧都失败"必须被调用方区分对待；
/// `Ok` 内部进一步携带 `status`（`Active` vs `Pending`）与 `pending` 诊断，
/// 以及旧实例卸载时的结构化 `dispose_report`。
enum SwapOutcome {
    /// 新实例已挂载（`status` 区分 `Active` 与 `Pending`）。
    Ok {
        fiber: Arc<Fiber>,
        plugin: Arc<dyn Plugin>,
        status: ReloadStatus,
        pending: Vec<PendingInject>,
        dispose_report: Option<CloseReport>,
    },
    /// 新版本 apply 失败，已回滚到旧实例（`restored` 即当前在服务的 fiber）。
    RolledBack {
        error: String,
        restored: Arc<Fiber>,
        pending: Vec<PendingInject>,
        dispose_report: Option<CloseReport>,
    },
    /// 回滚也失败，或旧实例卸载超时未完成：该条目当前**没有**可用实例，需要人工介入。
    Lost {
        error: String,
        rollback_error: String,
        dispose_report: Option<CloseReport>,
        /// 若因清理超时仍未完成而放弃换入，携带正在后台排空的 fiber 供 `LoaderState::draining` 阻断旁路重挂。
        draining_fiber: Option<Arc<Fiber>>,
    },
}

impl SwapOutcome {
    /// 诊断用短名（日志与测试消息）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn describe(&self) -> &'static str {
        match self {
            SwapOutcome::Ok {
                status: ReloadStatus::Pending,
                ..
            } => "Ok(新实例已挂载但依赖未就绪/Pending)",
            SwapOutcome::Ok { .. } => "Ok(新实例已上线)",
            SwapOutcome::RolledBack { .. } => "RolledBack(已回滚到旧实例)",
            SwapOutcome::Lost { .. } => "Lost(新旧实例均不可用)",
        }
    }

    /// 需要向测试/日志报告的错误（成功时为 None）。
    #[cfg(test)]
    fn error_message(&self) -> Option<&str> {
        match self {
            SwapOutcome::Ok { .. } => None,
            SwapOutcome::RolledBack { error, .. } => Some(error),
            SwapOutcome::Lost { error, .. } => Some(error),
        }
    }

    fn to_report(&self, label: &str) -> ReloadReport {
        match self {
            SwapOutcome::Ok {
                status,
                pending,
                dispose_report,
                ..
            } => ReloadReport {
                label: label.to_string(),
                status: *status,
                pending: pending.clone(),
                dispose_report: dispose_report.clone(),
                error: None,
            },
            SwapOutcome::RolledBack {
                error,
                pending,
                dispose_report,
                ..
            } => ReloadReport {
                label: label.to_string(),
                status: ReloadStatus::RolledBack,
                pending: pending.clone(),
                dispose_report: dispose_report.clone(),
                error: Some(error.clone()),
            },
            SwapOutcome::Lost {
                error,
                dispose_report,
                ..
            } => ReloadReport {
                label: label.to_string(),
                status: ReloadStatus::Lost,
                pending: Vec::new(),
                dispose_report: dispose_report.clone(),
                error: Some(error.clone()),
            },
        }
    }
}

impl Mounted {
    /// 把一次换入的结果落进记录；返回 `true` 表示该条目应当从装载表移除（`Lost`）。
    ///
    /// 不变量（M1-2）：记录必须**始终指向当前在服务的实例**：
    /// - `Ok`：推进到新实例（`staged` / `plugin` / `fiber` 三件套）；
    /// - `RolledBack`：把 `fiber` 指回 `restored`——**这就是修复本身**：
    ///   否则记录仍指着已 dispose 的旧实例，下一次重载会与幽灵实例冲突（服务名撞车
    ///   ⇒ 新实例 provide 失败 ⇒ 回滚也失败）；
    /// - `Lost`：条目没有可用实例，调用方应把它移出装载表并告警。
    #[cfg(test)]
    fn commit_outcome(&mut self, outcome: SwapOutcome, staged: Option<PathBuf>) -> bool {
        let fp = self
            .file_source
            .then(|| fingerprint(&self.source))
            .flatten();
        self.commit_outcome_with_fp(outcome, staged, fp)
    }

    fn commit_outcome_with_fp(
        &mut self,
        outcome: SwapOutcome,
        staged: Option<PathBuf>,
        pre_stage_fp: Option<(u64, u64)>,
    ) -> bool {
        if self.file_source {
            if let Some(fp) = pre_stage_fp {
                self.fingerprint = Some(fp);
            }
        } else {
            self.fingerprint = None;
        }
        match outcome {
            SwapOutcome::Ok { fiber, plugin, .. } => {
                self.fiber = Arc::downgrade(&fiber);
                // M4-5：volatile 声明是**插件版本**的一部分——新插件可能声明了另一组路径。
                // 缓存不跟着换，下一次 diff 会拿旧路径去判"只差 volatile"，把一次
                // 需要重挂的更新误判成原地提交（或反过来）。
                self.volatile = volatile_of(plugin.volatile_paths());
                self.plugin = plugin;
                if let Some(staged) = staged {
                    self.staged = staged;
                }
                // 变体身份必须跟当前在服务的**暂存副本**一致；HMR 的换字节
                // 路径走这里而不是 mount_loaded，漏更新会让报告给出旧 hash。
                self.sha256 = staged_sha256(&self.staged, self.file_source);
                false
            }
            SwapOutcome::RolledBack { restored, .. } => {
                self.fiber = Arc::downgrade(&restored);
                // 回滚后线上的是**旧**插件：声明缓存必须跟着退回去。
                self.volatile = volatile_of(restored.volatile_paths());
                // staged 未变；重算一次，保持“sha256 == hash(staged)”不变量。
                self.sha256 = staged_sha256(&self.staged, self.file_source);
                false
            }
            SwapOutcome::Lost { .. } => true,
        }
    }
}

/// 已暂存副本的内容哈希（WP2）。非文件来源（没有指纹）返回 `None`，报告填
/// `"builtin"`；文件来源但读取失败返回 `"unavailable: ..."`，绝不伪装成 builtin。
fn staged_sha256(staged: &std::path::Path, is_file_source: bool) -> Option<String> {
    if !is_file_source {
        return None;
    }
    let hash = crate::source::sha256_file(staged)
        .or_else(|_| crate::source::sha256_file(staged))
        .unwrap_or_else(|_| "unavailable: read_failed".into());
    Some(hash)
}

/// 空声明 = 不参与 volatile（与挂载路径的口径一致）。
fn volatile_of(paths: Vec<String>) -> Option<Vec<String>> {
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// 就地把 `record` 指向的实例换成 `new_plugin`；**换失败则回滚到 `record.plugin`**。
///
/// 全程受调用方传入的 `tx_guard`（`RowTransactionGuard`）保护：无论在旧实例卸载、
/// 新实例 `settle`/`dispose`、还是回滚实例 `settle`/`dispose` 哪一个 `.await` 点被取消，
/// 守卫析构时都会自动把当前活跃 fiber 推进到 `RowSlot::Draining` 并后台完成清理。
#[cfg(test)]
async fn swap_entry(record: &Mounted, new_plugin: Arc<dyn Plugin>) -> SwapOutcome {
    swap_entry_guarded(record, new_plugin, None).await
}

async fn swap_entry_guarded(
    record: &Mounted,
    new_plugin: Arc<dyn Plugin>,
    mut tx_guard: Option<&mut RowTransactionGuard>,
) -> SwapOutcome {
    // 卸载旧实例（带结构化报告，并遵守 App 的 LifecyclePolicy 等待预算）。
    // 这一步无法与安装合并：同一隔离域里一个服务名只能有一个实现，
    // 旧实例还在时新实例的 `provide` 必然冲突。
    let old_fiber = tx_guard
        .as_deref()
        .and_then(RowTransactionGuard::active_fiber)
        .or_else(|| record.fiber.upgrade());
    let dispose_report = if let Some(fiber) = &old_fiber {
        Some(fiber.dispose_with_report(None).await)
    } else {
        None
    };
    if let Some(report) = &dispose_report {
        if report.still_running {
            let reason = format!(
                "旧实例卸载未在预算内完成（状态={}），后台清理仍在进行，拒绝挂载新实例",
                report.status.as_str()
            );
            return SwapOutcome::Lost {
                error: reason.clone(),
                rollback_error: reason,
                dispose_report,
                draining_fiber: old_fiber,
            };
        }
    }

    // 挂新实例。**用记录里的派生上下文**（M4-1）：行级 isolate/intercept 挂在它上面，
    // 用根 ctx 重挂等于把行悄悄搬回默认域。
    let scope = &record.scope;
    let fiber = mount_with_label(
        scope,
        new_plugin.clone(),
        record.config.clone(),
        record.inject.clone(),
        &record.label,
    );
    if let Some(guard) = tx_guard.as_deref_mut() {
        guard.set_active_fiber(Some(fiber.clone()));
    }
    fiber.settle().await;
    let Some(error) = fiber.error() else {
        let (status, pending) = if fiber.state() == FiberState::Active {
            (ReloadStatus::Active, Vec::new())
        } else {
            (ReloadStatus::Pending, fiber.pending_injects())
        };
        return SwapOutcome::Ok {
            fiber,
            plugin: new_plugin,
            status,
            pending,
            dispose_report,
        };
    };

    // 新实例失败：先把它拆干净——半途注册的副作用必须回收，
    // 否则它们会跟着回滚后的旧实例一起留在线上。
    // 与旧实例卸载及对账替换采用同一安全红线：只要仍有未完成的后台清理
    // （包括 T2 OP_APPLY 超时延后清理、半途 disposer 超时或受管任务未退出），
    // 一律拒绝在不安全状态上挂载回滚旧实例，并登记 `draining_fiber` 阻断旁路重挂。
    let new_dispose = fiber.dispose_with_report(None).await;
    let dispose_report = merge_close_reports(dispose_report, new_dispose.clone());
    if new_dispose.still_running {
        let rollback_error = format!(
            "失败新实例的清理未在预算内完成（状态={}），后台清理仍在进行，拒绝挂载回滚实例",
            new_dispose.status.as_str()
        );
        return SwapOutcome::Lost {
            error: format!("{error}；且无法安全回滚：{rollback_error}"),
            rollback_error,
            dispose_report,
            draining_fiber: Some(fiber),
        };
    }

    // 用旧插件把旧实例装回来。
    let restored = mount_with_label(
        scope,
        record.plugin.clone(),
        record.config.clone(),
        record.inject.clone(),
        &record.label,
    );
    if let Some(guard) = tx_guard {
        guard.set_active_fiber(Some(restored.clone()));
    }
    restored.settle().await;
    match restored.error() {
        // 回滚成功：报新版本的失败，并说明已经回到旧实例。
        None => {
            let pending = if restored.state() == FiberState::Active {
                Vec::new()
            } else {
                restored.pending_injects()
            };
            SwapOutcome::RolledBack {
                error: format!("{error}（已回滚到旧实例）"),
                restored,
                pending,
                dispose_report,
            }
        }
        // 回滚也失败：必须先按预算清理失败的回滚 fiber（避免泄漏在 runtime 表或漏登 draining），
        // 再把新旧两边的错误都报出来。此时线上**确实**是空的，静默才是最坏的结果。
        Some(rollback_error) => {
            let rollback_dispose = restored.dispose_with_report(None).await;
            let dispose_report = merge_close_reports(dispose_report, rollback_dispose.clone());
            let draining_fiber = rollback_dispose.still_running.then_some(restored);
            let extra = if rollback_dispose.still_running {
                format!(
                    "；且回滚实例清理未在预算内完成（状态={}，后台清理仍在进行）",
                    rollback_dispose.status.as_str()
                )
            } else {
                String::new()
            };
            SwapOutcome::Lost {
                error: format!(
                    "{error}；且回滚失败：{rollback_error}{extra}（该条目当前没有可用实例，需要人工介入）"
                ),
                rollback_error,
                dispose_report,
                draining_fiber,
            }
        }
    }
}

/// 把失败新实例的清理报告合并到旧实例的清理报告中（有错误或仍在后台运行时保留，绝不静默丢弃）。
fn merge_close_reports(old: Option<CloseReport>, failed_new: CloseReport) -> Option<CloseReport> {
    if failed_new.errors.is_empty() && !failed_new.still_running {
        return old;
    }
    match old {
        None => Some(failed_new),
        Some(mut base) => {
            base.errors.extend(failed_new.errors);
            base.still_running |= failed_new.still_running;
            base.elapsed += failed_new.elapsed;
            base.status = if base.still_running {
                CloseStatus::TimedOut
            } else if !base.errors.is_empty() {
                CloseStatus::CompletedWithErrors
            } else {
                CloseStatus::Completed
            };
            Some(base)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_reload_swap(
    ctx: &Ctx,
    state: &Arc<LoaderState>,
    label: &str,
    mut record: Mounted,
    new_plugin: Arc<dyn Plugin>,
    staged: Option<PathBuf>,
    pre_stage_fp: Option<(u64, u64)>,
    guard: tokio::sync::MutexGuard<'_, ()>,
) -> ReloadReport {
    // 在单把 `rows` 锁内原子校验当前槽位仍是 `Mounted` 且指向同一 `fiber` 并跃迁至 `Disposing`：
    // 若在预检/暂存/打开新库窗口内插件已经发生自我卸载（`SelfDisposing` 或已摘除），
    // 立刻丢弃新插件并返回 `NotMounted`，绝不复活已自卸载的行或与正在清理的旧实例并发冲突。
    let expected_fiber = record.fiber.upgrade();
    let Some(mut tx_guard) = state
        .claim_mounted_for_dispose(label, expected_fiber.as_ref())
        .into_claimed()
    else {
        drop(new_plugin);
        drop(guard);
        return ReloadReport {
            label: label.to_string(),
            status: ReloadStatus::NotMounted,
            pending: Vec::new(),
            dispose_report: None,
            error: None,
        };
    };
    let outcome = swap_entry_guarded(&record, new_plugin, Some(&mut tx_guard)).await;

    let report = outcome.to_report(label);
    let (lost_reason, draining_fiber) = match &outcome {
        SwapOutcome::Lost {
            rollback_error,
            draining_fiber,
            ..
        } => (Some(rollback_error.clone()), draining_fiber.clone()),
        _ => (None, None),
    };
    let old_plugin = record.plugin.clone();
    let old_declared = record.declared.clone();
    let old_isolate = record.scope.isolate.clone();
    let remove_entry = record.commit_outcome_with_fp(outcome, staged, pre_stage_fp);
    if remove_entry {
        if let Some(draining) = draining_fiber {
            tx_guard.finish_draining(draining);
        } else {
            tx_guard.finish_empty();
        }
        let provides = state.declared_provides_for(label, &old_declared, None, Some(&old_plugin));
        let startup = state.effective_row_startup(label, old_declared.id.as_deref());
        let error_msg = report
            .error
            .clone()
            .unwrap_or_else(|| lost_reason.clone().unwrap_or_else(|| "未知".to_string()));
        state.record_failed_row(
            FailedRow {
                label: label.to_string(),
                plugin: old_declared.name.clone(),
                category: FailureCategory::RuntimeUnavailable,
                startup,
                degraded: false,
                provides,
                message: error_msg,
            },
            Some(old_isolate),
        );
        ctx.log(
            Level::Error,
            &format!(
                "热重载 {label}: 新旧实例均不可用（回滚失败原因：{}），条目已从装载表移除，需要人工介入",
                lost_reason.as_deref().unwrap_or("未知")
            ),
        );
    } else {
        state.clear_failed_row(label);
        if matches!(report.status, ReloadStatus::Active | ReloadStatus::Pending) {
            clear_reload_failures(state, label);
        }
        tx_guard.commit_mounted(record.clone());
    }

    state.record_reload(report.clone());
    // `loader.report` 是实时诊断面：重载改了实例/记录/失败，报告必须跟上。
    // （`publish_report` 自己会挡住"装载器不在 ACTIVE"的收尾期时序。）
    state.publish_report();
    drop(guard);

    let report_json = report.to_json();
    // 广播。监听者用它做可视化、指标或"重载后重放状态"。
    ctx.emit(
        "loader/reload",
        vec![Dyn::new(ExternValue(json!({
            "label": label,
            "source": record.source.to_string_lossy(),
            "status": report.status.as_str(),
            "pending": report_json["pending"],
            "dispose": report_json["dispose"],
            "error": report.error,
        })))],
    )
    .await;

    let dispose_error_count = report
        .dispose_report
        .as_ref()
        .map(|dispose| dispose.errors.len())
        .unwrap_or(0);
    match report.status {
        ReloadStatus::Active if dispose_error_count == 0 => {
            ctx.log(Level::Info, &format!("热重载 {label}: 成功"));
        }
        ReloadStatus::Active => {
            ctx.log(
                Level::Warn,
                &format!("热重载 {label}: 成功（旧实例清理产生 {dispose_error_count} 条错误）"),
            );
        }
        ReloadStatus::Pending => {
            let keys: Vec<&str> = report
                .pending
                .iter()
                .map(|item| item.key.as_str())
                .collect();
            ctx.log(
                Level::Warn,
                &format!(
                    "热重载 {label}: 已挂载但依赖未就绪（Pending: {}）",
                    keys.join(", ")
                ),
            );
        }
        ReloadStatus::RolledBack | ReloadStatus::Lost => {
            ctx.log(
                Level::Error,
                &format!(
                    "热重载 {label}: {}",
                    report.error.as_deref().unwrap_or("失败")
                ),
            );
        }
        ReloadStatus::Rejected | ReloadStatus::NotMounted => {}
    }

    report
}

/// **手动**重挂一行并返回结构化 [`ReloadReport`]：预检新库（落新文件名 + 打开 + ABI 探活）
/// → 换名重挂 → 更新记录。
///
/// 与 [`reload_one`] 执行同一套受 `mutation` 临界区保护的流程，但直接返回 [`ReloadReport`]，
/// 显式区分 `Active` / `Pending` / `RolledBack` / `Lost` / `Rejected` / `NotMounted`
/// 并附带旧实例的 [`CloseReport`]。
pub async fn reload_one_report(ctx: &Ctx, state: &Arc<LoaderState>, label: &str) -> ReloadReport {
    let reject = |state: &Arc<LoaderState>, error: String| -> ReloadReport {
        let report = ReloadReport {
            label: label.to_string(),
            status: ReloadStatus::Rejected,
            pending: Vec::new(),
            dispose_report: None,
            error: Some(error),
        };
        state.record_reload(report.clone());
        report
    };

    // 预检 → 换实例 → 改记录 → 发布报告：整个变更序列与对账/启停互斥。
    let guard = state.mutation.lock().await;
    // 在预检 / 来源校验 / 暂存之前先快照磁盘指纹，避免热重载期间并发写入的新版本被
    // 重载结束时的事后指纹采样吞掉（HMR 丢失更新）。
    let pre_stage_fp = state.mounted_record_clone(label).and_then(|record| {
        record
            .file_source
            .then(|| fingerprint(&record.source))
            .flatten()
    });
    match state.preflight_dependencies(&state.tree_snapshot()) {
        Ok((report, provides)) => state.record_preflight(report, provides),
        Err(err) => {
            let failure = format!("依赖预检失败（reload 未执行）: {err}");
            advance_fingerprint_with(state, label, pre_stage_fp);
            record_reload_failure(state, label, &failure);
            let report = reject(state, failure.clone());
            state.publish_report();
            ctx.log(
                Level::Error,
                &format!("热重载 {label}: {failure}（保留当前实例）"),
            );
            drop(guard);
            return report;
        }
    }
    let Some(record) = state.mounted_record_clone(label) else {
        return ReloadReport {
            label: label.to_string(),
            status: ReloadStatus::NotMounted,
            pending: Vec::new(),
            dispose_report: None,
            error: None,
        };
    };

    let active_policy = match state.source.admission().resolve() {
        Ok(policy) => policy,
        Err(err) => {
            advance_fingerprint_with(state, label, pre_stage_fp);
            record_reload_failure(state, label, &err);
            let report = reject(state, err.clone());
            state.publish_report();
            ctx.log(
                Level::Error,
                &format!("热重载 {label}: 来源准入被拒绝（保留当前实例）: {err}"),
            );
            return report;
        }
    };
    if let Some(policy) = &active_policy {
        if let Err(err) = policy.verify(&record.source) {
            advance_fingerprint_with(state, label, pre_stage_fp);
            record_reload_failure(state, label, &err);
            let report = reject(state, err.clone());
            state.publish_report();
            ctx.log(
                Level::Error,
                &format!("热重载 {label}: 来源策略拒绝新库（保留当前实例）: {err}"),
            );
            return report;
        }
    }
    let staged = match stage_plugin(&record.source, state.next_revision()) {
        Ok(staged) => staged,
        Err(err) => {
            let msg = err.to_string();
            ctx.log(
                Level::Error,
                &format!("热重载 {label}: 暂存新库失败（保留当前实例，下轮重试）: {msg}"),
            );
            let report = reject(state, msg);
            state.publish_report();
            return report;
        }
    };
    #[cfg(test)]
    state.hooks.pass_before_staged_open(&staged);
    let plugin = match &active_policy {
        Some(policy) => LoadedPlugin::open_staged_verified(&record.source, &staged, policy),
        None => LoadedPlugin::open_staged(&staged),
    };
    let plugin = match plugin {
        Ok(plugin) => plugin,
        Err(err) => {
            drop(StagedFile::new(&staged));
            let msg = err.to_string();
            advance_fingerprint_with(state, label, pre_stage_fp);
            record_reload_failure(state, label, &msg);
            let report = reject(state, msg.clone());
            state.publish_report();
            ctx.log(
                Level::Error,
                &format!("热重载 {label}: 新库无法打开（保留当前实例）: {msg}"),
            );
            return report;
        }
    };
    if !plugin.ping() {
        drop(plugin);
        advance_fingerprint_with(state, label, pre_stage_fp);
        record_reload_failure(state, label, "ABI 探活失败（保留当前实例）");
        let report = reject(state, "ABI 探活失败".into());
        state.publish_report();
        ctx.log(
            Level::Error,
            &format!("热重载 {label}: ABI 探活失败（保留当前实例）"),
        );
        return report;
    }

    let new_plugin: Arc<dyn Plugin> = Arc::new(plugin);
    finish_reload_swap(
        ctx,
        state,
        label,
        record,
        new_plugin,
        Some(staged),
        pre_stage_fp,
        guard,
    )
    .await
}

/// **手动**重挂一行：预检新库（落新文件名 + 打开 + ABI 探活）→ 换名重挂 → 更新记录。
///
/// 兼容 0.1 返回值约定（预检/准入/打开/探活失败返回 `Err`，换入流程完成后返回 `Ok(())`）；
/// 需要显式区分 `Active` 与 `Pending` 或读取旧实例清理错误时，请调用 [`reload_one_report`]
/// 或在重载后读取 [`LoaderState::last_reload`]。
pub async fn reload_one(
    ctx: &Ctx,
    state: &Arc<LoaderState>,
    label: &str,
) -> std::result::Result<(), String> {
    let report = reload_one_report(ctx, state, label).await;
    if report.status == ReloadStatus::Rejected {
        Err(report.error.unwrap_or_else(|| "热重载被拒绝".into()))
    } else {
        Ok(())
    }
}

/// 使用已准备好的 [`Plugin`] 实例热替换指定行，并返回结构化 [`ReloadReport`]。
///
/// 适用于内置/自定义 `PluginSource` 或内存内载体（不经磁盘暂存与动态库重开），
/// 与 [`reload_one_report`] 共享同一套 `mutation` 互斥、旧实例结构化关闭、回滚与报告发布路径。
pub async fn reload_one_with_plugin(
    ctx: &Ctx,
    state: &Arc<LoaderState>,
    label: &str,
    new_plugin: Arc<dyn Plugin>,
) -> ReloadReport {
    let guard = state.mutation.lock().await;
    let Some(record) = state.mounted_record_clone(label) else {
        return ReloadReport {
            label: label.to_string(),
            status: ReloadStatus::NotMounted,
            pending: Vec::new(),
            dispose_report: None,
            error: None,
        };
    };
    finish_reload_swap(ctx, state, label, record, new_plugin, None, None, guard).await
}

/// 预检失败时也要把指纹推进到装载表：不然 HMR 每 500ms 重试同一个坏库（重试风暴）。
///
/// 若源文件此刻短暂缺失或不可读（`fingerprint` 返回 `None`），必须保留原 `Some(...)` 指纹，
/// 绝不写成 `None`——`stale_labels` 会跳过 `fingerprint == None` 的行，写成 `None` 会让该行永久退出 HMR。
#[cfg(test)]
fn advance_fingerprint(state: &Arc<LoaderState>, label: &str) {
    let fp = state
        .mounted_record_clone(label)
        .and_then(|record| fingerprint(&record.source));
    advance_fingerprint_with(state, label, fp);
}

fn advance_fingerprint_with(
    state: &Arc<LoaderState>,
    label: &str,
    pre_stage_fp: Option<(u64, u64)>,
) {
    if let Some(fp) = pre_stage_fp {
        state.with_mounted_mut(label, |record| {
            if record.fingerprint.is_some() {
                record.fingerprint = Some(fp);
            }
        });
    }
}

/// 把一次**没有执行**的热重载失败并进对账产物，让 `loader.report.failed` 看得见它。
///
/// 只追加 `failures`，**不动**上一轮真正执行过的对账计数（`report.reconcile`
/// 的 `applied`/`skipped`/`lagging` 描述的是"最近一次对账"，不该被一次未执行的重载
/// 抹掉）。同一条失败重复出现只记一次：HMR 会按文件变化反复尝试，日志里已经够多了。
fn record_reload_failure(state: &Arc<LoaderState>, label: &str, failure: &str) {
    let mut last = state.last.lock().unwrap();
    let outcome = last.get_or_insert_with(ReconcileOutcome::default);
    let line = format!("{label}: {failure}");
    if !outcome.failures.contains(&line) {
        outcome.failures.push(line);
    }
}

/// 热重载成功恢复到 `Active` / `Pending` 后，清理该行残留的陈旧失败条目。
fn clear_reload_failures(state: &Arc<LoaderState>, label: &str) {
    let prefix = format!("{label}: ");
    if let Some(last) = state.last.lock().unwrap().as_mut() {
        last.failures.retain(|line| !line.starts_with(&prefix));
    }
}

fn find_by_label<'a>(entries: &'a mut [Entry], label: &str) -> Option<&'a mut Entry> {
    let mut ordinals = HashMap::new();
    find_by_label_into(entries, label, &mut ordinals)
}

/// 与 [`desired_rows`] **同一走法**的查找：同一个计数器语义、同样的遍历规则
/// （见 [`stable_label`]）。两处若漂移，回写就会落到错误的行上。
fn find_by_label_into<'a>(
    entries: &'a mut [Entry],
    label: &str,
    ordinals: &mut HashMap<String, usize>,
) -> Option<&'a mut Entry> {
    for entry in entries.iter_mut() {
        if stable_label(entry, ordinals) == label {
            return Some(entry);
        }
        if let Some(found) = find_by_label_into(&mut entry.children, label, ordinals) {
            return Some(found);
        }
    }
    None
}

/// 收集指定行（及其全部后代，当目标为组行时）的结构行标集合，与 `stable_label` 同序。
fn collect_subtree_labels(entries: &[Entry], target: &str) -> HashSet<String> {
    fn visit(
        entries: &[Entry],
        target: &str,
        in_subtree: bool,
        ordinals: &mut HashMap<String, usize>,
        out: &mut HashSet<String>,
    ) {
        for entry in entries {
            let label = stable_label(entry, ordinals);
            let matched = in_subtree || label == target;
            if matched {
                out.insert(label);
            }
            visit(&entry.children, target, matched, ordinals, out);
        }
    }
    let mut ordinals = HashMap::new();
    let mut out = HashSet::new();
    visit(entries, target, false, &mut ordinals, &mut out);
    out
}

/// 现在**应当**挂着的一行：叶子条目 + 稳定行标 + 派生挂载上下文 + 上下文输入。
///
/// group 行自己不挂载（只是容器），所以这里只有叶子行。
#[derive(Clone)]
struct FlatRow {
    label: String,
    entry: Entry,
    /// 这一行的派生上下文（M4-1）：隔离域与 intercept 都已按祖先链叠加。
    scope: Ctx,
    /// 派生上下文的自变量（M4-3 的 diff 用它判断"上下文变没变"）。
    scope_inputs: Vec<ScopeInput>,
}

/// 派生上下文的自变量：链上每层的 label + `isolate`/`intercept` 声明。
fn scope_inputs(chain: &[(String, Entry)]) -> Vec<ScopeInput> {
    chain
        .iter()
        .map(|(label, entry)| ScopeInput {
            label: label.clone(),
            isolate: entry.isolate.clone(),
            intercept: entry.intercept.clone(),
        })
        .collect()
}

/// 期望态计算阶段发现的一条行级决策问题类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesiredIssueKind {
    DuplicateLabel,
    Interpolation,
}

/// 期望态计算阶段发现的一条行级决策问题。
struct DesiredIssue {
    label: String,
    entry: Entry,
    kind: DesiredIssueKind,
    isolate: Option<IsolateScope>,
    issue: String,
}

impl std::fmt::Debug for DesiredIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesiredIssue")
            .field("label", &self.label)
            .field("entry", &self.entry)
            .field("kind", &self.kind)
            .field("has_isolate", &self.isolate.is_some())
            .field("issue", &self.issue)
            .finish()
    }
}

/// 为决策问题分配稳定的 `failed_rows` 台账键（重复 label 使用独立键避免被首行 Mount 误清）。
fn assign_issue_keys(issues: Vec<DesiredIssue>) -> Vec<(String, DesiredIssue)> {
    let mut dup_counts: HashMap<String, usize> = HashMap::new();
    issues
        .into_iter()
        .map(|issue| {
            let key = match issue.kind {
                DesiredIssueKind::Interpolation => issue.label.clone(),
                DesiredIssueKind::DuplicateLabel => {
                    let count = dup_counts.entry(issue.label.clone()).or_insert(0);
                    *count += 1;
                    if *count == 1 {
                        format!("{}#duplicate", issue.label)
                    } else {
                        format!("{}#duplicate#{}", issue.label, *count)
                    }
                }
            };
            (key, issue)
        })
        .collect()
}

/// 期望态计算的产物（M4-6 起把"休眠"也记下来：报告要回答"为什么没生效"）。
struct Desired {
    rows: Vec<FlatRow>,
    /// 逐行的问题：不是"禁用"，而是"我们**无法判断**该不该挂"（保留行声明以支持启动策略与上游关联）。
    issues: Vec<DesiredIssue>,
    /// 当前**休眠**的叶子行标（自身或祖先的 `disabled` 求值为真）。组行不是"能挂的行"，不计。
    disabled: Vec<String>,
}

/// 现在**应当**挂着的一组行——也就是"期望态"（M4-2；M4-3 的 `plan` 拿它做差异）。
///
/// 三条纪律：
/// 1. **整棵树都要走到**：`stable_label` 的序号是全局的，跳过（无论因为 disabled 还是
///    表达式）都会让编号与 [`find_by_label`] 漂移，回写就落到错误的行上；
/// 2. **label 只由结构决定**（P2-15）：`!!js`、`disabled` 都不参与编号；
/// 3. **disabled 现算**：每个叶子行用**自己的**派生 ctx 求值祖先链上的表达式
///    （上游 `entry.disabled` 的 getter 同理），于是上下文一变，下一次决策就变。
fn desired_rows(
    entries: &[Entry],
    decision: &RowDecision<'_>,
    parent: &Ctx,
    scopes: &mut RowScopes,
) -> Desired {
    let mut walk = DesiredWalk {
        decision,
        parent,
        scopes,
        ordinals: HashMap::new(),
        seen: HashSet::new(),
        rows: Vec::new(),
        issues: Vec::new(),
        disabled: Vec::new(),
    };
    walk.visit(entries, &[]);
    Desired {
        rows: walk.rows,
        issues: walk.issues,
        disabled: walk.disabled,
    }
}

struct DesiredWalk<'a, 'b> {
    decision: &'a RowDecision<'b>,
    parent: &'a Ctx,
    scopes: &'a mut RowScopes,
    ordinals: HashMap<String, usize>,
    /// 已经出现过的行标：重复 `id` 会撞名（M4-3 之前会静默覆盖装载表记录）。
    seen: HashSet<String>,
    rows: Vec<FlatRow>,
    issues: Vec<DesiredIssue>,
    disabled: Vec<String>,
}

impl DesiredWalk<'_, '_> {
    fn visit(&mut self, entries: &[Entry], ancestors: &[(String, Entry)]) {
        for entry in entries {
            let label = stable_label(entry, &mut self.ordinals);
            // 重复 `id`（或 `id` 撞上生成的行标）必须**报出来**：装载表是 label → 记录，
            // 静默覆盖会让其中一行的实例变成无人认领的幽灵。重复出现的行**不挂载**，
            // 语义是"第一行获胜"，且问题进 `failures`/日志（strict 下整体失败）。
            if !self.seen.insert(label.clone()) {
                self.issues.push(DesiredIssue {
                    label,
                    entry: entry.clone(),
                    kind: DesiredIssueKind::DuplicateLabel,
                    isolate: None,
                    issue: "重复 label（两行解析成同一个行标识；请给它们不同的 id），已跳过该行"
                        .to_string(),
                });
                continue;
            }
            let mut chain: Vec<(String, Entry)> = ancestors.to_vec();
            chain.push((label.clone(), entry.clone()));

            // 组行自己不挂载：它的 `disabled` 会通过链传到每个后代（这是"组禁用"
            // 的全部机制——不需要逐个子行去改）。
            if entry.group {
                self.visit(&entry.children, &chain);
                continue;
            }

            // 派生上下文**先派生再决策**：链上的 `!!js` 表达式读的就是这一行的上下文
            // （上游 `this.evaluate(...)` 同理），所以决策不能没有它。
            let scope = self.scopes.scope_ctx(self.parent, &chain);

            // 派生 disabled：自己或任一祖先为真 ⇒ 这一行不挂。这是**正常状态**，不报错——
            // 但 M4-6 起要**记下来**（报告里点名休眠行：用户问的往往是"我配了它，为什么没生效"）。
            match self.decision.disabled(&chain, &scope) {
                Ok(true) => {
                    self.disabled.push(label);
                    continue;
                }
                Ok(false) => {}
                // 算不出来（子集外 / 语法错）⇒ 不挂载并逐行报出（fail loud 的落点）。
                Err(issue) => {
                    self.issues.push(DesiredIssue {
                        label,
                        entry: entry.clone(),
                        kind: DesiredIssueKind::Interpolation,
                        isolate: Some(scope.isolate.clone()),
                        issue,
                    });
                    continue;
                }
            }

            // `config` 位置的 `!!js`：**先算一遍**（M4-4b 的决策期预检）。
            //
            // 真正的求值发生在每行 fiber 自己的 `internal/config` 解析里（值要跟着上下文
            // 走、每次解析现算），这里算的这一遍只为了两件事：
            // ① 错误带**配置路径**（链上只有"这一行的配置"这一层信息）；
            // ② 算不出来的行**不挂**——挂了就是把原始标记喂给插件（正是 M1-4 消除的静默错误）。
            if let Some(config) = &entry.config {
                let path = format!("{label}.config");
                if let Err(issue) =
                    interpolate_value(config, &path, &scope, self.decision.evaluator)
                {
                    self.issues.push(DesiredIssue {
                        label,
                        entry: entry.clone(),
                        kind: DesiredIssueKind::Interpolation,
                        isolate: Some(scope.isolate.clone()),
                        issue,
                    });
                    continue;
                }
            }
            self.rows.push(FlatRow {
                label,
                entry: entry.clone(),
                scope,
                scope_inputs: scope_inputs(&chain),
            });
        }
    }
}

/// 行的**稳定 label**：有 `id` 用它，否则 `name#<序号>`（同名行的出现序）。
///
/// 这是 P2-15 的修复：原来的 `id ?? name` 会让**同名无 id 的多行互相覆盖**，
/// 装载表、回写、对账（plan/diff/stale_labels）随之全错。
///
/// **label 只由结构决定**（M4-2 修订）：早先"含未求值 `!!js` 的行不消耗序号"会让编号
/// 依赖求值结果（表达式行、禁用组的子树都会把它改掉），而 [`find_by_label`] 的走法
/// 一旦与之漂移，回写就落到错误的行上。现在两处都是"整棵树都走、按出现序编号"。
fn stable_label(entry: &Entry, ordinals: &mut HashMap<String, usize>) -> String {
    if let Some(id) = &entry.id {
        return id.clone();
    }
    let counter = ordinals.entry(entry.name.clone()).or_insert(0);
    let label = format!("{}#{}", entry.name, *counter);
    *counter += 1;
    label
}

/// 与装载器同一套结构行标的一行（`id` 或 `name#N`）。
#[derive(Clone, Debug)]
pub struct LabeledEntry {
    pub label: String,
    pub entry: Entry,
}

/// 按装载器同一套走法生成叶子行标（组行本身不返回，但会消耗序号）。
///
/// `inspect` / `validate` 用它，避免“预览显示 `hello`、`status` 显示 `hello#0`”
/// 这种无法对应的漂移。重复 `id` 仍会得到相同 label；是否拒绝由解析/装载阶段裁决。
pub fn labeled_entries(entries: &[Entry]) -> Vec<LabeledEntry> {
    fn visit(
        entries: &[Entry],
        ordinals: &mut HashMap<String, usize>,
        out: &mut Vec<LabeledEntry>,
    ) {
        for entry in entries {
            let label = stable_label(entry, ordinals);
            if entry.group {
                visit(&entry.children, ordinals, out);
            } else {
                out.push(LabeledEntry {
                    label,
                    entry: entry.clone(),
                });
            }
        }
    }
    let mut ordinals = HashMap::new();
    let mut rows = Vec::new();
    visit(entries, &mut ordinals, &mut rows);
    rows
}

#[cfg(test)]
mod loader_adopt_tests;

#[cfg(test)]
mod source_admission_tests;

#[cfg(test)]
mod tests;
