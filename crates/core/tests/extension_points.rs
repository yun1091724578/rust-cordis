//! M3-3：`internal/get` / `internal/set` 的**同步钩子链**。
//!
//! 上游形态（`reflect.ts:82` / `:120`）：两者都在 **Proxy 陷阱**里，
//! 是 waterfall，实参 `(ctx, name[, value], error, next)`；钩子可以接管/改写这次读写。
//! 上游的显式方法 `ctx.get(name)` / `ctx.set(name, v)`（`reflect.ts:152/164`）
//! **绕过**这两个 waterfall——只有"属性访问"才触发。
//!
//! Rust 没有 Proxy：属性读/写与显式方法合一，因此本仓把两条陷阱路径映射到
//! **注入检查的读**（`Ctx::require`）与**写**（`Ctx::set`）上（形态偏离见附录 A）；
//! `try_get`（宽容读）不触发钩子，对应上游"显式 `ctx.get` 绕过 waterfall"。

use cordis_core::{App, Config, Ctx, Dyn, Inject, Mode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 轮询等待：`internal/service` 是 **spawn** 派发的（不能在 provide/移除的同步路径里 await），
/// 所以断言前必须给事件一点时间。
async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时：{what}");
}

/// 一个只注册 `event` 监听器的插件，把收到的事件名收集起来。
fn listener_plugin(
    event: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
) -> Arc<dyn cordis_core::Plugin> {
    cordis_core::plugin_fn("listener", Inject::default(), move |ctx: Ctx, _cfg| {
        let seen = seen.clone();
        Box::pin(async move {
            ctx.on(event, move |call: cordis_core::Call| {
                let seen = seen.clone();
                Box::pin(async move {
                    let name = call.args[0].downcast::<String>().expect("事件名");
                    seen.lock().unwrap().push((*name).clone());
                    None
                })
            });
            Ok(())
        })
    })
}

/// 一个什么都不做的插件（提供者宿主）。
fn empty_plugin(name: &'static str) -> Arc<dyn cordis_core::Plugin> {
    cordis_core::plugin_fn(name, Inject::default(), |_ctx: Ctx, _cfg| {
        Box::pin(async { Ok(()) })
    })
}

/// M3-3（红→绿）：读钩子可以**接管**一次读取——查表之前就给出答案。
#[tokio::test]
async fn get_hook_can_answer_instead_of_the_store() {
    let app = App::new();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed = Arc::new(Mutex::new(None::<String>));

    {
        let seen = seen.clone();
        app.store
            .clone()
            .hook_get(&app.root, move |_ctx: &Ctx, name: &str| {
                seen.lock().unwrap().push(name.to_string());
                (name == "virtual").then(|| Dyn::new("from-hook".to_string()))
            });
    }

    let observed_in = observed.clone();
    let plugin = cordis_core::plugin_fn(
        "virtual-reader",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let observed = observed_in.clone();
            Box::pin(async move {
                // 什么都没 provide：正常情况下 require 会失败（走 inject 检查）。
                let value = ctx.require("virtual").expect("读钩子应当接管这次读取");
                *observed.lock().unwrap() = Some((*value.downcast::<String>().unwrap()).clone());
                // 其他名字：钩子返回 None ⇒ 正常解析（这里必然失败，但错误来自内核）。
                let error = ctx.require("absent").expect_err("absent 没有被 provide");
                assert!(
                    error.to_string().contains("cannot get"),
                    "错误必须来自内核解析：{error}"
                );
                Ok(())
            })
        },
    );
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.error(), None, "插件应当装载成功");

    assert_eq!(
        observed.lock().unwrap().clone(),
        Some("from-hook".to_string())
    );
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec!["virtual".to_string(), "absent".to_string()],
        "钩子必须按名字逐个被咨询"
    );
}

