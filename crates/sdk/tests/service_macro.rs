//! M6 红测试：typed 服务（G6）——`#[cordis::service]` / `#[cordis::service_impl]`。
//!
//! 契约（D2，四条必须先修的缺陷 / 五条行为断言）：
//! 1. `async fn` 非 dyn ⇒ 宏**脱糖**成 `fn(&'a self, …) -> ServiceFuture<'a, _>`；
//! 2. `dyn Trait` 需 `Send + Sync` ⇒ 宏**重写 trait 头**；
//! 3. `Dyn::new(Arc<dyn T>)` 会得到 `Arc<Arc<dyn T>>`（双重 Arc）⇒ 值形态是 `Iface<dyn T>`（`T: ?Sized`）；
//! 4. 类型不匹配被伪装成 `NoInject` ⇒ 现在是 `ResolveError::TypeMismatch`。
//!
//! 键的形态是 `<name>@<major>`（§6.6 路线 1）；typed 面（`of` + downcast）与
//! JSON 面（`adapter`，`Ctx::call_iface` 走它）必须指向**同一份实现**。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cordis::core::{App, Config, Ctx, Dyn, Iface, Inject, ResolveError, ServiceFuture};
use cordis::{service, service_impl};
use serde_json::json;

// ------------------------------------------------------------------ 服务声明

#[service(name = "greeter", version = 1)]
pub trait Greeter {
    /// 打个招呼（typed 面与 JSON 面都必须走到它）。
    async fn greet(&self, who: String) -> Result<String, String>;
    /// 累计调用次数：两条通道打到同一份实现的证据。
    async fn calls(&self) -> Result<u32, String>;
}

/// 同 key（`greeter@1`）、**不同类型**的第二个服务：证明类型不匹配不会被静默吞掉。
#[service(name = "greeter", version = 1)]
pub trait Impostor {
    async fn greet(&self, who: String) -> Result<String, String>;
}

/// 作者写普通 `async fn`：宏负责脱糖（含 `.await` 与 `?`）。
#[derive(Default, Clone)]
struct GreeterImpl {
    calls: Arc<AtomicU32>,
}

#[service_impl]
impl Greeter for GreeterImpl {
    async fn greet(&self, who: String) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let normalized = normalize(who).await?; // 既有 `.await` 也有 `?`
        Ok(format!("hello, {normalized}"))
    }

    async fn calls(&self) -> Result<u32, String> {
        Ok(self.calls.load(Ordering::SeqCst))
    }
}

#[service_impl]
impl Impostor for GreeterImpl {
    async fn greet(&self, who: String) -> Result<String, String> {
        Ok(format!("impostor, {who}"))
    }
}

async fn normalize(who: String) -> Result<String, String> {
    if who.trim().is_empty() {
        return Err("空名字".to_string());
    }
    Ok(who.trim().to_lowercase())
}

// ------------------------------------------------------------------ 夹具静态

static T1_GREETING: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T4_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T5_SEEN: Mutex<Option<String>> = Mutex::new(None);
static T9_SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn provider(
) -> impl Fn(Ctx, Config) -> cordis::core::BoxFuture<cordis::core::Result<()>> + Send + Sync + 'static
{
    |ctx: Ctx, _cfg| {
        Box::pin(async move {
            GreeterService::provide(&ctx, Arc::new(GreeterImpl::default()))?;
            Ok(())
        })
    }
}

// ------------------------------------------------------------------ 测试

/// M6-1：`#[cordis::service]` 产出键（`<name>@<major>`）与 typed 访问器，且端到端可 mount。
#[tokio::test]
async fn service_macro_generates_key_and_typed_accessor() {
    assert_eq!(GreeterService::NAME, "greeter");
    assert_eq!(GreeterService::MAJOR, 1);
    assert_eq!(
        GreeterService::KEY,
        "greeter@1",
        "键 = `<name>@<major>`（§6.6 路线 1）"
    );

    let app = App::new();
    app.plugin(
        cordis::core::plugin_fn("greeter-provider", Inject::default(), provider()),
        Config::Null,
    )
    .await
    .unwrap();

    let consumer = cordis::core::plugin_fn(
        "greeter-consumer",
        Inject::of(&["greeter@1"]),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let greeter = GreeterService::of(&ctx)?; // typed 访问器（`ResolveError` → `Error` 自动装箱）
                let greeting = greeter
                    .greet("World".into())
                    .await
                    .map_err(|err| -> cordis::core::Error { err.into() })?;
                T1_GREETING.lock().unwrap().push(greeting);
                Ok(())
            })
        },
    );
    app.plugin(consumer, Config::Null).await.unwrap();

    assert_eq!(T1_GREETING.lock().unwrap().as_slice(), ["hello, world"]);
}

