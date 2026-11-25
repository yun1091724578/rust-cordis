//! 宿主派发看门狗（[`DispatchPolicy`]）、容量上限（[`HostLimits`]）、启动策略与方法白名单注册表（O11 拆分）。
//!
//! per-App 策略直接存放在 `ctx.store` 扩展槽中（随 `App`/`Store` 自动释放，无进程级裸指针表），
//! 缺失时回落到进程级默认值；实例在注册时取一份快照。

pub use cordis_core::LifecyclePolicy;
use cordis_core::{CallerIdentity, Ctx};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// 派发看门狗（M2，D3；P0-2 起按 App 配置）：宿主等待 `ctx_fulfill` 的上限。
///
/// `None` = 不设上限（默认；与 M2 之前的行为逐字相同）。每个实例在注册时从
/// **所属 App** 的策略注册表取一份快照（见 [`set_dispatch_policy_for`]）；
/// 没有 per-App 条目时回落到进程级的 [`set_dispatch_policy`]。
/// **不设上限时，行为不端的插件可以占住一次派发**——这正是风险 1，缓解手段是
/// 卸载排水与看门狗。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DispatchPolicy {
    /// 入站挂起续延与流式挂起的上限。
    pub timeout_ms: Option<u64>,
    /// 出站 `ctx_call_iface` 的上限（`None` = 不设上限，默认）。
    ///
    /// 这是看门狗而不是业务超时：默认关闭，宿主显式开启后才会把“提供者永远
    /// 不结算”变成一条可读的 `HOST_TIMEOUT` 错误。合法的长调用应保持 `None`
    /// 或设置足够大的值。
    pub outbound_timeout_ms: Option<u64>,
}

impl DispatchPolicy {
    /// 与 [`HostLimits::new`] 对应的最小构造入口；0.1 仍保留公开字段。
    pub fn new(timeout_ms: Option<u64>, outbound_timeout_ms: Option<u64>) -> Self {
        Self {
            timeout_ms,
            outbound_timeout_ms,
        }
    }
}

static DISPATCH_POLICY: RwLock<DispatchPolicy> = RwLock::new(DispatchPolicy {
    timeout_ms: None,
    outbound_timeout_ms: None,
});

/// 设置**进程级**派发看门狗（覆盖式，后设生效）。
///
/// 这是**回落**而不是唯一入口：用 [`set_dispatch_policy_for`] 给单个 App 设置
/// 策略后，该 App 的实例优先使用自己的策略；只有没有 per-App 条目（或条目随
/// App 释放被清掉）时才读这里。已有实例保留注册时的快照。
pub fn set_dispatch_policy(policy: DispatchPolicy) {
    *DISPATCH_POLICY.write().unwrap() = policy;
}

pub(crate) fn dispatch_policy() -> DispatchPolicy {
    *DISPATCH_POLICY.read().unwrap()
}

/// 设置**单个 App** 的派发看门狗。
///
/// App 由 `ctx.store` 标识：策略直接保存在 `ctx.store` 上，随 `App` 自动释放；
/// 同一个 `App` 上派生的所有 ctx 共享这条策略，不同 `App` 互不影响。
/// 装载器在 `LoaderConfig.dispatch_policy` 有值时会在挂载前调用它。
///
/// 只影响**之后注册**的实例（实例在注册时取快照）；已注册实例不会被追溯修改。
pub fn set_dispatch_policy_for(ctx: &Ctx, policy: DispatchPolicy) {
    ctx.store.set_extension(policy);
}

/// 内部读取：per-App 优先，缺失时回落到进程级策略。
pub(crate) fn dispatch_policy_for_ctx(ctx: &Ctx) -> DispatchPolicy {
    ctx.store
        .extension::<DispatchPolicy>()
        .unwrap_or_else(dispatch_policy)
}