/// M3-3（红→绿）：读钩子返回 `None` ⇒ **完全落回正常解析**（不改变任何行为）。
#[tokio::test]
async fn get_hook_falls_through_to_normal_resolution() {
    let app = App::new();
    let calls = Arc::new(AtomicUsize::new(0));

    {
        let calls = calls.clone();
        app.store
            .clone()
            .hook_get(&app.root, move |_ctx: &Ctx, _name: &str| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            });
    }

    let provider = cordis_core::plugin_fn("get-provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("svc", Dyn::new(7u32))?;
            Ok(())
        })
    });
    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;

    let seen = Arc::new(Mutex::new(None::<u32>));
    let seen_in = seen.clone();
    let consumer = cordis_core::plugin_fn(
        "get-consumer",
        Inject::of(&["svc"]),
        move |ctx: Ctx, _cfg| {
            let seen = seen_in.clone();
            Box::pin(async move {
                let value = ctx.require_as::<u32>("svc").expect("正常解析必须成功");
                *seen.lock().unwrap() = Some(*value);
                Ok(())
            })
        },
    );
    let consumer_fiber = app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;

    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Active);
    assert_eq!(seen.lock().unwrap().clone(), Some(7));
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "钩子必须被咨询过（返回 None 不改变行为）"
    );
}

