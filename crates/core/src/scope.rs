//! Cordis 的 `Object.create(parent)` 在 Rust 的等价物。
//!
//! JS 里 isolate / intercept 两张表都是**原型链**：`Object.create(this[...])` 造子表，
//! 读属性时沿原型链冒泡、子遮蔽父。Rust 没有原型，但有等价的更简单结构：
//! **持久化不可变单链表 + 线性查找**。
//!
//! 映射条目通常只有 0~3 个，线性查找比哈希更快也更好懂。
//! 同一个类型同时服务于 `isolate`（name → 域）与 `intercept`（name → 配置）两张表，
//! 这是本实现"短小"的第一个杠杆点。

use std::sync::Arc;

#[derive(Debug)]
pub struct Scope<K, V> {
    parent: Option<Arc<Scope<K, V>>>,
    entry: Option<(K, V)>,
}

impl<K: Eq, V> Scope<K, V> {
    /// 根表的等价物：空表，无父。
    pub fn root() -> Arc<Self> {
        Arc::new(Self {
            parent: None,
            entry: None,
        })
    }

    /// `Object.create(parent)` + 写入一个条目（等价 `ctx.isolate(name)` / `ctx.intercept(name, cfg)`）。
    pub fn child(parent: &Arc<Self>, key: K, value: V) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(parent.clone()),
            entry: Some((key, value)),
        })
    }

    /// 读：沿链冒泡，子遮蔽父。等价 JS 的 `key in obj` + `obj[key]`。
    pub fn get(&self, key: &K) -> Option<&V> {
        let mut cursor = Some(self);
        while let Some(scope) = cursor {
            if let Some((k, v)) = &scope.entry {
                if k == key {
                    return Some(v);
                }
            }
            cursor = scope.parent.as_deref();
        }
        None
    }

    /// 只看自己这一层，不冒泡（等价 `Object.hasOwn(obj, key)`）。
    pub fn own(&self, key: &K) -> Option<&V> {
        match &self.entry {
            Some((k, v)) if k == key => Some(v),
            _ => None,
        }
    }

    /// 上一层。用于"从远到近"收集整条链（如 intercept 配置的祖先优先合并）。
    pub fn parent_arc(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }
}