/// 宿主容量/准入上限（P0-3）。
///
/// 这是**宿主侧表项准入**：限制每个 App 的实例数、每个实例的派发记录、挂起续延
/// 与流数量。超限时宿主拒绝这次准入，返回可读的结构化错误，**不执行插件代码**。
///
/// 它不是每插件的内存/CPU 硬上限——T2 插件与宿主同进程共享地址空间；真正的
/// 资源硬限制需要 cgroup/Job Object 或进程外载体（T3）。
///
/// 所有字段都必须大于 0。0 **不是**“无限”，而是非法配置：装载器 / 设置入口会
/// 拒绝它，避免把“忘记填”静默解释成“永不接纳”。缺省值有意放宽以保持既有行为：
/// 1024 / 1024 / 256 / 32。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostLimits {
    /// 单个 App 内同时注册的实例上限。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_instances_per_app: usize,
    /// 单个实例的在飞派发记录总上限（入站、iface、出站与流共用这张表）。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_dispatches_per_instance: usize,
    /// 单个实例同时挂起的入站续延上限。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_pending_per_instance: usize,
    /// 单个实例同时存在的流上限。
    #[serde(deserialize_with = "deserialize_nonzero_usize")]
    pub max_streams_per_instance: usize,
}

/// 手写 `Default`：派生 `Default` 会给 usize 全 0，而 0 是非法配置。
impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_instances_per_app: 1024,
            max_dispatches_per_instance: 1024,
            max_pending_per_instance: 256,
            max_streams_per_instance: 32,
        }
    }
}

impl HostLimits {
    /// 带校验的构造入口：嵌入方可在注册前发现 0/非法上限。
    /// 0.1 仍保留公开字段与字面量构造，这个入口只是更安全的路径。
    pub fn new(
        max_instances_per_app: usize,
        max_dispatches_per_instance: usize,
        max_pending_per_instance: usize,
        max_streams_per_instance: usize,
    ) -> std::result::Result<Self, String> {
        let limits = Self {
            max_instances_per_app,
            max_dispatches_per_instance,
            max_pending_per_instance,
            max_streams_per_instance,
        };
        limits.validate()?;
        Ok(limits)
    }

    fn validate(self) -> std::result::Result<(), String> {
        let fields = [
            ("maxInstancesPerApp", self.max_instances_per_app),
            ("maxDispatchesPerInstance", self.max_dispatches_per_instance),
            ("maxPendingPerInstance", self.max_pending_per_instance),
            ("maxStreamsPerInstance", self.max_streams_per_instance),
        ];
        for (name, value) in fields {
            if value == 0 {
                return Err(format!(
                    "HostLimits.{name} 不能为 0；0 不是“无限”，请给出正整数"
                ));
            }
        }
        Ok(())
    }
}

fn deserialize_nonzero_usize<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "HostLimits 的容量上限不能为 0（0 不是“无限”）",
        ));
    }
    Ok(value)
}

static DEFAULT_HOST_LIMITS: RwLock<HostLimits> = RwLock::new(HostLimits {
    max_instances_per_app: 1024,
    max_dispatches_per_instance: 1024,
    max_pending_per_instance: 256,
    max_streams_per_instance: 32,
});

/// 设置**进程级**容量准入上限（覆盖式，后设生效）。
///
/// 与 [`set_dispatch_policy`] 同一套回落语义：只有没有 per-App 条目（或条目随
/// App 释放被清掉）的 App 才读这里。传入 0 会返回错误且不改动当前值。
pub fn set_default_host_limits(limits: HostLimits) -> std::result::Result<(), String> {
    limits.validate()?;
    *DEFAULT_HOST_LIMITS.write().unwrap() = limits;
    Ok(())
}

pub(crate) fn default_host_limits() -> HostLimits {
    *DEFAULT_HOST_LIMITS.read().unwrap()
}

/// 设置**单个 App** 的容量准入上限。
///
/// App 由 `ctx.store` 标识：上限直接保存在 `ctx.store` 上，随 `App` 自动释放；
/// 同一个 `App` 上派生的所有 ctx 共享这套上限，不同 `App` 互不影响。
/// 装载器在 `LoaderConfig.host_limits` 有值时会在挂载前调用它。
///
/// 只影响**之后注册**的实例（实例在注册时取快照）；已注册实例不会被追溯修改。
/// 任一项为 0 会返回错误且不改动当前值。
pub fn set_host_limits_for(ctx: &Ctx, limits: HostLimits) -> std::result::Result<(), String> {
    limits.validate()?;
    ctx.store.set_extension(limits);
    Ok(())
}

