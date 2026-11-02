//! **volatile 配置引用**（M4-5）：一个身份稳定、值可被内核原地提交的句柄。
//!
//! # 它解决什么问题
//! 有些配置值的"变"和别的配置不一样：连接池、日志目录、凭据轮换这类东西，
//! 用户改配置时**不该让插件重挂**——重挂会丢掉连接、断掉会话。
//! 上游把它表达成 schemastery 的 `.volatile()`：这种字段在**校验期**被换成
//! `Volatile<T>` 引用（cosmokit `volatile.ts:39-45`），插件拿引用、读 `.get()`；
//! 配置只差 volatile 字段时，装载器**不重挂**，而是把新值写进**原来那个引用**
//! （`updateVolatile`，`:84-86`）。
//!
//! # 本仓的形态（对应上游的哪些东西）
//! - [`VolatileRef`] ↔ cosmokit 的 `Volatile<T>`（`get()` 相同；写入是内核权限）；
//! - [`Plugin::volatile_paths`](crate::Plugin::volatile_paths) ↔ schema 里的 `.volatile()` 元数据；
//! - [`declare_volatile`] ↔ 给"函数式插件"补声明（等价 JS 里"这个插件的 schema 声明了 volatile"）；
//! - [`split_path`] / [`path_value`] ↔ `volatileEntries` 收集出来的**点号路径**
//!   （上游要求 volatile 字段在**固定对象路径**上：数组元素与动态键不允许，
//!   见 `schemastery/src/index.ts:488-506` 的 `validateVolatileSchema`）；
//! - [`equal_except_volatile`] ↔ `equalExceptVolatile`（`config/diff.ts:43`）与
//!   `deepEqual(…, strict)` 里"两个 volatile 引用恒相等"那条规则。
//!
//! # 为什么引用不放进配置值里
//! 上游把引用塞在"已解析配置"的对象图里（JS 里什么都能放）。本仓的 [`Config`]
//! 是 JSON——往里塞 `Arc` 只能靠伪造标记（像 `!!js` 那样），那会让"配置是数据"
//! 这条性质出现例外。所以引用由**内核按路径持有**（[`Fiber`](crate::Fiber) 的表），
//! 插件用 [`Ctx::volatile`](crate::Ctx::volatile) 取同一个 `Arc`。

use crate::dyn_value::Config;
use crate::{BoxFuture, Ctx, Plugin, PluginKey, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 一个**稳定引用**：插件持有它，值只在 volatile-only 更新时被内核原地改写。
///
/// 与 [`Dyn`](crate::Dyn) 的区别是用途：`Dyn` 是"任意值"的传递包装，
/// `VolatileRef` 是"**同一个位置**的值会变"的显式声明——插件因此可以放心缓存它
/// （连接池对象、缓存的目录句柄），而不必在每次配置更新时重建。
#[derive(Debug)]
pub struct VolatileRef {
    snapshot: Mutex<Config>,
}

impl VolatileRef {
    /// 建一个新引用（内核在 fiber 装载、解析出配置之后调用）。
    pub fn new(value: Config) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(value),
        })
    }

    /// 当前快照。**每次读都是一次拷贝**——引用只保证"位置稳定"，
    /// 不保证读到的值能一直用（对应 cosmokit 注释里的
    /// "keep the reference, or capture its value for one operation only"）。
    pub fn get(&self) -> Config {
        self.snapshot.lock().unwrap().clone()
    }

    /// 原地提交新值（对应 cosmokit `updateVolatile`）。
    ///
    /// **只有内核与装载器该调它**：插件侧写引用等于绕过"配置是唯一真相"这条纪律。
    /// 之所以是 `pub` 而不是 `pub(crate)`：volatile 提交发生在**装载器**（另一个 crate）里。
    pub fn commit(&self, value: Config) {
        *self.snapshot.lock().unwrap() = value;
    }
}

/// 把点号路径切成段（`"a.b"` → `["a", "b"]`）。
///
/// 与上游一致：路径段就是对象键，**数组下标不在 volatile 的声明范围内**
/// （`validateVolatileSchema` 把 `*` / `<key>`（数组与动态键）一律拒绝）。
pub fn split_path(path: &str) -> Vec<String> {
    path.split('.')
        .filter(|segment| !segment.is_empty())
        .map(String::from)
        .collect()
}

/// 取配置里某条路径的值（段不存在 ⇒ `None`；等价上游 `path.reduce((v, k) => Reflect.get(v, k))`
/// 里"取不到就是 `undefined`"的那一半）。
pub fn path_value<'a>(config: &'a Config, path: &[String]) -> Option<&'a Config> {
    let mut cursor = config;
    for segment in path {
        cursor = cursor.get(segment.as_str())?;
    }
    Some(cursor)
}

