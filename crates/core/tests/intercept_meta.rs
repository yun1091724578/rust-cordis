//! M3：intercept 消费点（G3）——**提供者侧**读 `d(k) ⊕ ι(k)`（论文 §6.3，右偏）。
//!
//! 语义（SDK方案.md §4 M3）：
//! - 提供者在**被调用时**拿到一个合并后的实参：`merge(组件声明 d, context 携带的 ι)`；
//! - **右偏**：context 携带的覆盖组件声明；链条优先级 = 最近 intercept 层 > 祖先层 > 组件声明；
//! - `d` 来自调用者 fiber 的 `inject: { svc: <config> }` 声明，`ι` 来自 `ctx.intercept` 链；
//! - 两条通道：① 论文的（经 `ExternService::call` 的 `caller_meta` 到达提供者）；
//!   ② 引擎便利 `ctx.resolved_config()`（**明确不是** Def 26 的 d(k)，是消费侧自省面）。
//!
//! 策略留给提供者：内核只**传递**合并结果，不做访问控制裁决。

use cordis_core::{
    App, Config, Ctx, ExternService, FiberState, Inject, Plugin, ResolvedConfig, Result,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ------------------------------------------------------------------ 夹具

/// 回声提供者：把**提供者侧读到的**合并元数据原样回抛，断言因此能从调用点做。
struct Echo;

impl ExternService for Echo {
    fn service(&self) -> &str {
        "svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["hello".into()]
    }

    fn call(
        &self,
        caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> cordis_core::BoxFuture<Result<Config>> {
        Box::pin(async move { Ok(json!({ "meta": caller_meta })) })
    }
}

/// 记账提供者：把每次调用看到的 `caller_meta`（`None` ⇒ `null`）记进**本测试自己的**槽。
struct Recorder(&'static Mutex<Vec<Config>>);

impl ExternService for Recorder {
    fn service(&self) -> &str {
        "svc"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["hello".into()]
    }

    fn call(
        &self,
        caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> cordis_core::BoxFuture<Result<Config>> {
        // 先同步记账（锁立刻释放），再返回。
        self.0
            .lock()
            .unwrap()
            .push(caller_meta.unwrap_or(Config::Null));
        Box::pin(async move { Ok(Config::Null) })
    }
}

/// 提供者插件：激活时把 `service` 以 `"svc"` 之名暴露出去。
fn provider(name: &'static str, service: Arc<dyn ExternService>) -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(name, Inject::default(), move |ctx: Ctx, _config: Config| {
        let service = service.clone();
        Box::pin(async move {
            ctx.provide_iface("svc", service)?;
            Ok(())
        })
    })
}

/// 消费者插件：不做事，只把自己的 ctx 交出来——`inject` 声明挂在**它的**链上。
fn consumer(
    name: &'static str,
    slot: &'static Mutex<Option<Ctx>>,
    inject: Inject,
) -> Arc<dyn Plugin> {
    cordis_core::plugin_fn(name, inject, move |ctx: Ctx, _config: Config| {
        Box::pin(async move {
            *slot.lock().unwrap() = Some(ctx);
            Ok(())
        })
    })
}

fn captured(slot: &Mutex<Option<Ctx>>) -> Ctx {
    slot.lock().unwrap().clone().expect("消费者应已装载")
}

/// 从给定调用点调一次服务，取回提供者回抛的 `meta`。
async fn meta_at(ctx: &Ctx) -> Config {
    let answer = ctx
        .call_iface("svc", 1, "hello", json!({}))
        .await
        .expect("接口调用应成功");
    answer["meta"].clone()
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时：{what}");
}

// ------------------------------------------------------------------ 测试 1

static T1_CTX: Mutex<Option<Ctx>> = Mutex::new(None);

/// M3-1：提供者看到的是**声明与上下文两路合并**的结果（论文的单一实参形态）。
#[tokio::test]
async fn providers_see_the_merged_metadata_of_declaration_and_context() {
    let app = App::new();
    app.plugin(provider("t1-provider", Arc::new(Echo)), Config::Null)
        .await
        .unwrap();

    // 祖先层 = context 携带的 ι；消费者自己声明 d = { a: 1 }。
    let site = app.root.intercept("svc", json!({ "b": 2 }));
    let consumer = consumer(
        "t1-consumer",
        &T1_CTX,
        Inject(vec![("svc".into(), json!({ "a": 1 }))]),
    );
    site.plugin_and_await(consumer, Config::Null).await.unwrap();

    assert_eq!(
        meta_at(&captured(&T1_CTX)).await,
        json!({ "a": 1, "b": 2 }),
        "声明（a）与上下文（b）两路都要到达提供者"
    );
}

// ------------------------------------------------------------------ 测试 2

static T2_CTX: Mutex<Option<Ctx>> = Mutex::new(None);

/// M3-2：**右偏**——同键冲突时 context 携带的 ι 覆盖组件声明的 d（其余键保留）。
#[tokio::test]
async fn context_carried_metadata_overrides_the_component_declaration() {
    let app = App::new();
    app.plugin(provider("t2-provider", Arc::new(Echo)), Config::Null)
        .await
        .unwrap();

    let site = app.root.intercept("svc", json!({ "a": 2 }));
    let consumer = consumer(
        "t2-consumer",
        &T2_CTX,
        Inject(vec![("svc".into(), json!({ "a": 1, "b": 1 }))]),
    );
    site.plugin_and_await(consumer, Config::Null).await.unwrap();

    assert_eq!(
        meta_at(&captured(&T2_CTX)).await,
        json!({ "a": 2, "b": 1 }),
        "context 的 a=2 必须覆盖声明的 a=1；声明里独有的 b 保留"
    );
}

// ------------------------------------------------------------------ 测试 3

static T3_CTX: Mutex<Option<Ctx>> = Mutex::new(None);

/// M3-3：链条优先级 = **最近 intercept 层 > 祖先层 > 组件声明**。
#[tokio::test]
async fn nearest_intercept_layer_wins_over_an_ancestor() {
    let app = App::new();
    app.plugin(provider("t3-provider", Arc::new(Echo)), Config::Null)
        .await
        .unwrap();

    let ancestor = app.root.intercept("svc", json!({ "a": 1, "b": 1 }));
    let nearest = ancestor.intercept("svc", json!({ "b": 2 }));
    let consumer = consumer(
        "t3-consumer",
        &T3_CTX,
        Inject(vec![("svc".into(), json!({ "d": 0 }))]),
    );
    nearest
        .plugin_and_await(consumer, Config::Null)
        .await
        .unwrap();

    assert_eq!(
        meta_at(&captured(&T3_CTX)).await,
        json!({ "a": 1, "b": 2, "d": 0 }),
        "最近层 b=2 覆盖祖先 b=1；祖先的 a 与声明的 d 都要保留（声明垫底）"
    );
}

// ------------------------------------------------------------------ 测试 4

static T4_SEEN: Mutex<Vec<Config>> = Mutex::new(Vec::new());
static T4_CTX: Mutex<Option<Ctx>> = Mutex::new(None);
static T4_APPLIES: AtomicUsize = AtomicUsize::new(0);

/// M3-4：元数据**在调用时**计算——调用点再叠一层 intercept 不重挂、不重启任何 fiber。
#[tokio::test]
async fn changing_intercept_does_not_restart_the_fiber() {
    let app = App::new();
    let provider_fiber = app
        .plugin(
            provider("t4-provider", Arc::new(Recorder(&T4_SEEN))),
            Config::Null,
        )
        .await
        .unwrap();

    let site = app.root.intercept("svc", json!({ "a": 1 }));
    let consumer = cordis_core::plugin_fn(
        "t4-consumer",
        Inject::of(&["svc"]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                T4_APPLIES.fetch_add(1, Ordering::SeqCst);
                *T4_CTX.lock().unwrap() = Some(ctx);
                Ok(())
            })
        },
    );
    let consumer_fiber = site.plugin_and_await(consumer, Config::Null).await.unwrap();
    let (consumer_uid, provider_uid) = (consumer_fiber.uid(), provider_fiber.uid());

    let ctx = captured(&T4_CTX);
    ctx.call_iface("svc", 1, "hello", json!({})).await.unwrap();
    // 调用点上再叠一层：只对这一条链生效。
    let deeper = ctx.intercept("svc", json!({ "b": 2 }));
    deeper
        .call_iface("svc", 1, "hello", json!({}))
        .await
        .unwrap();

    assert_eq!(
        *T4_SEEN.lock().unwrap(),
        vec![json!({ "a": 1 }), json!({ "a": 1, "b": 2 })],
        "同一 fiber 的两条链各看各的：说明合并发生在**调用时**，不是挂载时"
    );
    assert_eq!(
        T4_APPLIES.load(Ordering::SeqCst),
        1,
        "叠加 intercept 不该重跑 apply"
    );
    assert_eq!(
        consumer_fiber.uid(),
        consumer_uid,
        "消费者不该被重启（uid 稳定）"
    );
    assert_eq!(provider_fiber.uid(), provider_uid, "提供者不该被重启");
    assert_eq!(consumer_fiber.state(), FiberState::Active);
}