/// 设置**单个 App** 的生命周期三段等待预算（apply / drain / dispose）。
///
/// 底层直接写入 `ctx.store`，因此同 App 下内核（`Fiber::load` / `unload` / `dispose_with_report`）
/// 与宿主 T2 载体（`OP_APPLY` / 排水 / `OP_CLEANUP` / `OP_DISPOSE`）共享同一份配置；
/// 不同 `App` 互不影响。任一项传入 `Some(0)` 会返回 `Err` 且不改动当前生效配置。
pub fn set_lifecycle_policy_for(
    ctx: &Ctx,
    policy: LifecyclePolicy,
) -> std::result::Result<(), String> {
    ctx.store.set_lifecycle_policy(policy)
}

/// 读取**单个 App** 当前的生命周期等待预算。
pub fn lifecycle_policy_for(ctx: &Ctx) -> LifecyclePolicy {
    ctx.store.lifecycle_policy()
}

/// 内部读取：per-App 优先；缺失时回落到进程级默认。
pub(crate) fn host_limits_for_ctx(ctx: &Ctx) -> HostLimits {
    ctx.store
        .extension::<HostLimits>()
        .unwrap_or_else(default_host_limits)
}

// ------------------------------------------------------------ 启动与故障处置策略（阶段 3）

/// 单条配置行的启动级别。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RowStartup {
    /// 必需行（缺省）：启动期失败按 `LoaderConfig::strict` 处理（默认 fail loud）。
    #[default]
    Required,
    /// 可选行：仅当错误类别为 [`FailureCategory::RuntimeUnavailable`] 时允许降级
    /// （记录警告与 `loader.report`，不阻断启动）。
    ///
    /// [`FailureCategory::Source`]、[`FailureCategory::Integrity`] 与
    /// [`FailureCategory::PersistedState`] 不会因行标记为 `Optional` 而静默放行，
    /// 仍由各自的独立处置策略控制（默认 fail-closed）。
    Optional,
}

impl RowStartup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
        }
    }
}

/// 装载器故障分类。
///
/// 区分「可由行级 `Optional` 降级的运行不可用」与「必须拥有独立处置策略的来源 / 完整性 / 持久化状态错误」。
///
/// 归类边界：
/// - `RuntimeUnavailable`：解析不到插件路径、开发模式下 `DynLib::open` 失败、探活失败、`apply`/`validate`/`!!js` 失败、超时或后台清理未结束阻断重挂；
/// - `Source`：未配置来源策略（默认拒绝）、artifact 不在 allow-list、allow-list 身份字段（`id`/`version`/`source_revision`/`abi_version`）不匹配；
/// - `Integrity`：启用来源策略时缺失/损坏/非法 sidecar manifest、artifact 或暂存副本 SHA-256 不匹配、ABI 入口/前缀/版本/`PluginDesc` 字段校验失败、`PluginDesc.id` 与 manifest 不一致；
/// - `PersistedState`：主配置/patch 文件读取或解析失败、重复行 `id` 冲突、运行期配置回写基线冲突或 patch 层落盘失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// 运行不可用（找不到插件、探活失败、`apply`/`validate`/`!!js` 失败、超时等）。
    RuntimeUnavailable,
    /// 来源准入错误（未配置来源策略、不在 allow-list、allow-list 身份不匹配等）。
    Source,
    /// 完整性 / 契约校验错误（SHA-256 不匹配、sidecar manifest 缺失/损坏/非法、ABI 前缀/版本不匹配等）。
    Integrity,
    /// 持久化状态错误（主配置/patch 读取与解析失败、重复行 `id`、回写基线冲突、patch 层落盘失败等）。
    PersistedState,
}

impl FailureCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::Source => "source",
            Self::Integrity => "integrity",
            Self::PersistedState => "persisted_state",
        }
    }
}

/// 宿主对特定故障类别的处置策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureDisposition {
    /// 缺省（fail-closed）：该类别错误直接导致启动失败或上报非降级错误。
    #[default]
    Fail,
    /// 显式降级：不挂载失败行 / 跳过损坏层，记录告警与 `loader.report` 诊断而不拖垮启动。
    Degrade,
}

