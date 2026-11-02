//! M3-1：`internal/update` / `internal/commit` 的对齐规格。
//!
//! 上游参照：
//! - `cordis/packages/core/src/fiber.ts:486-506`（`update`：先 `_resolve` → commit → 赋值 → waterfall）
//! - `cordis/packages/core/src/events.ts:54-73`（内置 listener 路由 + global prepend 拼接）
//! - vendor 修改：`deepseek-harness/vendor/cordis/src/fiber.ts:736-753`
//!
//! 有意偏离（附录 A）：① `internal/commit` **总是**发出（上游 `noSave` 不发），
//! 让 loader 用单一通道区分"推进记录"与"写盘"；② `Ret=Option<Dyn>` 合并 null/undefined
//! ⇒ "调了 next 但显式返回 None"与"未调 next"无法区分，统一按否决
//! （上游只把 `undefined` 当否决）。

use cordis_core::{App, Config, Ctx, Disposable, Dyn, Fiber, Inject, UpdateOutcome};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn json(value: &str) -> Config {
    serde_json::from_str(value).unwrap()
}

/// 一个"活着的"插件：apply 里数一次数，提供 `svc`；可选地在 apply 里注册一个
/// per-fiber 的 `internal/update` 钩子。
fn counting_plugin(
    name: &'static str,
    applies: Arc<AtomicUsize>,
    hook: Option<Arc<dyn Fn(cordis_core::Call) + Send + Sync>>,
) -> Arc<dyn cordis_core::Plugin> {
    // 服务名带插件名：两个 fiber 挂同一个 App 时不得相撞（同一域里一个服务名一个实现）。
    let svc = format!("svc.{name}");
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, _cfg: Config| {
            let applies = applies.clone();
            let hook = hook.clone();
            let svc = svc.clone();
            Box::pin(async move {
                applies.fetch_add(1, Ordering::SeqCst);
                if let Some(hook) = hook {
                    ctx.on("internal/update", move |call: cordis_core::Call| {
                        hook(call);
                        Box::pin(async { Some(Dyn::new(true)) })
                    });
                }
                ctx.provide(&svc, Dyn::new(true))?;
                Ok(())
            })
        }),
    })
}

fn plain_plugin(name: &'static str) -> Arc<dyn cordis_core::Plugin> {
    cordis_core::plugin_fn(name, Inject::default(), |_ctx: Ctx, _cfg| {
        Box::pin(async { Ok(()) })
    })
}