/// M3-3（红→绿）：写钩子可以**否决**（`Some(Err)`）、**接管**（`Some(Ok)`）或放行（`None`）。
#[tokio::test]
async fn set_hook_can_take_over_or_veto_a_write() {
    let app = App::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let outcomes = Arc::new(Mutex::new(Vec::<String>::new()));

    {
        let calls = calls.clone();
        app.store
            .clone()
            .hook_set(&app.root, move |_ctx: &Ctx, name: &str, _value: &Dyn| {
                calls.fetch_add(1, Ordering::SeqCst);
                match name {
                    "vetoed" => Some(Err("被写钩子拒绝".to_string().into())),
                    "taken" => Some(Ok(())), // 接管：内核的 set 不再执行
                    _ => None,
                }
            });
    }

    let outcomes_in = outcomes.clone();
    let plugin = cordis_core::plugin_fn("writer", Inject::default(), move |ctx: Ctx, _cfg| {
        let outcomes = outcomes_in.clone();
        Box::pin(async move {
            for name in ["vetoed", "taken", "plain"] {
                ctx.provide(name, Dyn::new(1u32))?;
            }
            let mut record = Vec::new();
            for name in ["vetoed", "taken", "plain"] {
                match ctx.set(name, Dyn::new(2u32)) {
                    Ok(()) => record.push(format!("{name}:Ok")),
                    Err(err) => record.push(format!("{name}:Err({err})")),
                }
            }
            record.push(format!(
                "values={:?}",
                ["vetoed", "taken", "plain"]
                    .iter()
                    .map(|name| ctx
                        .require_as::<u32>(name)
                        .map(|value| *value)
                        .unwrap_or_default())
                    .collect::<Vec<u32>>()
            ));
            *outcomes.lock().unwrap() = record;
            Ok(())
        })
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.error(), None, "插件应当装载成功");

    let outcomes = outcomes.lock().unwrap().clone();
    assert_eq!(
        outcomes[0], "vetoed:Err(被写钩子拒绝)",
        "否决必须原样传出错误：{outcomes:?}"
    );
    assert_eq!(outcomes[1], "taken:Ok", "接管必须报成功：{outcomes:?}");
    assert_eq!(outcomes[2], "plain:Ok", "放行必须走正常路径：{outcomes:?}");
    assert_eq!(
        outcomes[3], "values=[1, 1, 2]",
        "被否决与被接管的写入都不得落内核；放行的才生效：{outcomes:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3, "每次 set 都必须被咨询一次");
}

/// M3-4（红→绿）：`internal/service` 在 provide/移除时广播——
/// **提供**带值、**移除**带 `None`（对齐上游 `reflect.ts:223-227`：
/// 值取"当下"的实现，`_getImpl(name, false)?.value`，刚删完就是 `undefined`）。
///
/// 注意门：apply 期（提供者还没 ACTIVE）的 provide **不广播**（上游 `reflect.ts:194` 同门）；
/// 本测试在 ACTIVE 期提供，正好走广播路径。
type ServiceEvents = Arc<Mutex<Vec<(String, Option<u32>)>>>;

#[tokio::test]
async fn internal_service_carries_value_and_none_on_removal() {
    let app = App::new();
    let seen: ServiceEvents = Arc::new(Mutex::new(Vec::new()));

    {
        let seen = seen.clone();
        app.root
            .on("internal/service", move |call: cordis_core::Call| {
                let seen = seen.clone();
                Box::pin(async move {
                    let name = (*call.args[0]
                        .downcast::<String>()
                        .expect("第一个实参是服务名"))
                    .clone();
                    // 第二实参恒存在：`Some(值)` = 服务可用；`None` = 刚被移除（"value 不可依赖"）。
                    let value = match call.args[1].downcast::<Option<Dyn>>() {
                        Some(slot) => match &*slot {
                            Some(value) => value.downcast::<u32>().map(|v| *v),
                            None => None,
                        },
                        None => None,
                    };
                    seen.lock().unwrap().push((name, value));
                    None
                })
            });
    }

    let fiber = app.root.plugin(empty_plugin("svc-host"), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);

    fiber.ctx().provide("svc", Dyn::new(5u32)).unwrap();
    wait_for("提供事件", || !seen.lock().unwrap().is_empty()).await;

    // 卸载 ⇒ 提供者的 disposer **先摘表再通知**，所以事件里的值已经是 None。
    fiber.dispose().await;
    wait_for("移除事件", || seen.lock().unwrap().len() >= 2).await;

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![("svc".to_string(), Some(5)), ("svc".to_string(), None)],
        "恰好两次且顺序固定"
    );
}

/// M3-4（红→绿）：`internal/service` 携带 **realm filter**——只有与提供者同域的注册者收得到
/// （这就是 P2-4 里"死代码" `realm_filter` 的消费点）；
/// 另验证 `fiber_filter` 按**生命周期子树**放行监听者。
#[tokio::test]
async fn internal_service_respects_realm_filter_and_fiber_subtree_filter() {
    let app = App::new();
    let default_seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let realm_seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    app.root.on("internal/service", {
        let seen = default_seen.clone();
        move |call: cordis_core::Call| {
            let seen = seen.clone();
            Box::pin(async move {
                let name = call.args[0].downcast::<String>().expect("服务名");
                seen.lock().unwrap().push((*name).clone());
                None
            })
        }
    });

    // 域 A：一个监听者插件常驻在里面。
    let realm = app.root.isolate("svc", None);
    let realm_listener = realm.plugin(
        listener_plugin("internal/service", realm_seen.clone()),
        Config::Null,
    );
    realm_listener.settle().await;

    // 默认域提供者：ACTIVE 期提供 ⇒ 事件只该到默认域。
    let default_provider = app
        .root
        .plugin(empty_plugin("default-provider"), Config::Null);
    default_provider.settle().await;
    default_provider
        .ctx()
        .provide("svc", Dyn::new(1u32))
        .unwrap();
    wait_for("默认域事件", || {
        !default_seen.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        realm_seen.lock().unwrap().is_empty(),
        "跨域不得投递（realm_filter 的消费点）：{:?}",
        realm_seen.lock().unwrap()
    );

    // 域 A 提供者：事件只该到域 A。
    let realm_provider = realm.plugin(empty_plugin("realm-provider"), Config::Null);
    realm_provider.settle().await;
    realm_provider.ctx().provide("svc", Dyn::new(2u32)).unwrap();
    wait_for("域 A 事件", || !realm_seen.lock().unwrap().is_empty()).await;
    assert_eq!(realm_seen.lock().unwrap().clone(), vec!["svc".to_string()]);
    assert_eq!(
        default_seen.lock().unwrap().clone(),
        vec!["svc".to_string()],
        "默认域监听者不该看到域 A 的事件"
    );

    // —— fiber_filter：只放行"注册者在这条子树里（含锚点自己）"的监听者。
    let anchor_seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let other_seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let anchor = app.root.plugin(
        listener_plugin("fiber-filtered", anchor_seen.clone()),
        Config::Null,
    );
    anchor.settle().await;
    let other = app.root.plugin(
        listener_plugin("fiber-filtered", other_seen.clone()),
        Config::Null,
    );
    other.settle().await;

    let filter = app.store.fiber_filter(&anchor);
    app.store
        .clone()
        .dispatch(
            Mode::Emit,
            "fiber-filtered",
            &anchor.ctx(),
            vec![Dyn::new("ping".to_string())],
            None,
            filter.as_ref(),
        )
        .await;

    assert_eq!(
        anchor_seen.lock().unwrap().clone(),
        vec!["ping".to_string()],
        "锚点子树内的注册必须收到"
    );
    assert!(other_seen.lock().unwrap().is_empty(), "另一条子树不得收到");
    let _ = other;
}

/// M3-5（红→绿）：`ctx.once` **恰好触发一次**，触发后自动摘除；
/// 之后再派发不触发、再手动摘除也是安全空操作（对应 `events.ts:173-179`：
/// 包装器**先 dispose 再调用**原监听器）。
#[tokio::test]
async fn once_fires_exactly_once_and_disposes_idempotently() {
    let app = App::new();
    let calls = Arc::new(AtomicUsize::new(0));

    let handle = {
        let calls = calls.clone();
        app.root.once("ping", move |_call: cordis_core::Call| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            })
        })
    };

    app.root.emit("ping", vec![]).await;
    app.root.emit("ping", vec![]).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "第二次派发不得再触发");

    // 触发已经自动摘除；再手动 run 是安全空操作（幂等），随后派发依旧不触发。
    handle.run().await;
    app.root.emit("ping", vec![]).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "摘除之后不得再触发");

    // 提前手动摘除：从未触发过的一次性监听器同样不再触发。
    let manual = Arc::new(AtomicUsize::new(0));
    let handle = {
        let manual = manual.clone();
        app.root.once("pong", move |_call: cordis_core::Call| {
            let manual = manual.clone();
            Box::pin(async move {
                manual.fetch_add(1, Ordering::SeqCst);
                None
            })
        })
    };
    handle.run().await;
    app.root.emit("pong", vec![]).await;
    assert_eq!(manual.load(Ordering::SeqCst), 0, "提前摘除后不得触发");
}