/// M6-2：脱糖后的 trait 必须仍然 **object-safe**（`dyn Greeter`）且 `Send + Sync`。
#[tokio::test]
async fn async_service_methods_are_dyn_compatible() {
    // 编译即断言：`dyn Greeter` 可作为类型参数、可进 `Arc`、可进 `Dyn`。
    fn assert_send_sync<T: ?Sized + Send + Sync>() {}
    assert_send_sync::<dyn Greeter>();

    let typed: Arc<dyn Greeter> = Arc::new(GreeterImpl::default());
    let value = Dyn::new(Iface::<dyn Greeter>(typed.clone()));
    assert!(
        value.downcast::<Iface<dyn Greeter>>().is_some(),
        "`Iface<dyn Greeter>` 必须原样取回（不是 `Arc<Arc<dyn Greeter>>`）"
    );

    // 脱糖后的调用点：普通 `fn(&'a self, …) -> ServiceFuture<'a, _>`，借用随 future 走。
    let future: ServiceFuture<'_, Result<String, String>> = typed.greet("A".to_string());
    assert_eq!(future.await.unwrap(), "hello, a");
}

/// M6-3：作者在 impl 里照旧写 `async fn`（`.await` / `?` 全部照常）。
#[tokio::test]
async fn service_impl_macro_lets_authors_write_async_fn() {
    let typed: Arc<dyn Greeter> = Arc::new(GreeterImpl::default());
    assert_eq!(
        typed.greet("  MiXeD  ".to_string()).await.unwrap(),
        "hello, mixed"
    );
    assert_eq!(
        typed.greet("   ".to_string()).await.unwrap_err(),
        "空名字",
        "`?` 必须照常短路，错误类型原样穿过（E: Display）"
    );
    // 两次调用都进了实现（计数在空名字检查之前），所以是 2——顺带证明方法体真的跑在 impl 上。
    assert_eq!(typed.calls().await.unwrap(), 2);
}

/// M6-4：typed 访问器与 JSON 适配器打到**同一份实现**（D2 的 T1/T2 规则的基础）。
#[tokio::test]
async fn typed_accessor_and_json_adapter_dispatch_to_the_same_impl() {
    let app = App::new();
    app.plugin(
        cordis::core::plugin_fn("greeter-provider", Inject::default(), provider()),
        Config::Null,
    )
    .await
    .unwrap();

    let consumer = cordis::core::plugin_fn(
        "greeter-consumer",
        Inject::of(&["greeter@1"]),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let greeter = GreeterService::of(&ctx)?;
                let typed = greeter.greet("typed".into()).await.map_err(to_error)?;

                // JSON 面：和 T2 调用者走同一条路（`call_iface` → `ExternService::call`）。
                let json_out = ctx
                    .call_iface("greeter@1", 1, "greet", json!(["json"]))
                    .await?;
                let json_out = json_out.as_str().unwrap_or_default().to_string();

                // 版本协商：请求更高版本必须被明确拒绝，而不是静默调用。
                let mismatch = ctx
                    .call_iface("greeter@1", 2, "greet", json!(["x"]))
                    .await
                    .unwrap_err()
                    .to_string();

                // 两条通道各记一次 ⇒ 计数器必须是 2（同一个实现对象）。
                let calls = greeter.calls().await.map_err(to_error)?.to_string();

                T4_SEEN
                    .lock()
                    .unwrap()
                    .extend([typed, json_out, mismatch, calls]);
                Ok(())
            })
        },
    );
    app.plugin(consumer, Config::Null).await.unwrap();

    let seen = T4_SEEN.lock().unwrap().clone();
    assert_eq!(seen[0], "hello, typed");
    assert_eq!(seen[1], "hello, json", "JSON 面必须打到同一份实现");
    assert!(
        seen[2].contains("greeter@1") && seen[2].contains("接口版本"),
        "{seen:?}"
    );
    assert_eq!(seen[3], "2", "typed + JSON 各算一次");
}

