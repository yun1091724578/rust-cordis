//! 动态值与可逆副作用。
//!
//! 这是本实现"短小"的第二个杠杆点：JS 的 `effect()` 要支持
//! `void | Disposable | Iterator | Promise | AsyncIterable` 五种返回形态，
//! 还要返回一个"既可直接调用、又可 await"的 wrapper（400 行）。
//! Rust 里 `Disposable` 就是**一个只能跑一次的异步闭包**，配合 `group()` 表达
//! "一次 effect 里 yield 多个 disposer"，语义等价、代码量差一个数量级。

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 手写 BoxFuture，省掉 `futures` 依赖。
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// **借 `&self` 的服务方法**返回的 future（M6）。
///
/// 与 [`BoxFuture`] 只差一个 `'a`：`BoxFuture` 是 `'static`，而 `async fn` 脱糖以后
/// future 借用 `&'a self`（D2）——一个 `'static` 的盒子装不下它。
/// 因此提供者侧的适配器必须**自己持一份 `Arc`** 把对象保住（"一次 `Arc<Self>` 提升"），
/// 而不是指望借用把生命周期拉长。
pub type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 插件配置 / intercept 配置的统一表示。
pub type Config = serde_json::Value;

/// 平台错误类型（对应 JS 里可以 `throw` 任何东西；这里收敛为 Error）。
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// 把 `catch_unwind` 截获的 panic 载荷转成一条可读消息（M5-1）。
///
/// panic 载荷是 `Box<dyn Any + Send>`：常见形态只有两种——`&str`
/// （`panic!("字面量")`）与 `String`（`panic!("{}", x)`）。其余一律退化成
/// 一句"未知载荷"：诊断通道自己绝不能再 panic。
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "未知的 panic 载荷".to_string()
    }
}

/// 可装箱的任意 Rust 值。
///
/// 对应 JS 的动态值：既是**服务值**，也是**事件参数**。
/// 服务值用 `Arc<dyn Any>` 而不是 JSON，好处是插件之间可以直接传 Rust 类型
/// （`ctx.require_as::<Tools>("tools")` 拿到 `Arc<Tools>`），跨 ABI 时才降级为 JSON。
///
/// # 使用规则（真实陷阱）
/// `Dyn::new("x")` 装箱的是 `&'static str`，不是 `String`；两者是不同的 `Any` 类型，
/// `downcast::<String>()` 会失败。跨插件传递的值一律用**自有类型**（`String` / 结构体），
/// 不要依赖类型推断出来的引用类型。
///
/// # 服务值的 JSON 面（M6）
/// 服务值除了被 downcast，还可以带一个**按方法名调用的面孔**（[`Dyn::with_adapter`]）：
/// T1 typed 服务的注册值就是"typed 面 + JSON 面"的组合，两条通道因此指向同一个实现
/// （D2 的 T1/T2 规则，见 [`crate::Iface`]）。判据只有 [`Dyn::adapter`] 一个。
#[derive(Clone)]
pub struct Dyn(Arc<dyn Any + Send + Sync>, Option<Arc<dyn ExternService>>);

impl Dyn {
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self(Arc::new(value), None)
    }

    /// 把一个**已有 `Arc`** 装箱，`downcast::<T>()` 直接拿回同一份 `Arc<T>`。
    ///
    /// 与 `Dyn::new(arc)` 的区别正是上面"真实陷阱"那一节：后者装箱的是 `Arc<T>`
    /// 本体，`downcast::<T>()` 会失败。事件参数里要传共享句柄（比如 fiber）时用这个。
    pub fn shared<T: Any + Send + Sync>(value: Arc<T>) -> Self {
        Self(value as Arc<dyn Any + Send + Sync>, None)
    }

    /// 给这个服务值挂上 **JSON 面**（M6）：一个按方法名调用的适配器。
    ///
    /// 于是同一个注册值既能被 T1 用 `ctx.iface::<dyn T>(key)` 类型化地拿回来，
    /// 又能被 T2/T3 用 `ctx.call_iface(key, …)` 调到——**同一份实现**。
    pub fn with_adapter(self, adapter: Arc<dyn ExternService>) -> Self {
        Self(self.0, Some(adapter))
    }

    /// 「能被按方法名调用吗」的**唯一判据**：显式挂上的适配器，或值本身就是
    /// [`IfaceRef`]（T2 提供者注册服务接口用的那条既有通道）。
    ///
    /// 把两条通道收在这里，是因为"什么算可调用接口"必须只有一个答案——
    /// 否则同一个服务会随读取入口时而可调、时而不可调（读取语义的统一口径同
    /// `host::encode_service`）。
    pub fn adapter(&self) -> Option<Arc<dyn ExternService>> {
        match &self.1 {
            Some(adapter) => Some(adapter.clone()),
            None => self
                .0
                .clone()
                .downcast::<IfaceRef>()
                .ok()
                .map(|iface| iface.0.clone()),
        }
    }

    pub fn downcast<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.0.clone().downcast::<T>().ok()
    }

    pub fn is<T: Any + Send + Sync>(&self) -> bool {
        self.0.is::<T>()
    }
}

impl std::fmt::Debug for Dyn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dyn(<dyn>)")
    }
}

/// 跨 ABI 边界的服务值：JSON 表示。
///
/// Rust 进程内插件之间可以直接传 `Arc<T>`（零拷贝、类型安全）；一旦越过 C ABI，
/// 类型标识本身就不可能稳定，所以**跨 ABI 的服务值一律降级为 JSON**。
/// 这个类型就是那条边界线的显式标记。
#[derive(Clone, Debug)]
pub struct ExternValue(pub Config);