/// 注册完成前到达的并发派发也不能让 `once` 触发两次。
#[tokio::test]
async fn once_survives_a_concurrent_first_dispatch() {
    let app = App::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in = calls.clone();
    let _handle = app.root.once("once-race", move |_call: cordis_core::Call| {
        let calls = calls_in.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        })
    });

    let (left, right) = tokio::join!(
        app.root.emit("once-race", vec![]),
        app.root.emit("once-race", vec![])
    );
    let _ = (left, right);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "并发派发也只能触发一次");
}

/// M3-6：`resolve_intercept` 的合并契约（纯核心）——**祖先优先、最近层覆盖**，
/// 兄弟层互不可见，没有叠加过任何一层时返回 `None`。
/// 对应上游 `service.ts:51-67` 的 `resolveConfig`（链上每层 unshift + 逐层覆盖）。
///
/// 消费点（行级 `intercept` 到插件）在 M4-1；这里先把契约钉死。
#[test]
fn intercept_resolution_prefers_nearest_layer() {
    let app = App::new();
    assert!(
        app.root.resolve_intercept("svc").is_none(),
        "没叠加过任何一层 ⇒ None"
    );

    let outer = app
        .root
        .intercept("svc", serde_json::json!({ "a": 1, "b": 1 }));
    let middle = outer.intercept("svc", serde_json::json!({ "b": 2 }));
    let inner = middle.intercept("svc", serde_json::json!({ "c": 3 }));

    assert_eq!(
        outer.resolve_intercept("svc").unwrap(),
        serde_json::json!({ "a": 1, "b": 1 })
    );
    assert_eq!(
        middle.resolve_intercept("svc").unwrap(),
        serde_json::json!({ "a": 1, "b": 2 })
    );
    assert_eq!(
        inner.resolve_intercept("svc").unwrap(),
        serde_json::json!({ "a": 1, "b": 2, "c": 3 }),
        "最近层覆盖祖先（b=2），各层其余键都要保留"
    );

    // 兄弟层互不可见：另一个 child 叠加的层不进入这条链。
    let sibling = app.root.intercept("svc", serde_json::json!({ "z": 9 }));
    assert_eq!(
        sibling.resolve_intercept("svc").unwrap(),
        serde_json::json!({ "z": 9 })
    );
    assert_eq!(
        inner.resolve_intercept("svc").unwrap(),
        serde_json::json!({ "a": 1, "b": 2, "c": 3 }),
        "兄弟层不得泄漏进这条链"
    );

    // 层是按名字入链的：其他名字没有叠加过 ⇒ None。
    assert!(inner.resolve_intercept("other").is_none());
}
