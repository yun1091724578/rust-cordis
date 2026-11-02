//! T1 **typed 服务**的值形态（M6，G6）。
//!
//! 只有一条类型，但它修掉的是 G6 记的那个**值形态缺陷**：`Dyn` 装的是 `Arc<dyn Any>`，
//! 而 `Any` 要求 `Sized`——于是 `Dyn::new(arc)` 装进去的是 `Arc<T>` **本体**：
//! `downcast::<T>()` 必然失败，`downcast::<Arc<T>>()` 拿回来的是 `Arc<Arc<T>>`
//! （插件之间多一层引用计数，且 `dyn Trait` 根本装不进去）。

use std::sync::Arc;

/// 一个 typed 服务的注册值：`Iface::<dyn Greeter>(Arc<dyn Greeter>)`。
///
/// 三个性质，各对应一条契约：
///
/// 1. **可装箱**：`Iface<T>` 本身是 `Sized`（`Arc<T>` 对 `T: ?Sized` 也是 `Sized`），
///    所以 `Dyn::new(Iface(arc))` 合法——这是 `dyn Trait` 进 `Any` 的唯一通道；
/// 2. **类型可分**：`Iface<dyn A>` 与 `Iface<dyn B>` 是**不同的 `TypeId`**，
///    于是"服务在但类型不对"能被判出来（[`crate::ResolveError::TypeMismatch`]），
///    而不是伪装成"没声明 inject"；
/// 3. **同一份引用**：取回的是 `Arc<T>` 本身的克隆，不加层（G6 的"双重 `Arc`"）。
///
/// JSON 面（T2/T3 调用者按方法名调用）挂在**同一个注册值**上，见
/// [`Dyn::with_adapter`](crate::Dyn::with_adapter)——两条通道因此指向同一个对象
/// （D2 的 T1/T2 规则：一个 key 在实现域内只有一个实现）。
pub struct Iface<T: ?Sized + Send + Sync + 'static>(pub Arc<T>);

// 手写 `Clone`：`#[derive(Clone)]` 会给 `T: ?Sized` 加上 `T: Clone` 约束，
// 而我们要的只是"克隆那层 `Arc`"（`dyn Trait` 当然不是 `Clone`）。
impl<T: ?Sized + Send + Sync + 'static> Clone for Iface<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized + Send + Sync + 'static> std::fmt::Debug for Iface<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 `Arc` 的地址：诊断信息必须逐字可复现（demo 的逐字节断言依赖这一点）。
        write!(f, "Iface({})", std::any::type_name::<T>())
    }
}