/// **带方法的服务**在核心里面的抽象。
///
/// # 为什么内核需要知道"服务可以有方法"
/// 只有 `ExternValue` 的插件生态是不成立的：插件之间只能交换 JSON *值* 与事件，
/// 而真实插件的能力形态是**服务方法**（`tools.register()`、`settings.configure()`）。
/// cordis 文档里那句"其他插件通过 key 查找服务，而非导入具体实现"要落地，
/// 就必须有一条"按方法名调用"的通道。
///
/// # 为什么是 trait 而不是 vtable
/// 内核**不知道 C ABI 的存在**。它只知道三件事：这个服务叫什么、接口版本是多少、
/// 能不能按方法名调用。路由到具体插件、JSON 编解码、跨边界错误映射，
/// 全部是载体层（`cordis-host`）的事。于是换载体（WASM/进程外）不动内核。
///
/// # 版本协商规则
/// **请求版本 ≤ 提供版本即兼容**（接口只能追加方法，不能改已有方法的语义）。
/// 请求更高版本会被明确拒绝，而不是调用一个不存在的函数指针。
pub trait ExternService: Send + Sync + 'static {
    fn service(&self) -> &str;
    fn version(&self) -> u32;
    /// 方法名清单。用于诊断、能力发现，以及**调用前的可用性检查**。
    fn methods(&self) -> Vec<String>;
    /// 调用一个方法。
    ///
    /// `caller_meta` 是**调用点**上的合并结果 `d(k) ⊕ ι(k)`（M3，论文 §6.3）：
    /// 调用者的组件声明（`inject` 里的那条配置）被 context 携带的 intercept 元数据
    /// 覆盖后的样子；调用点没带任何元数据时为 `None`。
    /// **内核只传递它，策略留给提供者**——访问控制、审计、按调用方切换行为都在这层之上。
    fn call(
        &self,
        caller_meta: Option<Config>,
        method: String,
        args: Config,
    ) -> BoxFuture<Result<Config>>;

    /// 按**流**调用一个方法（ADR 0001 §5）：提供者把结果逐块推给 `sink`。
    ///
    /// 默认实现 = **长度 1 的流**：拿 [`ExternService::call`] 的返回值推一块然后收尾。
    /// 于是"不写流式代码的提供者"也能被流式调用——调用方不需要在两类提供者之间分叉，
    /// 跨 ABI 的提供者更是零成本（宿主那边同样只补了一件值）。
    fn call_stream(
        &self,
        caller_meta: Option<Config>,
        method: String,
        args: Config,
        sink: crate::StreamSink,
    ) -> BoxFuture<Result<()>> {
        let value = self.call(caller_meta, method, args);
        Box::pin(async move {
            let value = value.await?;
            match sink.push(value) {
                crate::PushOutcome::Sent => {
                    sink.close();
                    Ok(())
                }
                // 刚开流、窗口 ≥ 1：第一块推不进只可能是流已经被收场。
                _ => Err("流在提供者的返回值送达之前就结束了".into()),
            }
        })
    }
}

/// 服务接口在核心里面的装箱形态。
#[derive(Clone)]
pub struct IfaceRef(pub Arc<dyn ExternService>);

impl std::fmt::Debug for IfaceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IfaceRef({}@{})", self.0.service(), self.0.version())
    }
}

/// 接口版本不兼容时的错误。用独立类型而不是字符串，是为了让调用方能区分
/// "能力缺失"（可降级）与"真错误"（不可降级）——这是插件生态长期演进的关键分野。
#[derive(Debug, Clone)]
pub struct IfaceVersionMismatch {
    pub service: String,
    pub requested: u32,
    pub offered: u32,
}

impl std::fmt::Display for IfaceVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "service \"{}\" 请求接口版本 {}，但提供方最高只到 {}；\
             接口只能追加方法，请求更高版本意味着该插件是为更新的宿主/提供方编译的",
            self.service, self.requested, self.offered
        )
    }
}

impl std::error::Error for IfaceVersionMismatch {}

/// 一个**只能执行一次**的异步释放动作。
///
/// - `run` 消费 `self`：Rust 的所有权让"重复释放"变成编译期不可能，
///   而 JS 需要靠 `if (!runner.epoch) return` 这样的运行期标志来幂等。
/// - 内部 `Option` 让被丢弃的 disposer 可以安全地"永不执行"。
pub struct Disposable(Option<Box<dyn FnOnce() -> BoxFuture<()> + Send>>);

impl Disposable {
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce() -> BoxFuture<()> + Send + 'static,
    {
        Self(Some(Box::new(f)))
    }

    /// 纯同步释放动作的便捷构造。
    pub fn sync<F>(f: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new(move || {
            f();
            Box::pin(async {})
        })
    }

    pub fn noop() -> Self {
        Self::sync(|| {})
    }

    pub async fn run(self) {
        if let Some(f) = self.0 {
            f().await
        }
    }
}

impl std::fmt::Debug for Disposable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Disposable")
    }
}

/// 一个 effect 里注册了多个资源时的组合器。
///
/// 等价 JS 的 `ctx.effect(function* () { yield a; yield b })`，
/// 释放顺序**逆序（LIFO）且串行**——这是唯一被文档保证的顺序语义。
pub fn group(items: Vec<Disposable>) -> Disposable {
    Disposable::new(move || {
        Box::pin(async move {
            for item in items.into_iter().rev() {
                item.run().await;
            }
        })
    })
}
