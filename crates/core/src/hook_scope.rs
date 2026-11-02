//! 作用域锚点（M4）：**注册期 scope** 的身份、父键关系与派发期谓词。
//!
//! # 它解决什么
//! 多 agent / 多会话 / 子代理树需要"同一棵进程里，各自只看见自己那一支的注册"。
//! 既有两条派发轴（`realm_filter` 服务域、`fiber_filter` 生命周期子树）是**派发期**
//! 筛既有钩子；这里补的是"注册**在哪里**"：作用域 ctx 上的注册带着 [`ScopeAnchor`]，
//! 派发点用**自己的**谓词逐个裁决（逐条对齐上游 `events.ts:76-85`：
//! `thisArg[Context.filter].call(thisArg, hook.ctx)`，`hook.global` 旁路）。
//!
//! # 两条落地规则（都写进台账）
//! 1. **派发点没有 scope ⇒ 这一轴不参与**：既有代码全部属于这一类，行为逐字不变。
//! 2. **没有 scope 的*注册*永远参与**（本仓相对上游的**有意偏离**）：上游的 filter
//!    管所有钩子，而本仓既有的钩子全都没有 scope——管上它们就等于让所有既有监听器
//!    在"带 scope 的派发点"下集体失声。于是 scope 只能**藏起自己这一支**，
//!    全局观察者（比如 harness 的审计插件）注册在无 scope 的 ctx 上即可照旧工作。
//!
//! # 为什么不复用 fiber
//! 作用域要**比 fiber 活得久**（会话结束、fiber 卸载之后，子代理树的关系还要能查），
//! 而且一个 fiber 上可以叠多个作用域。于是锚点只持 `Weak<Fiber>`（防环）：
//! 身份是单调 uid，父键关系可**重新绑定**。

use crate::Ctx;
use crate::Fiber;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// 拓扑锁：`bind_parent` 的"检查祖先关系 + 写父指针"必须是一个原子步。
///
/// 只锁节点自己的 `parent` 会让两个线程同时通过检查（A→B 与 B→A），
/// 形成强引用环；查询与修改共用这把锁后，拓扑在任意时刻都是一致的。
static TOPOLOGY: Mutex<()> = Mutex::new(());

/// 作用域谓词：派发点用它逐个裁决"某个注册参不参与这次派发"。
///
/// 入参是**注册者**的 ctx（上游的 `hook.ctx`）——因此 `Ctx::in_scope_of` 是写它的
/// 常用原料（`Ctx::filter_subtree` 就是那个组合）。
pub type ScopePredicate = Arc<dyn Fn(&Ctx) -> bool + Send + Sync>;

/// 一个作用域的身份（M4）。见模块文档。
pub struct ScopeAnchor {
    uid: u64,
    /// 创建它的 fiber（弱引用：锚点常被 hook 记着，强引用会把 fiber 吊住）。
    fiber: Option<Weak<Fiber>>,
    /// 父键关系（可重绑）。`bind_parent` 逐条检查成环。
    parent: Mutex<Option<Arc<ScopeAnchor>>>,
    predicate: ScopePredicate,
}

fn next_scope_uid() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

impl ScopeAnchor {
    pub(crate) fn new(fiber: Option<Weak<Fiber>>, predicate: ScopePredicate) -> Arc<Self> {
        Arc::new(Self {
            uid: next_scope_uid(),
            fiber,
            parent: Mutex::new(None),
            predicate,
        })
    }

    /// 进程内单调、从 1 起、**永不复用**（与实例/派生命柄同一纪律）。
    pub fn uid(&self) -> u64 {
        self.uid
    }

    /// 创建它的 fiber（已经走了就是 `None`——作用域本身不受影响）。
    pub fn fiber(&self) -> Option<Arc<Fiber>> {
        self.fiber.as_ref().and_then(Weak::upgrade)
    }

    /// 派发期的谓词：入参是**注册者**的 ctx。
    pub fn predicate(&self) -> &ScopePredicate {
        &self.predicate
    }

    /// `self` 是不是 `other` 的祖先（或就是它自己）。
    ///
    /// 与 [`ScopeAnchor::bind_parent`] 共用拓扑锁：查询看到的是一条不会被
    /// 并发改写撕开的父链。`visited` 是对**历史遗留环**的防御——即使环已经
    /// 存在（旧版本或外部错误状态），遍历也必须在有限步内结束。
    pub fn is_ancestor_or_self_of(self: &Arc<Self>, other: &Arc<ScopeAnchor>) -> bool {
        let _topology = TOPOLOGY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.is_ancestor_or_self_locked(other)
    }

    fn is_ancestor_or_self_locked(self: &Arc<Self>, other: &Arc<ScopeAnchor>) -> bool {
        let mut cursor = Some(other.clone());
        let mut visited = HashSet::new();
        while let Some(node) = cursor {
            if node.uid == self.uid {
                return true;
            }
            if !visited.insert(node.uid) {
                return false;
            }
            cursor = node.parent.lock().unwrap().clone();
        }
        false
    }

    /// 父键（只读快照）。
    pub fn parent(&self) -> Option<Arc<ScopeAnchor>> {
        self.parent.lock().unwrap().clone()
    }

    /// 绑一个**新**父（可重绑）。成环 ⇒ 可读错误，且关系原封不动。
    pub fn bind_parent(
        self: &Arc<Self>,
        parent: &Arc<ScopeAnchor>,
    ) -> std::result::Result<(), String> {
        // 检查与写入在同一个拓扑临界区里：两个方向相反的并发绑定只能成功一个。
        let _topology = TOPOLOGY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.uid == parent.uid || self.is_ancestor_or_self_locked(parent) {
            return Err(format!(
                "绑定作用域父键会成环：{} 自己（或它的后代）不能当它的父",
                self.uid
            ));
        }
        *self.parent.lock().unwrap() = Some(parent.clone());
        Ok(())
    }
}

impl std::fmt::Debug for ScopeAnchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeAnchor")
            .field("uid", &self.uid)
            .field("fiber_alive", &self.fiber().is_some())
            .field("parent", &self.parent().map(|p| p.uid))
            .finish()
    }
}

/// 派发期裁决（M4 的两条落地规则都在这里，store 只管调用）。
pub fn allows(
    dispatch: Option<&Arc<ScopeAnchor>>,
    global: bool,
    hook_scope: Option<&Arc<ScopeAnchor>>,
    hook_ctx: &Ctx,
) -> bool {
    if global {
        return true;
    }
    let Some(scope) = dispatch else {
        return true; // 规则 1：派发点没有 scope ⇒ 这一轴不参与
    };
    if hook_scope.is_none() {
        return true; // 规则 2：没有 scope 的注册永远参与
    }
    (scope.predicate())(hook_ctx)
}