impl FailureDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fail => "fail",
            Self::Degrade => "degrade",
        }
    }
}

/// 单个 App 的启动故障处置策略与行级覆盖。
///
/// 三类敏感错误（`source` / `integrity` / `persisted_state`）拥有彼此独立的处置开关，
/// 默认全部为 [`FailureDisposition::Fail`]（fail-closed）；显式安装本策略后，
/// 这三类错误不再受行级 `Optional` 或 `LoaderConfig::strict = false` 影响，仅服从各自开关。
///
/// 注意：
/// - `with_persisted_state(Degrade)` 控制**初始启动**对损坏主配置/patch 层的降级；运行期 `reload()`
///   遇到读取/解析错误始终保留上一份有效快照（防止编辑半截的 patch 静默撤销既有覆写），并按精确文件路径
///   记录到 `persistedStateErrors`；
/// - `LoaderConfig::strict_sidecars = true` 依赖预检同样感知行级 `Optional`（仅降级 `RuntimeUnavailable`）
///   与本策略对 `Source` / `Integrity` 的独立开关，不会与策略语义冲突。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupPolicy {
    source: FailureDisposition,
    integrity: FailureDisposition,
    persisted_state: FailureDisposition,
    row_overrides: HashMap<String, RowStartup>,
}

impl StartupPolicy {
    /// 构造默认 fail-closed 策略（`source` / `integrity` / `persisted_state` 均为 `Fail`）。
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_source(mut self, disposition: FailureDisposition) -> Self {
        self.source = disposition;
        self
    }

    pub fn with_integrity(mut self, disposition: FailureDisposition) -> Self {
        self.integrity = disposition;
        self
    }

    pub fn with_persisted_state(mut self, disposition: FailureDisposition) -> Self {
        self.persisted_state = disposition;
        self
    }

    /// 按稳定行标或条目 `id` 覆盖行级启动策略（宿主代码级覆盖优先于 YAML 声明）。
    pub fn with_row_startup(mut self, label_or_id: impl Into<String>, startup: RowStartup) -> Self {
        self.row_overrides.insert(label_or_id.into(), startup);
        self
    }

    /// 便捷入口：将指定行标或 `id` 标为 [`RowStartup::Optional`]。
    pub fn with_optional_row(self, label_or_id: impl Into<String>) -> Self {
        self.with_row_startup(label_or_id, RowStartup::Optional)
    }

    /// 便捷入口：将指定行标或 `id` 标为 [`RowStartup::Required`]。
    pub fn with_required_row(self, label_or_id: impl Into<String>) -> Self {
        self.with_row_startup(label_or_id, RowStartup::Required)
    }

    pub fn source(&self) -> FailureDisposition {
        self.source
    }

    pub fn integrity(&self) -> FailureDisposition {
        self.integrity
    }

    pub fn persisted_state(&self) -> FailureDisposition {
        self.persisted_state
    }

    /// 读取程序化行级覆盖（先精确匹配稳定 `label`，再回落匹配 `entry.id`）。
    pub fn row_startup(&self, label: &str, entry_id: Option<&str>) -> Option<RowStartup> {
        self.row_overrides
            .get(label)
            .copied()
            .or_else(|| entry_id.and_then(|id| self.row_overrides.get(id).copied()))
    }
}

/// 为单个 App 安装或清除 [`StartupPolicy`]。
///
/// 传 `None` 恢复默认行为（行级 `Optional` 仅降级 `RuntimeUnavailable`，`Source`/`Integrity`/`PersistedState`
/// 默认 fail-closed，兼容既有 `strict = false` 诊断路径）。
pub fn set_startup_policy_for(ctx: &Ctx, policy: Option<StartupPolicy>) {
    match policy {
        Some(policy) => ctx.store.set_extension(policy),
        None => ctx.store.remove_extension::<StartupPolicy>(),
    }
}

/// 读取单个 App 当前安装的 [`StartupPolicy`]（未显式安装时为 `None`）。
pub fn startup_policy_for(ctx: &Ctx) -> Option<StartupPolicy> {
    ctx.store.extension::<StartupPolicy>()
}