/// 声明路径的合法性（对应 `validateVolatileSchema`）：非空、不含空段、**互不嵌套**。
///
/// 为什么禁止嵌套：`a` 与 `a.b` 同时 volatile 会让"这次提交改了哪些路径"没有唯一答案
/// （写 `a` 时 `a.b` 的引用还指在旧值上），上游直接把它当配置错误。
pub fn validate_volatile_paths(paths: &[String]) -> std::result::Result<(), String> {
    let mut seen: Vec<Vec<String>> = Vec::new();
    for path in paths {
        // `split_path` 会过滤空段，所以必须先对原始字符串检查——否则 "a..b"
        // 会被静默折叠成 "a.b"，让"声明了哪条路径"与用户写的不是同一个东西。
        if path.split('.').any(|segment| segment.is_empty()) {
            return Err(format!("volatile 路径不能含空段（收到 \"{path}\"）"));
        }
        let segments = split_path(path);
        if segments.is_empty() {
            return Err(format!("volatile 路径不能为空（收到 \"{path}\"）"));
        }
        let nested = seen.iter().any(|known| {
            known.len() <= segments.len() && known.iter().zip(&segments).all(|(a, b)| a == b)
                || segments.len() <= known.len() && segments.iter().zip(known).all(|(a, b)| a == b)
        });
        if nested {
            return Err(format!(
                "volatile 路径不能互相嵌套（\"{path}\" 与已声明的某条重叠）"
            ));
        }
        seen.push(segments);
    }
    Ok(())
}

/// 去掉 volatile 路径之后的副本（比较用：这些位置上的差异一律视为"不算差异"）。
///
/// 用"抹掉这些位置再整体比较"而不是"边比边跳过"：配置很小，而
/// "相等"这件事必须一眼能看懂——它决定"这次更新要不要重挂"。
pub fn without_volatile(config: &Config, paths: &[Vec<String>]) -> Config {
    fn walk(value: &Config, paths: &[Vec<String>], prefix: &mut Vec<String>) -> Config {
        if paths
            .iter()
            .any(|path| path.len() == prefix.len() && path == prefix)
        {
            // 正好走在一条 volatile 路径的末端：整段不参与比较（`null` 当哨兵）。
            return Config::Null;
        }
        match value {
            Config::Object(object) => {
                let mut out = serde_json::Map::with_capacity(object.len());
                for (key, item) in object {
                    prefix.push(key.clone());
                    out.insert(key.clone(), walk(item, paths, prefix));
                    prefix.pop();
                }
                Config::Object(out)
            }
            Config::Array(list) => {
                Config::Array(list.iter().map(|item| walk(item, paths, prefix)).collect())
            }
            other => other.clone(),
        }
    }
    walk(config, paths, &mut Vec::new())
}

/// 两份配置"除 volatile 路径之外"是否相等（对应 `equalExceptVolatile`）。
///
/// 这是 volatile-only 判定与"候选 vs 运行值"判定的共同内核：
/// **前置条件**用它比原始配置，"这次更新要不要重挂"用它比已解析配置。
pub fn equal_except_volatile(left: &Config, right: &Config, paths: &[Vec<String>]) -> bool {
    without_volatile(left, paths) == without_volatile(right, paths)
}

/// 给任意插件**补上 volatile 声明**（M4-5）。
///
/// 上游的声明写在 schema 里（`.volatile()`），本仓没有 schema 库，
/// 于是声明是一等数据：`declare_volatile(plugin_fn(...), &["data"])`。
/// 用包装器而不是给 [`FnPlugin`](crate::FnPlugin) 加字段，是为了不惊动
/// 已有的每一个构造点——声明是**附加能力**，不是插件的必填项。
pub fn declare_volatile(inner: Arc<dyn Plugin>, paths: &[&str]) -> Arc<dyn Plugin> {
    Arc::new(VolatilePlugin {
        inner,
        paths: paths.iter().map(|path| (*path).to_string()).collect(),
    })
}

/// [`declare_volatile`] 的实现体（组合而非继承：`Plugin` 是 trait，没有"子类"）。
struct VolatilePlugin {
    inner: Arc<dyn Plugin>,
    paths: Vec<String>,
}

impl Plugin for VolatilePlugin {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn key(&self) -> PluginKey {
        self.inner.key()
    }

    fn inject(&self) -> crate::Inject {
        self.inner.inject()
    }

    fn validate(&self, raw: Config) -> std::result::Result<Config, crate::ValidationError> {
        self.inner.validate(raw)
    }

    /// 包装器必须转发声明：否则 `declare_volatile(with_provision(...))` 会让
    /// `provision()` 变回默认空集，Def48/Def76 检查被静默跳过。
    fn provision(&self) -> Vec<String> {
        self.inner.provision()
    }

    fn apply(&self, ctx: Ctx, config: Config) -> BoxFuture<Result<()>> {
        self.inner.apply(ctx, config)
    }

    fn volatile_paths(&self) -> Vec<String> {
        self.paths.clone()
    }

    fn apply_volatile(&self, ctx: &Ctx, values: &[(String, Config)]) -> Result<()> {
        self.inner.apply_volatile(ctx, values)
    }
}

/// 一份"路径 → 新值"的 volatile 提交（装载器与 ABI 侧共用的载荷形状）。
pub type VolatileValues = Vec<(String, Config)>;

/// 便捷：按声明路径从配置里取出一组值（取不到的路径**跳过**——
/// 上游 `volatileEntries` 也只收集"确实存在于已解析配置里"的引用）。
pub fn values_at(config: &Config, paths: &[String]) -> VolatileValues {
    paths
        .iter()
        .filter_map(|path| {
            path_value(config, &split_path(path)).map(|value| (path.clone(), value.clone()))
        })
        .collect()
}

/// 引用表（fiber 用）。键是**点号路径**，与插件声明、`Ctx::volatile` 的入参同一形状。
pub(crate) type VolatileTable = HashMap<Box<str>, Arc<VolatileRef>>;
