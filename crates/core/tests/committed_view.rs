//! `committed` 视图（论文 Table 2 的 ω）的行为规格。
//!
//! # 参考实现的精确语义
//! 属性访问沿 fiber 链读的是 `fiber.committed` 快照，**不是**实时解析表
//! （`reflect.ts:86` `const impl = fiber.store?.[prop]`，而 `fiber._store` 是实时表）。
//!
//! 快照的**两处写入**：
//! 1. `_reload` 开头：`this.store = { ...this._store }`（`fiber.ts:421`）——
//!    激活那一刻的依赖解析被冻结；
//! 2. `provide`：`this.ctx.fiber.store![name] = impl`（`reflect.ts:193`）——
//!    自己提供的服务要能在自己的快照里读到。
//!
//! 快照的**两处清除**：
//! 1. `_unload` 末尾：`this.store = undefined`（`fiber.ts:459`，在所有 disposer 跑完之后）；
//! 2. `provide` 的 disposer 末尾：`delete this.ctx.fiber.store![name]`（`reflect.ts:202`，
//!    紧邻的注释是 *"ensure self access before dependencies cleanup"*）——
//!    即**先让依赖者拆完，再让自己拆**。
//!
//! 这几条合起来给出本文件的规格：**一个组件在整个生命周期内（含拆除过程）都能读到
//! 它激活时解析到的依赖；只有拆除彻底结束后才读不到。**
//! 论文 p.61：*"Reading the view rather than the store is what Theorem 63 relies on."*

use cordis_core::{App, Config, Ctx, Dyn, Inject};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// 读一个字符串服务，把结果（成功值或错误）都变成可比较的字符串。
fn read_string(ctx: &Ctx, name: &str) -> String {
    match ctx.require(name) {
        Ok(value) => match value.downcast::<String>() {
            Some(text) => format!("Ok({})", *text),
            None => "Err(type mismatch)".to_string(),
        },
        Err(err) => format!("Err({err})"),
    }
}

/// 提交视图必须保留**域身份**：同一个 fiber 在默认域与派生 isolate 域
/// 提供同名服务时，`require()` 不能读到另一个域的实现，也不能回落到
/// 一个从未注册过该服务的域。
#[tokio::test]
async fn committed_view_is_scoped_by_realm() {
    let app = App::new();
    let provider = cordis_core::plugin_fn("realm-view", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("svc", Dyn::new(String::from("default")))?;
            let isolated = ctx.isolate("svc", Some(7));
            isolated.provide("svc", Dyn::new(String::from("isolated")))?;
            Ok(())
        })
    });
    let fiber = app.plugin(provider, Config::Null).await.unwrap();
    let base = fiber.ctx();

    let get_string = |ctx: &Ctx| {
        ctx.get("svc")
            .and_then(|value| value.downcast::<String>())
            .map(|value| (*value).clone())
    };

    // 默认域：get() 与 require() 必须看到同一份实现。
    assert_eq!(get_string(&base).as_deref(), Some("default"));
    assert_eq!(read_string(&base, "svc"), "Ok(default)");

    // 派生域 1：两路同样都看到隔离实现。
    let isolated = base.isolate("svc", Some(7));
    assert_eq!(get_string(&isolated).as_deref(), Some("isolated"));
    assert_eq!(read_string(&isolated, "svc"), "Ok(isolated)");

    // 派生域 2：没有注册过这个服务，不得回落到 7 或默认域。
    let stranger = base.isolate("svc", Some(8));
    assert_eq!(get_string(&stranger), None, "get() 不得跨域回退");
    let error = read_string(&stranger, "svc");
    assert!(
        !error.contains("default") && !error.contains("isolated"),
        "require() 不得返回其它域的值：{error}"
    );
}

/// **消费方**在**自己的拆除过程**中读那个正在离开的依赖。
/// 提供者离开 → 消费方被驱动去拆除；拆除期间它必须仍能读到该依赖。
/// 缺失 committed 视图时这里拿到的是
/// `Err(cannot get required service "svc" in inactive context)`。
#[tokio::test]
async fn consumer_can_read_departing_service_during_its_own_teardown() {
    let app = App::new();
    let observed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let provider = cordis_core::plugin_fn("provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("svc", Dyn::new(String::from("still-here")))?;
            Ok(())
        })
    });

    let slot = observed.clone();
    let consumer =
        cordis_core::plugin_fn("consumer", Inject::of(&["svc"]), move |ctx: Ctx, _cfg| {
            let slot = slot.clone();
            Box::pin(async move {
                // 把"读依赖"放在 effect 的 disposer 里——即组件自己的拆除阶段。
                ctx.effect(move |ctx| {
                    cordis_core::Disposable::new(move || {
                        Box::pin(async move {
                            *slot.lock().unwrap() = Some(read_string(&ctx, "svc"));
                        })
                    })
                });
                Ok(())
            })
        });

    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    let consumer_fiber = app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Active);

    provider_fiber.dispose().await;
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Pending);

    let seen = observed.lock().unwrap().clone().expect("disposer 应当跑过");
    assert_eq!(
        seen, "Ok(still-here)",
        "拆除期间应当仍能读到正在离开的依赖（committed view）"
    );
}