// ------------------------------------------------------------ 宿主方法白名单（阶段 3）

#[derive(Clone, Debug, PartialEq, Eq)]
enum CallerRule {
    Row(String),
    Plugin(String),
    Uid(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MethodRule {
    Exact(String),
    AllMethods,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AllowRule {
    caller: CallerRule,
    service: String,
    method: MethodRule,
}

/// 宿主管理调用入口的方法白名单策略（默认拒绝）。
///
/// 生效范围（写入 `docs/support-contract.md`）：
/// - **覆盖**：宿主管理的 `call_iface`、`call_iface_stream` 与 T2 `HostApi::ctx_call_iface`；
/// - **身份来源**：由宿主/内核直接从调用点 `Context` 所属 `Fiber` 绑定的 [`CallerIdentity`]
///   （`fiber_uid` / `plugin` / 不可变 `row_label` / `is_root`），完全不依赖 `TraceSink`
///   或 `current_caller_uid()`；
/// - **身份强度提示**：`allow_row_*` 匹配装载器在挂载前绑定的配置行标（插件无法自报篡改）；
///   `allow_plugin_*` 匹配 `fiber.name()`（即插件自报 `Plugin::name()` / `PluginDesc.id`），
///   对 T2 插件应优先使用 `allow_row_*` 或配合 `PluginSourcePolicy`（校验 manifest `id` + SHA-256）使用；
/// - **不覆盖**：同进程原生 `Ctx::get` / `Ctx::require` / `Ctx::require_as` / `Ctx::iface`
///   （含 `#[cordis::service]` 生成的 `<Trait>Service::of` 类型直调）、以及事件派发
///   （`emit` / `parallel` / `serial` / `bail` / `waterfall`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodAllowList {
    allow_root: bool,
    rules: Vec<AllowRule>,
}

impl Default for MethodAllowList {
    fn default() -> Self {
        Self::new()
    }
}

impl MethodAllowList {
    /// 构造默认拒绝的白名单（默认放行存活的宿主根上下文 `is_root = true`）。
    pub fn new() -> Self {
        Self {
            allow_root: true,
            rules: Vec::new(),
        }
    }

    /// 构造严格全拒绝白名单（连同宿主根上下文也必须显式放行）。
    pub fn deny_all() -> Self {
        Self {
            allow_root: false,
            rules: Vec::new(),
        }
    }

    /// 设置是否默认放行存活的宿主根上下文（`CallerIdentity::is_root == true`）。
    pub fn with_allow_root(mut self, allow: bool) -> Self {
        self.allow_root = allow;
        self
    }

    /// 放行指定装载器行标调用 `service.method`。
    pub fn allow_row_method(
        mut self,
        row_label: impl Into<String>,
        service: impl Into<String>,
        method: impl Into<String>,
    ) -> Self {
        self.rules.push(AllowRule {
            caller: CallerRule::Row(row_label.into()),
            service: service.into(),
            method: MethodRule::Exact(method.into()),
        });
        self
    }

    /// 放行指定装载器行标调用 `service` 的一组方法。
    pub fn allow_row_methods<I, S>(
        mut self,
        row_label: impl Into<String>,
        service: impl Into<String>,
        methods: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let row_label = row_label.into();
        let service = service.into();
        for method in methods {
            self.rules.push(AllowRule {
                caller: CallerRule::Row(row_label.clone()),
                service: service.clone(),
                method: MethodRule::Exact(method.into()),
            });
        }
        self
    }

    /// 放行指定装载器行标调用 `service` 的全部方法。
    pub fn allow_row_service(
        mut self,
        row_label: impl Into<String>,
        service: impl Into<String>,
    ) -> Self {
        self.rules.push(AllowRule {
            caller: CallerRule::Row(row_label.into()),
            service: service.into(),
            method: MethodRule::AllMethods,
        });
        self
    }

    /// 放行指定插件名调用 `service.method`。
    pub fn allow_plugin_method(
        mut self,
        plugin: impl Into<String>,
        service: impl Into<String>,
        method: impl Into<String>,
    ) -> Self {
        self.rules.push(AllowRule {
            caller: CallerRule::Plugin(plugin.into()),
            service: service.into(),
            method: MethodRule::Exact(method.into()),
        });
        self
    }

    /// 放行指定插件名调用 `service` 的一组方法。
    pub fn allow_plugin_methods<I, S>(
        mut self,
        plugin: impl Into<String>,
        service: impl Into<String>,
        methods: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let plugin = plugin.into();
        let service = service.into();
        for method in methods {
            self.rules.push(AllowRule {
                caller: CallerRule::Plugin(plugin.clone()),
                service: service.clone(),
                method: MethodRule::Exact(method.into()),
            });
        }
        self
    }

    /// 放行指定插件名调用 `service` 的全部方法。
    pub fn allow_plugin_service(
        mut self,
        plugin: impl Into<String>,
        service: impl Into<String>,
    ) -> Self {
        self.rules.push(AllowRule {
            caller: CallerRule::Plugin(plugin.into()),
            service: service.into(),
            method: MethodRule::AllMethods,
        });
        self
    }

    /// 放行指定 fiber uid 调用 `service.method`。
    pub fn allow_uid_method(
        mut self,
        fiber_uid: u64,
        service: impl Into<String>,
        method: impl Into<String>,
    ) -> Self {
        self.rules.push(AllowRule {
            caller: CallerRule::Uid(fiber_uid),
            service: service.into(),
            method: MethodRule::Exact(method.into()),
        });
        self
    }

    /// 放行指定 fiber uid 调用 `service` 的全部方法。
    pub fn allow_uid_service(mut self, fiber_uid: u64, service: impl Into<String>) -> Self {
        self.rules.push(AllowRule {
            caller: CallerRule::Uid(fiber_uid),
            service: service.into(),
            method: MethodRule::AllMethods,
        });
        self
    }

    /// 判定由宿主绑定的 `caller` 是否允许调用 `service.method`。
    pub fn check(
        &self,
        caller: &CallerIdentity,
        service: &str,
        method: &str,
    ) -> std::result::Result<(), String> {
        if caller.fiber_uid == cordis_core::UID_DISPOSED {
            return Err("调用方上下文已处置（UID_DISPOSED），拒绝调用".to_string());
        }
        if caller.is_root && self.allow_root {
            return Ok(());
        }
        let allowed = self.rules.iter().any(|rule| {
            if rule.service != service {
                return false;
            }
            let method_ok = match &rule.method {
                MethodRule::AllMethods => true,
                MethodRule::Exact(allowed_method) => allowed_method == method,
            };
            if !method_ok {
                return false;
            }
            match &rule.caller {
                CallerRule::Row(label) => caller.row_label.as_deref() == Some(label.as_str()),
                CallerRule::Plugin(name) => caller.plugin == *name,
                CallerRule::Uid(uid) => caller.fiber_uid == *uid,
            }
        });
        if allowed {
            Ok(())
        } else {
            Err(format!("白名单未授权 {caller} 调用 {service}.{method}"))
        }
    }
}

/// 为单个 App 安装或清除宿主方法白名单策略。
///
/// 传 `Some(policy)` 后，本 `App` 上所有经由 `Context::call_iface`、`Context::call_iface_stream`
/// 及 T2 `HostApi::ctx_call_iface` 发起的方法调用都会在执行前按宿主绑定的 [`CallerIdentity`]
/// 校验白名单（默认拒绝）；传 `None` 清除白名单限制。
pub fn set_method_allow_list_for(ctx: &Ctx, policy: Option<MethodAllowList>) {
    match policy {
        Some(policy) => {
            let shared = Arc::new(policy.clone());
            ctx.store.set_call_auth_hook(Some(Arc::new(
                move |caller: &CallerIdentity, service: &str, method: &str| {
                    shared.check(caller, service, method)
                },
            )));
            ctx.store.set_extension(policy);
        }
        None => {
            ctx.store.set_call_auth_hook(None);
            ctx.store.remove_extension::<MethodAllowList>();
        }
    }
}

/// 读取单个 App 当前安装的方法白名单策略（未安装时为 `None`）。
pub fn method_allow_list_for(ctx: &Ctx) -> Option<MethodAllowList> {
    ctx.store.extension::<MethodAllowList>()
}