// ------------------------------------------------------------------ 测试 5

static T5_SEEN: Mutex<Option<ResolvedConfig>> = Mutex::new(None);

/// M3-5：引擎便利通道——插件经 `ctx.resolved_config()` 拿到自己的已解析配置与
/// 每个已声明依赖在**调用点**上的合并结果。
#[tokio::test]
async fn service_config_reaches_the_plugin_through_resolved_config() {
    let app = App::new();
    app.plugin(provider("t5-provider", Arc::new(Echo)), Config::Null)
        .await
        .unwrap();

    let site = app.root.intercept("svc", json!({ "b": 2 }));
    let consumer = cordis_core::plugin_fn(
        "t5-consumer",
        Inject(vec![("svc".into(), json!({ "a": 1 }))]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                *T5_SEEN.lock().unwrap() = Some(ctx.resolved_config());
                Ok(())
            })
        },
    );
    site.plugin_and_await(consumer, json!({ "level": 3 }))
        .await
        .unwrap();

    let resolved = T5_SEEN.lock().unwrap().clone().expect("插件应已装载");
    assert_eq!(
        resolved.plugin,
        json!({ "level": 3 }),
        "plugin 是插件**实际拿到**的配置"
    );
    assert_eq!(
        resolved.services.get("svc"),
        Some(&json!({ "a": 1, "b": 2 })),
        "services 里是调用点合并结果（声明 ⊕ intercept 链，右偏）"
    );
    assert!(
        !resolved.services.contains_key("other"),
        "没声明过的服务名不该凭空出现"
    );
}