/// M3-1（红→绿）：`ctx.on('internal/update', …)` 是**行内钩子**——只对该 fiber 生效。
///
/// 上游 `events.ts:54-58`：内置的 `internal/listener` 会把 `internal/update` 的
/// 非全局注册路由进 `fiber._hooks['internal/update']`，而不是事件总表。
#[tokio::test]
async fn per_fiber_update_hook_only_runs_for_its_fiber() {
    let app = App::new();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));

    let make = |label: &'static str, log: Arc<Mutex<Vec<String>>>| {
        let hook: Arc<dyn Fn(cordis_core::Call) + Send + Sync> = Arc::new(move |_call| {
            log.lock().unwrap().push(label.to_string());
        });
        counting_plugin(label, Arc::new(AtomicUsize::new(0)), Some(hook))
    };

    let fiber_a = app.root.plugin(make("a", log.clone()), Config::Null);
    fiber_a.settle().await;
    let fiber_b = app.root.plugin(make("b", log.clone()), Config::Null);
    fiber_b.settle().await;
    assert_eq!(fiber_a.state(), cordis_core::FiberState::Active);
    assert_eq!(fiber_b.state(), cordis_core::FiberState::Active);

    let outcome = fiber_a
        .update_with(json(r#"{"v":1}"#), false)
        .await
        .unwrap();
    assert_eq!(outcome, UpdateOutcome::Restarted);

    let lines = log.lock().unwrap().clone();
    assert_eq!(
        lines,
        vec!["a".to_string()],
        "只有 a 自己的钩子应当运行：{lines:?}"
    );
}

/// M3-1（红→绿）：带 `global: true` 注册的 `internal/update` 钩子对**每个** fiber 都运行。
#[tokio::test]
async fn global_update_hooks_run_for_every_fiber() {
    let app = App::new();
    let seen = Arc::new(Mutex::new(Vec::<u64>::new()));

    {
        let seen = seen.clone();
        app.root.on_with(
            "internal/update",
            move |call: cordis_core::Call| {
                let seen = seen.clone();
                Box::pin(async move {
                    let fiber = call.args[0]
                        .downcast::<Fiber>()
                        .expect("第一个实参是 fiber");
                    seen.lock().unwrap().push(fiber.uid());
                    call.next.call(call.args.clone()).await
                })
            },
            false,
            true,
        );
    }

    let fiber_a = app.root.plugin(plain_plugin("ga"), Config::Null);
    fiber_a.settle().await;
    let fiber_b = app.root.plugin(plain_plugin("gb"), Config::Null);
    fiber_b.settle().await;

    fiber_a
        .update_with(json(r#"{"v":1}"#), false)
        .await
        .unwrap();
    fiber_b
        .update_with(json(r#"{"v":2}"#), false)
        .await
        .unwrap();

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![fiber_a.uid(), fiber_b.uid()],
        "全局钩子应逐个 fiber 运行：{seen:?}"
    );
}

/// M3-1（红→绿）：`internal/update` 的实参 = `[fiber, resolved, no_save]`。
///
/// "resolved" 是过了 `internal/config` 瀑布之后的配置（M3-1 起 `update_with` 会先解析），
/// 不是调用方传进来的 raw。
#[tokio::test]
async fn update_args_carry_fiber_resolved_config_and_no_save() {
    let app = App::new();
    let config_runs = Arc::new(AtomicUsize::new(0));
    let recorded: Arc<Mutex<Vec<(u64, Config, bool)>>> = Arc::new(Mutex::new(Vec::new()));

    {
        let config_runs = config_runs.clone();
        app.root
            .on("internal/config", move |call: cordis_core::Call| {
                let config_runs = config_runs.clone();
                Box::pin(async move {
                    config_runs.fetch_add(1, Ordering::SeqCst);
                    // M3-2：首实参是 fiber，raw 在第二位。
                    let raw = call.args[1].downcast::<Config>().expect("raw config");
                    let mut object = raw.as_object().cloned().unwrap_or_default();
                    object.insert("resolved_by_hook".to_string(), serde_json::json!(true));
                    Some(Dyn::new(Config::Object(object)))
                })
            });
    }

    {
        let recorded_in_hook = recorded.clone();
        let hook: Arc<dyn Fn(cordis_core::Call) + Send + Sync> = Arc::new(move |call| {
            let fiber = call.args[0].downcast::<Fiber>().expect("fiber 实参");
            let resolved = call.args[1].downcast::<Config>().expect("resolved 实参");
            let resolved = (*resolved).clone();
            let no_save = call.args[2].downcast::<bool>().map(|v| *v).unwrap_or(false);
            recorded_in_hook
                .lock()
                .unwrap()
                .push((fiber.uid(), resolved, no_save));
        });
        let fiber = app.root.plugin(
            counting_plugin("args", Arc::new(AtomicUsize::new(0)), Some(hook)),
            Config::Null,
        );
        fiber.settle().await;

        let outcome = fiber.update_with(json(r#"{"v":1}"#), true).await.unwrap();
        assert_eq!(outcome, UpdateOutcome::Restarted);

        let recorded = recorded.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "钩子应恰好跑一次：{recorded:?}");
        assert_eq!(recorded[0].0, fiber.uid(), "第一个实参必须是本 fiber");
        assert_eq!(
            recorded[0].1["resolved_by_hook"],
            serde_json::json!(true),
            "第二个实参必须是 resolved 配置"
        );
        assert_eq!(
            recorded[0].1["v"],
            serde_json::json!(1),
            "raw 的字段必须还在"
        );
        assert!(recorded[0].2, "第三个实参必须原样带出 no_save");
        // 解析路径必须真的跑过。注意它会跑**两次**：update 自己解析一次，
        // 重启后的装载路径再解析一次——上游同样如此（`fiber.ts:490` 与 `:150`）。
        assert!(
            config_runs.load(Ordering::SeqCst) >= 1,
            "解析路径必须真的跑过"
        );
    }
}

/// M3-2（红→绿）：`internal/config` 的首实参必须是**正在解析的那个 fiber**，
/// 第二实参才是 raw config（上游 `fiber.ts:230`：`waterfall('internal/config', this, () => config)`）。
///
/// 现状（红）：派发实参是 `[raw]` —— 首实参根本不是 fiber，
/// 监听者拿不到"这是谁的配置"，也就无法做 per-fiber 的插值/改写。
#[tokio::test]
async fn internal_config_listener_receives_fiber() {
    let app = App::new();
    let recorded: Arc<Mutex<Vec<(u64, Config)>>> = Arc::new(Mutex::new(Vec::new()));

    {
        let recorded_in_hook = recorded.clone();
        app.root
            .on("internal/config", move |call: cordis_core::Call| {
                let recorded = recorded_in_hook.clone();
                Box::pin(async move {
                    let fiber = call.args[0]
                        .downcast::<Fiber>()
                        .expect("首实参必须是 fiber");
                    let raw = call.args[1]
                        .downcast::<Config>()
                        .expect("第二实参必须是 raw config");
                    recorded.lock().unwrap().push((fiber.uid(), (*raw).clone()));
                    call.next.call(call.args.clone()).await
                })
            });
    }

    let fiber = app.root.plugin(plain_plugin("cfg"), Config::Null);
    fiber.settle().await; // 装载路径必须解析一次

    // update 路径再解析（装载路径与 update 路径都要带上 fiber）。
    fiber.update_with(json(r#"{"v":1}"#), false).await.unwrap();

    let recorded = recorded.lock().unwrap().clone();
    assert!(!recorded.is_empty(), "internal/config 必须被派发过");
    assert!(
        recorded.iter().all(|(uid, _)| *uid == fiber.uid()),
        "首实参必须是正在解析的 fiber：{recorded:?}"
    );
    assert!(
        recorded.iter().any(|(_, raw)| raw == &json(r#"{"v":1}"#)),
        "第二实参必须是 raw（update 传进来的那份）：{recorded:?}"
    );
}

/// M3-1（红→绿）：commit 收到 **raw**，waterfall 收到 **resolved**。
#[tokio::test]
async fn commit_receives_raw_config_waterfall_receives_resolved() {
    let app = App::new();
    let committed: Arc<Mutex<Vec<Config>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_resolved: Arc<Mutex<Vec<Config>>> = Arc::new(Mutex::new(Vec::new()));

    {
        let committed = committed.clone();
        app.store
            .clone()
            .on_commit(&app.root, move |_fiber, config, _no_save| {
                committed.lock().unwrap().push(config.clone())
            });
    }
    {
        app.root.on("internal/config", |call: cordis_core::Call| {
            Box::pin(async move {
                // M3-2：raw 在第二位（首位是 fiber）。
                let raw = call.args[1].downcast::<Config>().expect("raw config");
                let mut object = raw.as_object().cloned().unwrap_or_default();
                object.insert("resolved_by_hook".to_string(), serde_json::json!(true));
                Some(Dyn::new(Config::Object(object)))
            })
        });
    }

    let hook: Arc<dyn Fn(cordis_core::Call) + Send + Sync> = Arc::new({
        let seen_resolved = seen_resolved.clone();
        move |call| {
            let resolved = call.args[1].downcast::<Config>().expect("resolved 实参");
            seen_resolved.lock().unwrap().push((*resolved).clone());
        }
    });
    let fiber = app.root.plugin(
        counting_plugin("raw-resolved", Arc::new(AtomicUsize::new(0)), Some(hook)),
        Config::Null,
    );
    fiber.settle().await;

    let raw = json(r#"{"v":1}"#);
    fiber.update_with(raw.clone(), false).await.unwrap();

    let committed = committed.lock().unwrap().clone();
    let resolved = seen_resolved.lock().unwrap().clone();
    assert_eq!(committed.len(), 1, "commit 应恰好一次");
    assert_eq!(committed[0], raw, "commit 载荷必须是 raw（未解析）");
    assert_eq!(resolved.len(), 1, "update 钩子应恰好一次");
    assert_eq!(
        resolved[0]["resolved_by_hook"],
        serde_json::json!(true),
        "waterfall 载荷必须是 resolved"
    );
    assert_ne!(
        committed[0], resolved[0],
        "两者必须真的不同（否则这条测试没有意义）"
    );
}

/// M3-1（红→绿）：commit **先于** waterfall 发生，且被否决也不缺席。
#[tokio::test]
async fn commit_fires_before_waterfall_and_survives_veto() {
    let app = App::new();
    let order = Arc::new(Mutex::new(Vec::<String>::new()));

    {
        let order = order.clone();
        app.store
            .clone()
            .on_commit(&app.root, move |_fiber, _config, _no_save| {
                order.lock().unwrap().push("commit".to_string())
            });
    }
    let hook: Arc<dyn Fn(cordis_core::Call) + Send + Sync> = Arc::new({
        let order = order.clone();
        move |_call| order.lock().unwrap().push("update".to_string())
    });

    let applies = Arc::new(AtomicUsize::new(0));
    let fiber = app.root.plugin(
        counting_plugin_vetoing("order", applies.clone(), hook),
        Config::Null,
    );
    fiber.settle().await;

    let outcome = fiber.update_with(json(r#"{"v":1}"#), false).await.unwrap();
    assert_eq!(outcome, UpdateOutcome::Vetoed, "钩子返回 None 应否决");
    assert_eq!(
        order.lock().unwrap().clone(),
        vec!["commit".to_string(), "update".to_string()],
        "commit 必须先发生，且否决不改变这一点"
    );
    assert_eq!(applies.load(Ordering::SeqCst), 1, "否决后不得重启");
}

/// 与 `counting_plugin` 相同，但注册的钩子**不**批准（waterfall 返回 None）。
fn counting_plugin_vetoing(
    name: &'static str,
    applies: Arc<AtomicUsize>,
    hook: Arc<dyn Fn(cordis_core::Call) + Send + Sync>,
) -> Arc<dyn cordis_core::Plugin> {
    let svc = format!("svc.{name}");
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, _cfg: Config| {
            let applies = applies.clone();
            let hook = hook.clone();
            let svc = svc.clone();
            Box::pin(async move {
                applies.fetch_add(1, Ordering::SeqCst);
                ctx.on("internal/update", move |call: cordis_core::Call| {
                    hook(call);
                    Box::pin(async { None })
                });
                ctx.provide(&svc, Dyn::new(true))?;
                Ok(())
            })
        }),
    })
}

/// M3-1（红→绿）：派发 `internal/update` 时，`fiber.config` **已经是新 raw 配置**
/// （上游 `fiber.ts:492` 的赋值在 waterfall 之前）。
#[tokio::test]
async fn fiber_config_set_before_dispatch() {
    let app = App::new();
    let observed: Arc<Mutex<Option<(Config, Config)>>> = Arc::new(Mutex::new(None));

    {
        app.root.on("internal/config", |call: cordis_core::Call| {
            Box::pin(async move {
                // M3-2：raw 在第二位（首位是 fiber）。
                let raw = call.args[1].downcast::<Config>().expect("raw config");
                let mut object = raw.as_object().cloned().unwrap_or_default();
                object.insert("resolved_by_hook".to_string(), serde_json::json!(true));
                Some(Dyn::new(Config::Object(object)))
            })
        });
    }
    let hook: Arc<dyn Fn(cordis_core::Call) + Send + Sync> = Arc::new({
        let observed = observed.clone();
        move |call| {
            let fiber = call.args[0].downcast::<Fiber>().expect("fiber 实参");
            let resolved = call.args[1].downcast::<Config>().expect("resolved 实参");
            *observed.lock().unwrap() = Some((fiber.config(), (*resolved).clone()));
        }
    });
    let fiber = app.root.plugin(
        counting_plugin("assign", Arc::new(AtomicUsize::new(0)), Some(hook)),
        Config::Null,
    );
    fiber.settle().await;

    let new_raw = json(r#"{"v":"new"}"#);
    fiber.update_with(new_raw.clone(), false).await.unwrap();

    let (current, resolved) = observed.lock().unwrap().clone().expect("钩子应已运行");
    assert_eq!(current, new_raw, "派发时 fiber.config 必须已是新 raw 配置");
    assert_eq!(
        resolved["resolved_by_hook"],
        serde_json::json!(true),
        "resolved 是另一份值"
    );
    assert_ne!(current, resolved, "raw 与 resolved 必须可区分");
}

/// M3-1（红→绿）：否决 = `Vetoed` + 不报错 + fiber 与服务原地不动。
#[tokio::test]
async fn veto_prevents_restart_and_reports_vetoed() {
    let app = App::new();
    let applies = Arc::new(AtomicUsize::new(0));
    let hook: Arc<dyn Fn(cordis_core::Call) + Send + Sync> = Arc::new(|_call| {});
    let fiber = app.root.plugin(
        counting_plugin_vetoing("veto", applies.clone(), hook),
        Config::Null,
    );
    fiber.settle().await;
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);

    let outcome = fiber.update_with(json(r#"{"v":1}"#), false).await.unwrap();
    assert_eq!(outcome, UpdateOutcome::Vetoed, "被否决不是错误");
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);
    assert_eq!(applies.load(Ordering::SeqCst), 1, "否决不重启");
    assert!(
        app.root.get_as::<bool>("svc.veto").is_some(),
        "旧实例必须仍在服务"
    );
}

/// 否决必须把 `resolved` 与 volatile 引用表一起回滚：
/// 插件缓存的 `Arc<VolatileRef>` 不能被换成一份新的（否则后续 volatile-only
/// 提交写的是新表，插件永远读不到）。
#[tokio::test]
async fn veto_restores_resolved_and_volatile_reference() {
    let app = App::new();
    let seen: Arc<Mutex<Option<Arc<cordis_core::VolatileRef>>>> = Arc::new(Mutex::new(None));
    let seen_in = seen.clone();
    let base = cordis_core::plugin_fn("vol-veto", Inject::default(), move |ctx: Ctx, _cfg| {
        let seen = seen_in.clone();
        Box::pin(async move {
            ctx.on("internal/update", |_call| Box::pin(async { None }));
            *seen.lock().unwrap() = ctx.volatile("data");
            Ok(())
        })
    });
    let plugin = cordis_core::declare_volatile(base, &["data"]);
    let fiber = app.root.plugin(plugin, json(r#"{"data":1}"#));
    fiber.settle().await;

    let old = seen
        .lock()
        .unwrap()
        .clone()
        .expect("插件应拿到 volatile 引用");
    assert_eq!(old.get(), serde_json::json!(1));

    let outcome = fiber
        .update_with(json(r#"{"data":2}"#), false)
        .await
        .unwrap();
    assert_eq!(outcome, UpdateOutcome::Vetoed);

    assert_eq!(
        fiber.resolved()["data"],
        serde_json::json!(1),
        "resolved 必须回滚"
    );
    assert_eq!(old.get(), serde_json::json!(1), "旧引用不能被候选改写");
    let current = fiber.volatile_ref("data").expect("引用仍在");
    assert!(Arc::ptr_eq(&old, &current), "否决后必须保留原来的引用身份");
}

/// M3-1（红→绿）：**同步监听器钩子链**可以接管一次 `ctx.on` 注册——
/// 返回 `Some(disposable)` 后，core 不再把 handler 放进事件表。
#[tokio::test]
async fn listener_hook_can_take_over_registration() {
    let app = App::new();
    let taken = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));

    {
        let taken = taken.clone();
        app.store.clone().hook_listener(
            &app.root,
            move |_ctx: &Ctx, name: &str, _handler, _prepend, _global| {
                if name == "shadowed" {
                    taken.fetch_add(1, Ordering::SeqCst);
                    return Some(Disposable::sync(|| {}));
                }
                None
            },
        );
    }

    let calls2 = calls.clone();
    let plugin = cordis_core::plugin_fn(
        "shadowed-registrar",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let calls = calls2.clone();
            Box::pin(async move {
                ctx.on("shadowed", move |_call: cordis_core::Call| {
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        None
                    })
                });
                Ok(())
            })
        },
    );
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    app.root.emit("shadowed", vec![]).await;
    assert_eq!(taken.load(Ordering::SeqCst), 1, "钩子必须被咨询到");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "被接管的注册不得进入事件表"
    );
}

/// M3-1（红→绿）：已知偏差钉住——"调了 next 但显式返回 None"统一按否决
/// （`Ret=Option<Dyn>` 合并了 null 与 undefined；上游只把 undefined 当否决）。
#[tokio::test]
async fn waterfall_null_is_treated_as_veto() {
    let app = App::new();
    let applies = Arc::new(AtomicUsize::new(0));

    let plugin = Arc::new(cordis_core::FnPlugin {
        name: "null-veto",
        key: cordis_core::unique_key("null-veto"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new({
            let applies = applies.clone();
            move |ctx: Ctx, _cfg: Config| {
                let applies = applies.clone();
                Box::pin(async move {
                    applies.fetch_add(1, Ordering::SeqCst);
                    ctx.on("internal/update", |call: cordis_core::Call| {
                        Box::pin(async move {
                            let _ = call.next.call(call.args.clone()).await; // 明确放行了下游……
                            None // ……但自己返回 None
                        })
                    });
                    Ok(())
                })
            }
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    let outcome = fiber.update_with(json(r#"{"v":1}"#), false).await.unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Vetoed,
        "None 统一解释为否决（已知偏差）"
    );
    assert_eq!(applies.load(Ordering::SeqCst), 1, "否决 ⇒ 不重启");
}

/// M3-1（红→绿）：校验失败 ⇒ `update_with` 直接返回 Err，**fiber 保持原样**
/// （不再是"重启 → apply 失败 → fiber FAILED"）。
#[tokio::test]
async fn update_validation_failure_returns_err_and_fiber_stays() {
    let app = App::new();
    let applies = Arc::new(AtomicUsize::new(0));

    let plugin = cordis_core::plugin_fn_validated(
        "validated",
        Inject::default(),
        |raw: Config| {
            if raw.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                Ok(raw)
            } else {
                Err(cordis_core::ValidationError {
                    issues: vec!["ok: required".into()],
                })
            }
        },
        {
            let applies = applies.clone();
            move |ctx: Ctx, _cfg| {
                let applies = applies.clone();
                Box::pin(async move {
                    applies.fetch_add(1, Ordering::SeqCst);
                    ctx.provide("svc", Dyn::new(true))?;
                    Ok(())
                })
            }
        },
    );
    let fiber = app.root.plugin(plugin, json(r#"{"ok":true}"#));
    fiber.settle().await;
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);

    let error = fiber
        .update_with(json(r#"{"ok":false}"#), false)
        .await
        .expect_err("校验失败必须返回 Err");
    assert!(error.to_string().contains("ok: required"), "{error}");
    assert_eq!(
        fiber.state(),
        cordis_core::FiberState::Active,
        "fiber 不得被打成 FAILED"
    );
    assert_eq!(fiber.config(), json(r#"{"ok":true}"#), "旧配置必须原地不动");
    assert_eq!(applies.load(Ordering::SeqCst), 1, "不得重启");
    assert!(
        app.root.get_as::<bool>("svc").is_some(),
        "旧实例必须仍在服务"
    );
}
