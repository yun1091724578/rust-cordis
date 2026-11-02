//! cordis-sdk：插件作者用的**人体工学门面**（M6）。
//!
//! 三层在这里汇合，插件作者只需要**一条**依赖：
//!
//! | 路径 | 是什么 |
//! |---|---|
//! | `cordis::core` | 内核（服务容器 / epoch 生命周期 / 五种事件调度） |
//! | `cordis::abi` | 载体层（`cordis_abi::sdk` 原样保留，写动态库插件时用） |
//! | [`service`] / [`service_impl`] / [`events`] / [`plugin`] / [`config`] | 五个声明过程宏 |
//! | [`typed`] | 宏展开所依赖的**运行时胶水**（参数信封、载荷编解码、typed `next`） |
//!
//! # 五分钟速览
//!
//! ```ignore
//! #[cordis::service(name = "greeter", version = 1)]
//! pub trait Greeter {
//!     async fn greet(&self, who: String) -> Result<String, String>;
//! }
//!
//! #[derive(Clone)]
//! struct GreeterImpl;
//!
//! #[cordis::service_impl]
//! impl Greeter for GreeterImpl {
//!     async fn greet(&self, who: String) -> Result<String, String> {
//!         Ok(format!("hello, {who}"))
//!     }
//! }
//!
//! #[cordis::plugin(inject("greeter@1"))]
//! #[derive(Clone)]
//! struct Consumer;
//!
//! impl Consumer {
//!     async fn apply(&self, ctx: ::cordis::core::Ctx, _config: ::cordis::core::Config)
//!         -> ::cordis::core::Result<()>
//!     {
//!         let greeter = GreeterService::of(&ctx)?;     // typed 面（T1）
//!         let _ = greeter.greet("world".to_string()).await;
//!         // 动态库调用者走 JSON 面，落到**同一份实现**：
//!         //   ctx.call_iface("greeter@1", 1, "greet", json!(["world"]))
//!         Ok(())
//!     }
//! }
//! ```
//!
//! # 三条必须知道的规则（D2）
//!
//! 1. **键是 `<name>@<major>`**（§6.6 路线 1）：typed 服务的注册键带 major 版本，
//!    `inject` 里也要写这个键（`inject("greeter@1")`）。同事不同 major 的服务可以共存；
//!    **版本不匹配表现为"停在 Pending"**，不是"读到别的版本"。
//! 2. **typed 面只在 T1 有效**：`TypeId` 不跨编译单元稳定 ⇒ 只有进程内（同一编译单元）
//!    才能用 `GreeterService::of` 做类型化 downcast；T2/T3 一律走 JSON 面。
//!    同一个 key **不允许**同时有 T1 与 T2 两个提供者（内核不变量：一个键一个实现），
//!    需要同时服务两档时，由 T1 侧的实现持 `IfaceRef` 把 T2 的注册转发进来。
//! 3. **事件名逐字**：`#[event("tools/before-invoke")]` 产出的就是这一个字符串，
//!    宏绝不加前缀——ABI 侧的拦截名是硬编码的，改了就静默失配。
//!
//! 其余细节（`Clone` 契约、`ValidationError` 的形状、typed 事件的载荷形态）写在各个宏自己的文档里。
//!
//! # 编译期护栏
//!
//! `#[cordis::plugin]` 要求作者写固有 `async fn apply`。漏写时必须**编译失败**，
//! 不能回退到生成的 `Plugin::apply`（那会变成每次挂载递归一层，最终栈溢出）：
//!
//! ```compile_fail
//! #[cordis::plugin]
//! #[derive(Clone)]
//! struct MissingApply;
//! ```

pub use cordis_abi as abi;
pub use cordis_core as core;
pub use cordis_macros::{config, events, plugin, service, service_impl};
pub use serde;
pub use serde_json;

pub mod typed;