/// M6-5：类型不匹配报 `TypeMismatch`（不再是伪装的 `NoInject`）；同 key 二次注册被拒。
#[tokio::test]
async fn typed_accessor_reports_type_mismatch_not_noinject() {
    let app = App::new();
    app.plugin(
        cordis::core::plugin_fn("greeter-provider", Inject::default(), provider()),
        Config::Null,
    )
    .await
    .unwrap();

    let consumer = cordis::core::plugin_fn(
        "impostor-consumer",
        Inject::of(&["greeter@1"]),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let outcome = match ImpostorService::of(&ctx) {
                    Ok(_) => "Ok(拿到了不该拿到的实现)".to_string(),
                    Err(ResolveError::TypeMismatch { name, expected }) => {
                        format!("TypeMismatch({name}, {expected})")
                    }
                    Err(other) => format!("{other:?}"),
                };
                // 内核不变量：一个 key 在实现域内唯一。
                let conflict = match GreeterService::provide(&ctx, Arc::new(GreeterImpl::default()))
                {
                    Ok(_) => "Ok(重复注册被接受了)".to_string(),
                    Err(err) => err.to_string(),
                };
                T5_SEEN
                    .lock()
                    .unwrap()
                    .replace(format!("{outcome} | {conflict}"));
                Ok(())
            })
        },
    );
    app.plugin(consumer, Config::Null).await.unwrap();

    let seen = T5_SEEN.lock().unwrap().clone().unwrap_or_default();
    assert!(
        seen.starts_with("TypeMismatch(greeter@1,"),
        "必须是 TypeMismatch：{seen}"
    );
    assert!(
        seen.contains("Iface<dyn"),
        "错误里要能看出期望的类型：{seen}"
    );
    assert!(!seen.contains("NoInject"), "绝不能伪装成 NoInject：{seen}");
    assert!(seen.contains("已被"), "同 key 二次注册必须被拒：{seen}");
}

/// M6 附加（G8）：JSON 面必须**校验参数信封的形状**——`[a1, …, aN]`，长度等于方法元数。
#[tokio::test]
async fn json_adapter_shape_checks_the_argument_envelope() {
    let adapter = GreeterService::adapter(Arc::new(GreeterImpl::default()));
    assert_eq!(adapter.service(), "greeter");
    assert_eq!(adapter.version(), 1);
    let mut methods = adapter.methods();
    methods.sort();
    assert_eq!(methods, vec!["calls".to_string(), "greet".to_string()]);

    let not_array = adapter
        .call(None, "greet".to_string(), json!({ "who": "x" }))
        .await
        .unwrap_err()
        .to_string();
    let wrong_arity = adapter
        .call(None, "greet".to_string(), json!(["a", "b"]))
        .await
        .unwrap_err()
        .to_string();
    let bad_arg = adapter
        .call(None, "greet".to_string(), json!([42]))
        .await
        .unwrap_err()
        .to_string();
    let ok = adapter
        .call(None, "greet".to_string(), json!(["z"]))
        .await
        .unwrap();

    T9_SEEN
        .lock()
        .unwrap()
        .extend([not_array, wrong_arity, bad_arg, ok.to_string()]);
    let seen = T9_SEEN.lock().unwrap().clone();
    assert!(
        seen[0].contains("信封"),
        "不是数组 ⇒ 明确说信封形状：{seen:?}"
    );
    assert!(
        seen[1].contains('1') && seen[1].contains('2'),
        "元数不符 ⇒ 报出期望与实到：{seen:?}"
    );
    assert!(
        seen[2].contains("who"),
        "参数解不出来 ⇒ 带上参数名：{seen:?}"
    );
    assert_eq!(seen[3], "\"hello, z\"");
}

fn to_error<E: std::fmt::Display>(err: E) -> cordis::core::Error {
    err.to_string().into()
}