// ------------------------------------------------------------------ 测试 6

static T6_FIRST: Mutex<Vec<Config>> = Mutex::new(Vec::new());
static T6_SECOND: Mutex<Vec<Config>> = Mutex::new(Vec::new());
static T6_CTX: Mutex<Option<Ctx>> = Mutex::new(None);
static T6_APPLIES: AtomicUsize = AtomicUsize::new(0);

/// M3-6：元数据挂在**调用点的链**上，与提供者是谁无关——提供者换代后照旧送达，
/// 而且消费者不需要被重新装配（行级声明在 fiber 上，卸载/重装一趟依旧在）。
#[tokio::test]
async fn intercept_metadata_survives_a_provider_replacement() {
    let app = App::new();
    let first = app
        .plugin(
            provider("t6-first", Arc::new(Recorder(&T6_FIRST))),
            Config::Null,
        )
        .await
        .unwrap();

    let site = app.root.intercept("svc", json!({ "b": 2 }));
    let consumer = cordis_core::plugin_fn(
        "t6-consumer",
        Inject(vec![("svc".into(), json!({ "a": 1 }))]),
        |ctx: Ctx, _config: Config| {
            Box::pin(async move {
                T6_APPLIES.fetch_add(1, Ordering::SeqCst);
                *T6_CTX.lock().unwrap() = Some(ctx);
                Ok(())
            })
        },
    );
    let consumer_fiber = site.plugin_and_await(consumer, Config::Null).await.unwrap();
    let ctx = captured(&T6_CTX);
    ctx.call_iface("svc", 1, "hello", json!({})).await.unwrap();

    // 换提供者：同名服务不覆盖（`Store::provide` 会报冲突），所以先卸旧的再挂新的。
    // 依赖消失会让消费者卸载、回来再重装——这正是 cordis 的语义，不是重挂整棵树。
    first.dispose().await;
    let second = app
        .plugin(
            provider("t6-second", Arc::new(Recorder(&T6_SECOND))),
            Config::Null,
        )
        .await
        .unwrap();
    assert_eq!(second.state(), FiberState::Active, "新提供者应就绪");
    until("消费者重新激活", || {
        consumer_fiber.state() == FiberState::Active
    })
    .await;

    ctx.call_iface("svc", 1, "hello", json!({})).await.unwrap();

    assert_eq!(*T6_FIRST.lock().unwrap(), vec![json!({ "a": 1, "b": 2 })]);
    assert_eq!(
        *T6_SECOND.lock().unwrap(),
        vec![json!({ "a": 1, "b": 2 })],
        "新提供者看到同一份合并元数据：它来自调用点的链，不是提供者自己的状态"
    );
    assert_eq!(
        T6_APPLIES.load(Ordering::SeqCst),
        2,
        "依赖消失 ⇒ 卸载；回来 ⇒ 重装（恰好两次 apply）"
    );
    assert_eq!(consumer_fiber.state(), FiberState::Active);
}
