//! 宿主派发看门狗（[`DispatchPolicy`]）与容量上限（[`HostLimits`]）注册表（O11 拆分）。
//!
//! 两个注册表共享同一语义：per-App 优先（以 `Weak<Store>` 追踪 App 生命周期并在读写时清理失活条目），
//! 缺失时回落到进程级默认值；实例在注册时取一份快照。

use cordis_core::{Ctx, Store};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

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

/// 一个 App 的看门狗条目：`store` 是**弱引用**，App 释放后条目可被顺手清掉，
/// 注册表不会把 `Store` 吊住。
struct AppDispatchPolicy {
    store: Weak<Store>,
    policy: DispatchPolicy,
}

/// 键是 `Arc::as_ptr(&ctx.store)`（App 的身份；派生 ctx 共享同一个 `Arc<Store>`）。
fn app_dispatch_policies() -> &'static Mutex<HashMap<usize, AppDispatchPolicy>> {
    static POLICIES: OnceLock<Mutex<HashMap<usize, AppDispatchPolicy>>> = OnceLock::new();
    POLICIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 设置**单个 App** 的派发看门狗。
///
/// App 由 `ctx.store` 的同一性标识：同一个 `App` 上派生的所有 ctx（isolate /
/// intercept / 装载器行）共享这条策略；不同 `App` 互不影响。装载器在
/// `LoaderConfig.dispatch_policy` 有值时会在挂载前调用它。
///
/// 只影响**之后注册**的实例（实例在注册时取快照）；已注册实例不会被追溯修改。
pub fn set_dispatch_policy_for(ctx: &Ctx, policy: DispatchPolicy) {
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut policies = app_dispatch_policies().lock().unwrap();
    policies.retain(|_, entry| entry.store.strong_count() > 0);
    policies.insert(
        key,
        AppDispatchPolicy {
            store: Arc::downgrade(&ctx.store),
            policy,
        },
    );
}

/// 内部读取：per-App 优先，缺失或 App 已释放时回落到进程级策略。
pub(crate) fn dispatch_policy_for_ctx(ctx: &Ctx) -> DispatchPolicy {
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut policies = app_dispatch_policies().lock().unwrap();
    policies.retain(|_, entry| entry.store.strong_count() > 0);
    policies
        .get(&key)
        .map(|entry| entry.policy)
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

/// 一个 App 的容量上限条目：弱引用 Store，App 释放后条目可被清理。
struct AppHostLimits {
    store: Weak<Store>,
    limits: HostLimits,
}

/// 键是 `Arc::as_ptr(&ctx.store)`（App 的身份；派生 ctx 共享同一个 `Arc<Store>`）。
fn app_host_limits() -> &'static Mutex<HashMap<usize, AppHostLimits>> {
    static LIMITS: OnceLock<Mutex<HashMap<usize, AppHostLimits>>> = OnceLock::new();
    LIMITS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 设置**单个 App** 的容量准入上限。
///
/// App 由 `ctx.store` 的同一性标识：同一个 `App` 上派生的所有 ctx（isolate /
/// intercept / 装载器行）共享这套上限；不同 `App` 互不影响。装载器在
/// `LoaderConfig.host_limits` 有值时会在挂载前调用它。
///
/// 只影响**之后注册**的实例（实例在注册时取快照）；已注册实例不会被追溯修改。
/// 任一项为 0 会返回错误且不改动当前值。
pub fn set_host_limits_for(ctx: &Ctx, limits: HostLimits) -> std::result::Result<(), String> {
    limits.validate()?;
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut registry = app_host_limits().lock().unwrap();
    registry.retain(|_, entry| entry.store.strong_count() > 0);
    registry.insert(
        key,
        AppHostLimits {
            store: Arc::downgrade(&ctx.store),
            limits,
        },
    );
    Ok(())
}

/// 内部读取：per-App 优先；缺失或 App 已释放时回落到进程级默认。
pub(crate) fn host_limits_for_ctx(ctx: &Ctx) -> HostLimits {
    let key = Arc::as_ptr(&ctx.store) as usize;
    let mut registry = app_host_limits().lock().unwrap();
    registry.retain(|_, entry| entry.store.strong_count() > 0);
    registry
        .get(&key)
        .map(|entry| entry.limits)
        .unwrap_or_else(default_host_limits)
}