/// **提供方**读**自己提供的**服务的两个时刻。
///
/// # 时刻一：激活期间
/// 这条是 `reflect.ts:193`（`this.ctx.fiber.store![name] = impl`）的规格。
/// 快照是在 `apply` **之前**拍的，所以插件在 `apply` 里 `provide` 的东西
/// **必须额外写进快照**，否则它自己立刻就读不到——这正是那一行存在的原因。
///
/// # 时刻二：自己的拆除过程中
/// 规格由 disposable 的 LIFO 顺序决定。注册顺序 `[late, provide, early]`
/// 逆序执行得到 `early → provide 的 disposer → late`，于是：
/// - `early` 跑时 provide 的 disposer 还没跑 ⇒ **读得到**；
/// - `late` 跑时它已经跑完（`reflect.ts:202` 的 `delete`）⇒ **读不到**。
///
/// JS 那边这三个 disposer 是 `Promise.all` 并发跑的，所以"读得到/读不到"是竞态；
/// Rust 侧串行 + LIFO 把它变成了确定行为——这里就把这个确定行为钉住。
#[tokio::test]
async fn provider_can_read_its_own_service_while_active_and_until_its_provide_effect_is_released() {
    let app = App::new();
    let read_late: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let read_early: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let late_slot = read_late.clone();
    let early_slot = read_early.clone();
    let provider = cordis_core::plugin_fn("provider", Inject::default(), move |ctx: Ctx, _cfg| {
        let late_slot = late_slot.clone();
        let early_slot = early_slot.clone();
        Box::pin(async move {
            // ① 注册最早 → 执行最晚（在 provide 的 disposer 之后）。
            ctx.effect(move |ctx| {
                cordis_core::Disposable::new(move || {
                    Box::pin(async move {
                        *late_slot.lock().unwrap() = Some(read_string(&ctx, "svc"));
                    })
                })
            });

            // ② 提供自己。
            ctx.provide("svc", Dyn::new(String::from("mine")))?;

            // ③ 注册最晚 → 执行最先（在 provide 的 disposer 之前）。
            ctx.effect(move |ctx| {
                cordis_core::Disposable::new(move || {
                    Box::pin(async move {
                        *early_slot.lock().unwrap() = Some(read_string(&ctx, "svc"));
                    })
                })
            });

            Ok(())
        })
    });

    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    assert_eq!(provider_fiber.state(), cordis_core::FiberState::Active);

    // 时刻一：激活期间必须能读到自己提供的东西。
    assert_eq!(
        read_string(&provider_fiber.ctx(), "svc"),
        "Ok(mine)",
        "激活期间应当能读到自己提供的服务（reflect.ts:193）"
    );

    provider_fiber.dispose().await;

    // 时刻二：provide 的 disposer 之前 → 读得到。
    assert_eq!(
        read_early
            .lock()
            .unwrap()
            .clone()
            .expect("early disposer 应当跑过"),
        "Ok(mine)",
        "provide 的 disposer 还没跑时，应当仍能读到自己提供的服务"
    );
    // 时刻二：provide 的 disposer 之后 → 已经摘掉（reflect.ts:202 的 delete）。
    assert_eq!(
        read_late
            .lock()
            .unwrap()
            .clone()
            .expect("late disposer 应当跑过"),
        "Err(cannot get property \"svc\" without inject)",
        "provide 的 disposer 跑完后，自己提供的服务应当已从自己的视图里摘掉"
    );
}

/// 快照必须在**每次激活时重拍**，而不是"装一次就永远缓存"。
///
/// 提供者重启后换了新值，消费方重新激活时必须读到**新**值。
/// 这条同时给"提供者重启 → 依赖者重载"的整条链路做回归。
#[tokio::test]
async fn committed_view_is_resnapshotted_on_every_activation() {
    let app = App::new();
    let generation = Arc::new(AtomicU32::new(0));
    let reads: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));

    let gen = generation.clone();
    let provider = cordis_core::plugin_fn("provider", Inject::default(), move |ctx: Ctx, _cfg| {
        let value = gen.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            ctx.provide("svc", Dyn::new(value))?;
            Ok(())
        })
    });

    let seen = reads.clone();
    let consumer =
        cordis_core::plugin_fn("consumer", Inject::of(&["svc"]), move |ctx: Ctx, _cfg| {
            let seen = seen.clone();
            Box::pin(async move {
                let value = match ctx.require("svc") {
                    Ok(dyn_value) => match dyn_value.downcast::<u32>() {
                        Some(v) => *v,
                        None => return Err("svc is not a u32".into()),
                    },
                    Err(err) => return Err(err.to_string().into()),
                };
                seen.lock().unwrap().push(value);
                Ok(())
            })
        });

    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    let consumer_fiber = app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(*reads.lock().unwrap(), vec![1]);

    // 提供者就地重载：快照必须重拍，消费方必须读到新一代的值。
    provider_fiber.restart().await.unwrap();
    consumer_fiber.settle().await;
    assert_eq!(
        *reads.lock().unwrap(),
        vec![1, 2],
        "提供者重启后，消费方应当重新激活并读到新值"
    );
}
