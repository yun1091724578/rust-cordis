//! cordis-macros：插件声明的**过程宏**（M6）。
//!
//! 五个宏，与 `cordis-sdk` 一起构成插件作者的完整表面：
//!
//! | 宏 | 生成物 |
//! |---|---|
//! | [`macro@service`] | 脱糖后的 trait（`async fn` → 借用盒 future）+ 伴生类型 `<Trait>Service`（`NAME`/`MAJOR`/**`KEY`**/`of`/`provide`/`adapter`） |
//! | [`macro@service_impl`] | 脱糖后的 `impl`（作者照旧写 `async fn`） |
//! | [`macro@events`] | 事件名常量（**逐字**）+ 按模式生成的派发/监听入口 |
//! | [`macro@plugin`] | `impl Plugin`（name / inject / provision / volatile / 配置校验 + `apply` 钩子） |
//! | [`macro@config`] | serde 派生 + `parse` / `to_config`（挂载期校验的输入） |
//!
//! 展开里出现的路径一律是 `::cordis::…`（sdk 门面），所以**用户 crate 只需要一条依赖**。
//! 这一层只做语法；一切跨边界行为（JSON 信封、载荷形态、错误映射）在
//! `cordis::typed::{service, events}` 里，只有一处要审。

extern crate proc_macro;

use proc_macro::TokenStream;

mod attr;
mod config;
mod events;
mod plugin;
mod service;

/// 展开结果 → 编译产物：出错时给 `compile_error!`（**编译期报错，不静默降级**，D2）。
fn finish(result: syn::Result<proc_macro2::TokenStream>) -> TokenStream {
    match result {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// 声明一个 **T1 typed 服务**（D2）。
///
/// ```ignore
/// #[cordis::service(name = "greeter", version = 1)]
/// pub trait Greeter {
///     async fn greet(&self, who: String) -> Result<String, String>;
/// }
/// ```
///
/// 生成的伴生类型是 `<Trait>Service`，它的四个公开面：
/// - `NAME` / `MAJOR` / `KEY`（`KEY = "<name>@<major>"`，§6.6 路线 1）；
/// - `of(ctx) -> Result<Arc<dyn Trait>, ResolveError>`：typed 访问器（**T1 专用**）；
/// - `provide(ctx, Arc<T>)`：注册（typed 面 + JSON 面指向同一份实现）；
/// - `adapter(Arc<dyn Trait>)`：JSON 面适配器（T2/T3 调用者用）。
///
/// **写死的规则**：
/// - 方法必须是 `async fn`、接收者必须是 `&self`、不能有泛型/`impl Trait` 参数（否则不是
///   object-safe 的服务接口）——违反时**编译期报错**，不静默降级；
/// - 参数与返回值必须能走 JSON（`Serialize` / `DeserializeOwned`）：typed 面只在 T1 类型安全；
/// - 事件与服务的名字**照字面用**，宏不加任何前缀。
#[proc_macro_attribute]
pub fn service(args: TokenStream, input: TokenStream) -> TokenStream {
    finish(service::service(args.into(), input.into()))
}

/// 实现一个 `#[cordis::service]` 声明过的 trait：作者照旧写 `async fn`。
///
/// ```ignore
/// #[cordis::service_impl]
/// impl Greeter for GreeterImpl {
///     async fn greet(&self, who: String) -> Result<String, String> {
///         Ok(format!("hello, {who}"))
///     }
/// }
/// ```
///
/// 与 trait 侧同一套脱糖规则（借用盒 future），否则作者写的 `async fn` impl 会和脱糖后的
/// trait 类型对不上——"插件作者写普通 Rust"就不成立（D2）。
#[proc_macro_attribute]
pub fn service_impl(args: TokenStream, input: TokenStream) -> TokenStream {
    finish(service::service_impl(args.into(), input.into()))
}

/// 声明一组 **typed 事件**（G12）。
///
/// ```ignore
/// #[cordis::events]
/// pub enum AgentEvents {
///     #[event("agent/pre-step")]
///     PreStep(StepReport),
///     #[event("tools/before-invoke", mode = "waterfall")]
///     BeforeInvoke(Invoke),
/// }
/// ```
///
/// 每个变体产出一个**逐字**的事件名常量（变体名的 SCREAMING_SNAKE）+ 三样入口：
/// `pre_step`（派发）/ `on_pre_step`（typed 监听器）/ 常量名本身。
/// 载荷的线上形态是 `Dyn::new(ExternValue(json))`，与宿主 `json_to_args` 一致——
/// 于是 typed 监听器与动态库监听器看到同一份载荷（A9）。
///
/// `mode` 取 `emit`（默认）/ `parallel` / `serial` / `bail` / `waterfall`；
/// `waterfall` 的派发入口多一个 `tail`，typed 监听器多一个 [`typed::events::TypedNext`]。
///
/// [`typed::events::TypedNext`]: ../cordis/typed/events/struct.TypedNext.html
#[proc_macro_attribute]
pub fn events(args: TokenStream, input: TokenStream) -> TokenStream {
    finish(events::events(args.into(), input.into()))
}

/// 声明一个插件：生成 `impl Plugin`，`apply` 由作者写成普通 `async fn`。
///
/// ```ignore
/// #[cordis::plugin(version = "1.2.3", inject("greeter@1"), provision("agentLoop@1"), volatile("cache.dir"), config(LoopConfig))]
/// #[derive(Clone)]
/// pub struct AgentLoop { /* … */ }
///
/// impl AgentLoop {
///     async fn apply(&self, ctx: Cordis Ctx, config: Config) -> Result<()> { … }
/// }
/// ```
///
/// **参数**（都可省略）：
/// - `name = "…"`：显示名（默认 = 类型名）；`key = "…"`：注册表身份键（默认 = 显示名）；
/// - `version = "…"`：可选诊断/身份元数据；不是 ABI 版本，不参与 ABI 匹配；
/// - `inject(a, "b@1")`：依赖声明。裸标识符当**字符串**用（`inject(tools)` = `"tools"`）；
///   要命名空间就写字面量（`inject("tools@1")`）——宏不替你猜 major；
/// - `provision(a, "b@1")`、`volatile("cache.dir", data)`：同一套字面规则；
/// - `config(Type)`：挂载期用 `Type::parse` 校验配置（配合 [`macro@config`]）。
///
/// **两条契约**：
/// 1. **类型必须 `Clone`**：`Plugin::apply` 返回 `'static` future，借不了 `&self`，
///    宏在调用前克隆一份。共享状态请放 `Arc`（与既有示例/夹具的写法一致）。
/// 2. **`apply` 是钩子名**：`impl` 里写 `async fn apply(&self, ctx: Ctx, config: Config) -> Result<()>`。
#[proc_macro_attribute]
pub fn plugin(args: TokenStream, input: TokenStream) -> TokenStream {
    finish(plugin::plugin(args.into(), input.into()))
}

/// 声明一个 **typed 配置**：serde 派生 + `parse` / `to_config`。
///
/// ```ignore
/// #[cordis::config]
/// #[derive(Clone, Debug)]
/// pub struct LoopConfig { pub concurrency: u32, pub prompt: String }
/// ```
///
/// `parse` 把 `Config`（JSON）解成类型化配置，失败给 `ValidationError`（内核的
/// `invalid config:` 形状）——配合 `#[cordis::plugin(config(LoopConfig))]`，
/// 配置错误在**挂载期**就被报出来，而不是变成 apply 里的运行期惊喜。
#[proc_macro_attribute]
pub fn config(args: TokenStream, input: TokenStream) -> TokenStream {
    finish(config::config(args.into(), input.into()))
}
