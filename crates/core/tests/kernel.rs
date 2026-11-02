//! 内核语义验证。
//!
//! 每条测试对应 cordis 的一个**行为契约**（不是实现细节），跨语言必须成立。
//! 注释里给出对应的 JS 测试文件，便于对照。

use cordis_core::{App, Config, Ctx, Dyn, Inject, Mode, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn json(value: &str) -> Config {
    serde_json::from_str(value).unwrap()
}

fn noop_apply() -> impl Fn(Ctx, Config) -> cordis_core::BoxFuture<Result<()>> + Send + Sync + 'static
{
    |_ctx, _cfg| Box::pin(async { Ok(()) })
}

/// 提供服务 + 消费服务：消费者只有等提供者就绪才会被激活。
/// 对应 `core/tests/service.spec.ts`。
#[tokio::test]
async fn service_provider_and_consumer() {
    let app = App::new();

    let provider = cordis_core::plugin_fn("provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("greeting", Dyn::new("hello".to_string()))?;
            Ok(())
        })
    });
    let consumer =
        cordis_core::plugin_fn("consumer", Inject::of(&["greeting"]), |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let value = ctx.require_as::<String>("greeting").unwrap();
                assert_eq!(&*value, "hello");
                Ok(())
            })
        });

    // 先挂消费者：它必须停在 PENDING，且读服务会给出"未就绪"的错误。
    let consumer_fiber = app.root.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Pending);
    assert!(app.root.require("greeting").is_err());

    // 再挂提供者：消费者应当被自动拉起。
    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    consumer_fiber.settle().await;

    assert_eq!(provider_fiber.state(), cordis_core::FiberState::Active);
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Active);
}

/// 提供者卸载 → 消费者自动卸载；提供者回来 → 消费者自动重载。
/// 这是"加载顺序由服务可用性驱动"的核心证明。对应 `core/tests/dispose.spec.ts`。
#[tokio::test]
async fn provider_teardown_unloads_consumers() {
    let app = App::new();
    let activations = Arc::new(AtomicUsize::new(0));

    let provider = cordis_core::plugin_fn("provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("svc", Dyn::new(1u32))?;
            Ok(())
        })
    });

    let counter = activations.clone();
    let consumer =
        cordis_core::plugin_fn("consumer", Inject::of(&["svc"]), move |_ctx: Ctx, _cfg| {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

    let provider_fiber = app.root.plugin(provider.clone(), Config::Null);
    provider_fiber.settle().await;
    let consumer_fiber = app.root.plugin(consumer.clone(), Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(activations.load(Ordering::SeqCst), 1);
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Active);

    // 停用提供者 → 消费者必须跟着卸载（不是被删掉，而是回到 PENDING）。
    provider_fiber.dispose().await;
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Pending);

    // 提供者回来 → 消费者重新激活。
    let provider_again = app.root.plugin(provider, Config::Null);
    provider_again.settle().await;
    consumer_fiber.settle().await;
    assert_eq!(activations.load(Ordering::SeqCst), 2);
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Active);
}

/// effect 必须**逆序**释放，且父 fiber 卸载会递归卸载子 fiber。
/// 对应 `core/tests/fiber.spec.ts` / `dispose.spec.ts`。
#[tokio::test]
async fn effects_release_in_reverse_order_and_parent_dispose_is_recursive() {
    let app = App::new();
    let order = Arc::new(Mutex::new(Vec::<String>::new()));

    let log_first = order.clone();
    let log_second = order.clone();
    let parent = cordis_core::plugin_fn("parent", Inject::default(), move |ctx: Ctx, _cfg| {
        let log_first = log_first.clone();
        let log_second = log_second.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                cordis_core::Disposable::sync(move || {
                    log_first.lock().unwrap().push("first".into())
                })
            });
            ctx.effect(move |_| {
                cordis_core::Disposable::sync(move || {
                    log_second.lock().unwrap().push("second".into())
                })
            });
            Ok(())
        })
    });

    let log_child = order.clone();
    let child = cordis_core::plugin_fn("child", Inject::default(), move |ctx: Ctx, _cfg| {
        let log_child = log_child.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                cordis_core::Disposable::sync(move || {
                    log_child.lock().unwrap().push("child".into())
                })
            });
            Ok(())
        })
    });

    let parent_fiber = app.root.plugin(parent, Config::Null);
    parent_fiber.settle().await;
    // 子插件挂在**父 fiber 的上下文**上，因此生命周期由父级 effect 拥有。
    let child_fiber = parent_fiber.ctx().plugin(child, Config::Null);
    child_fiber.settle().await;
    assert_eq!(child_fiber.state(), cordis_core::FiberState::Active);

    // 卸载父级：子 fiber 必须递归卸载，父级自身的 effect 必须逆序释放。
    parent_fiber.dispose().await;

    let recorded = order.lock().unwrap().clone();
    assert_eq!(recorded, vec!["child", "second", "first"]);
    assert!(child_fiber.is_disposed());
}

#[tokio::test]
async fn disposing_the_root_unloads_children_and_effects() {
    let app = App::new();
    let cleaned = Arc::new(AtomicUsize::new(0));

    let root_cleanup = cleaned.clone();
    app.root.effect(move |_| {
        cordis_core::Disposable::sync(move || {
            root_cleanup.fetch_add(1, Ordering::SeqCst);
        })
    });

    let child_cleanup = cleaned.clone();
    let child = cordis_core::plugin_fn("root-child", Inject::default(), move |ctx: Ctx, _cfg| {
        let child_cleanup = child_cleanup.clone();
        Box::pin(async move {
            ctx.provide("root-child-svc", Dyn::new(1u32))?;
            ctx.effect(move |_| {
                cordis_core::Disposable::sync(move || {
                    child_cleanup.fetch_add(1, Ordering::SeqCst);
                })
            });
            Ok(())
        })
    });
    let child_fiber = app.root.plugin(child, Config::Null);
    child_fiber.settle().await;
    assert_eq!(child_fiber.state(), cordis_core::FiberState::Active);
    assert!(app.root.require("root-child-svc").is_ok());

    // 根 fiber 的 `running` 是 None（它从不 load），但 dispose 仍必须走 unload。
    app.root_fiber().dispose().await;

    assert!(child_fiber.is_disposed(), "根处置必须递归处置子插件");
    assert_eq!(child_fiber.state(), cordis_core::FiberState::Disposed);
    assert_eq!(
        cleaned.load(Ordering::SeqCst),
        2,
        "根 effect 与子插件 effect 都必须释放"
    );
    assert!(
        app.root.require("root-child-svc").is_err(),
        "子插件提供的服务必须消失"
    );
}

/// `App::close` 是统一关闭入口：处置整棵根树、释放 effect，并清空根级 hook。
#[tokio::test]
async fn app_close_disposes_the_tree_and_clears_root_hooks() {
    let app = App::new();
    let store = app.store.clone();
    let root = app.root.clone();
    let cleaned = Arc::new(AtomicUsize::new(0));

    let root_cleanup = cleaned.clone();
    root.effect(move |_| {
        cordis_core::Disposable::sync(move || {
            root_cleanup.fetch_add(1, Ordering::SeqCst);
        })
    });
    root.on("app-close/probe", |_call| Box::pin(async { None::<Dyn> }));
    assert_eq!(store.hook_count("app-close/probe"), 1, "根级 hook 已注册");

    let child_cleanup = cleaned.clone();
    let child = cordis_core::plugin_fn(
        "app-close-child",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let child_cleanup = child_cleanup.clone();
            Box::pin(async move {
                ctx.provide("app-close-svc", Dyn::new(1u32))?;
                ctx.effect(move |_| {
                    cordis_core::Disposable::sync(move || {
                        child_cleanup.fetch_add(1, Ordering::SeqCst);
                    })
                });
                Ok(())
            })
        },
    );
    let child_fiber = app.plugin(child, Config::Null).await.unwrap();

    app.close().await;

    assert!(child_fiber.is_disposed(), "App::close 必须递归处置子插件");
    assert_eq!(
        cleaned.load(Ordering::SeqCst),
        2,
        "根 effect 与子插件 effect 都必须释放"
    );
    assert_eq!(
        store.hook_count("app-close/probe"),
        0,
        "关闭后不得残留根级 hook"
    );
    assert!(
        root.fiber().is_disposed(),
        "根 fiber 必须进入 Disposed 终态"
    );
    assert!(
        root.require("app-close-svc").is_err(),
        "子插件提供的服务必须消失"
    );
}

/// 处置是终态：`dispose()` 之后 `state()` 不能又回到 Pending。
#[tokio::test]
async fn disposed_fiber_reports_terminal_state() {
    let app = App::new();
    let fiber = app.root.plugin(
        cordis_core::plugin_fn("terminal", Inject::default(), noop_apply()),
        Config::Null,
    );
    fiber.settle().await;
    fiber.dispose().await;
    assert!(fiber.is_disposed());
    assert_eq!(fiber.state(), cordis_core::FiberState::Disposed);
}

/// 加载期在**派生 isolate ctx** 上 provide，激活时必须按实际注册的 realm 通知
/// 同域 consumer；否则 consumer 会永远停在 Pending。
#[tokio::test]
async fn derived_isolate_provide_notifies_same_realm_consumer() {
    let app = App::new();
    let provider = cordis_core::plugin_fn("iso-provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            let iso = ctx.isolate("svc", Some(7));
            iso.provide("svc", Dyn::new(42u32))?;
            Ok(())
        })
    });
    let consumer =
        cordis_core::plugin_fn("iso-consumer", Inject::of(&["svc"]), |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let value = ctx.require_as::<u32>("svc")?;
                assert_eq!(*value, 42);
                Ok(())
            })
        });

    let iso = app.root.isolate("svc", Some(7));
    let consumer_fiber = iso.plugin(consumer, Config::Null);
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Pending);

    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    consumer_fiber.settle().await;
    assert_eq!(consumer_fiber.state(), cordis_core::FiberState::Active);
}

/// 父 fiber 挂一个提供服务的子 fiber，父卸载时子 provide disposer 不得与父
/// 的 driver 互相等待（子 dispose 由父的 effect 链触发）。
#[tokio::test]
async fn parent_with_provider_child_disposes_without_deadlock() {
    let app = App::new();
    let child = cordis_core::plugin_fn("child-provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("svc", Dyn::new(1u32))?;
            Ok(())
        })
    });
    let parent = cordis_core::plugin_fn(
        "parent-with-child",
        Inject::default(),
        move |ctx: Ctx, _cfg| {
            let child = child.clone();
            Box::pin(async move {
                let child_fiber = ctx.plugin(child, Config::Null);
                child_fiber.settle().await;
                Ok(())
            })
        },
    );

    let parent_fiber = app.root.plugin(parent, Config::Null);
    parent_fiber.settle().await;
    assert_eq!(parent_fiber.state(), cordis_core::FiberState::Active);

    let result =
        tokio::time::timeout(std::time::Duration::from_secs(2), parent_fiber.dispose()).await;
    assert!(result.is_ok(), "parent/child provider dispose deadlocked");
}

/// 同一个插件的多个实例共享一条 Runtime 记录；全部卸载后记录才消失。
/// 对应 `registry.ts` 用 callback 函数对象当 Map key 的语义。
#[tokio::test]
async fn plugin_runtime_is_shared_across_instances() {
    let app = App::new();
    let plugin = cordis_core::plugin_fn("multi", Inject::default(), noop_apply());

    let first = app.root.plugin(plugin.clone(), Config::Null);
    first.settle().await;
    let second = app.root.plugin(plugin.clone(), Config::Null);
    second.settle().await;

    assert_eq!(app.store.runtime_count(), 1);

    first.dispose().await;
    assert_eq!(app.store.runtime_count(), 1, "还有一个实例存活");
    second.dispose().await;
    assert_eq!(
        app.store.runtime_count(),
        0,
        "最后一个实例卸载后 runtime 消失"
    );
}

/// 隔离域：同名服务在两个域里互不可见，各自提供、各自消费。
/// 对应 `core/tests/isolate.spec.ts`。
#[tokio::test]
async fn isolate_realms_are_independent() {
    let app = App::new();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));

    let make_provider = |value: &'static str| {
        cordis_core::plugin_fn("provider", Inject::default(), move |ctx: Ctx, _cfg| {
            Box::pin(async move {
                ctx.provide("slot", Dyn::new(value.to_string()))?;
                Ok(())
            })
        })
    };

    let realm_a = app.root.isolate("slot", None);
    let realm_b = app.root.isolate("slot", None);

    let fiber_a = realm_a.plugin(make_provider("A"), Config::Null);
    fiber_a.settle().await;
    let fiber_b = realm_b.plugin(make_provider("B"), Config::Null);
    fiber_b.settle().await;

    let observed = seen.clone();
    let reader = cordis_core::plugin_fn("reader", Inject::of(&["slot"]), move |ctx: Ctx, _cfg| {
        let observed = observed.clone();
        Box::pin(async move {
            let value = ctx.require_as::<String>("slot").unwrap();
            observed.lock().unwrap().push((*value).clone());
            Ok(())
        })
    });

    let reader_a = realm_a.plugin(reader.clone(), Config::Null);
    reader_a.settle().await;
    let reader_b = realm_b.plugin(reader, Config::Null);
    reader_b.settle().await;

    let mut got = seen.lock().unwrap().clone();
    got.sort();
    assert_eq!(got, vec!["A".to_string(), "B".to_string()]);
}

/// 配置校验失败 → fiber 进入 FAILED，且**半途注册的副作用被立刻回收**。
/// 对应 `fiber.ts:27-36` 的 `ValidationError` 与 `_reload` 的 catch。
#[tokio::test]
async fn failed_load_is_cleaned_up_and_reports_validation_error() {
    let app = App::new();
    let effects = Arc::new(AtomicUsize::new(0));

    let counter = effects.clone();
    let plugin = cordis_core::plugin_fn_validated(
        "bad-config",
        Inject::default(),
        |raw| {
            if raw.get("port").is_some() {
                Ok(raw)
            } else {
                Err(cordis_core::ValidationError {
                    issues: vec!["port: required (at port)".to_string()],
                })
            }
        },
        move |ctx: Ctx, _cfg| {
            let counter = counter.clone();
            Box::pin(async move {
                ctx.effect(move |_| {
                    let counter = counter.clone();
                    cordis_core::Disposable::sync(move || {
                        counter.fetch_add(1, Ordering::SeqCst);
                    })
                });
                // 注册完副作用之后才失败：这模拟"插件写到一半抛错"。
                Err("boom".into())
            })
        },
    );

    let fiber = app.root.plugin(plugin, json(r#"{"port": 8080}"#));
    fiber.settle().await;

    assert_eq!(fiber.state(), cordis_core::FiberState::Failed);
    assert_eq!(
        effects.load(Ordering::SeqCst),
        1,
        "半途注册的 effect 必须被回收"
    );
    assert!(fiber.error().unwrap().contains("boom"));

    // 配置校验的失败路径：缺字段 → 直接 FAILED，且不会跑 apply。
    let plugin2 = cordis_core::plugin_fn_validated(
        "bad-config-2",
        Inject::default(),
        |raw| {
            if raw.get("port").is_some() {
                Ok(raw)
            } else {
                Err(cordis_core::ValidationError {
                    issues: vec!["port: required (at port)".to_string()],
                })
            }
        },
        noop_apply(),
    );
    let fiber2 = app.root.plugin(plugin2, Config::Null);
    fiber2.settle().await;
    assert_eq!(fiber2.state(), cordis_core::FiberState::Failed);
    assert_eq!(
        fiber2.error().unwrap(),
        "invalid config:\n  - port: required (at port)"
    );
}

/// `update()` 是 FAILED 的唯一恢复路径，且会递增 revision 强制重载。
/// 对应 `core/tests/update.spec.ts`。
#[tokio::test]
async fn update_recovers_from_failure() {
    let app = App::new();

    let plugin = cordis_core::plugin_fn_validated(
        "recoverable",
        Inject::default(),
        |raw| {
            if raw.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                Ok(raw)
            } else {
                Err(cordis_core::ValidationError {
                    issues: vec!["ok: required".into()],
                })
            }
        },
        |ctx: Ctx, cfg| {
            Box::pin(async move {
                let ok = cfg.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                assert!(ok);
                ctx.provide("recovered", Dyn::new(true))?;
                Ok(())
            })
        },
    );

    let fiber = app.root.plugin(plugin, json(r#"{"ok": false}"#));
    fiber.settle().await;
    assert_eq!(fiber.state(), cordis_core::FiberState::Failed);

    fiber.update(json(r#"{"ok": true}"#)).await.unwrap();
    assert_eq!(fiber.state(), cordis_core::FiberState::Active);
    assert!(app.root.get_as::<bool>("recovered").is_some());
}

/// 五种事件调度的语义差异。
/// 对应 `core/tests/events.spec.ts`。
#[tokio::test]
async fn event_dispatch_modes() {
    let app = App::new();
    let ctx = app.root.clone();

    // on 注册的监听器由 fiber 拥有：用一个插件来注册，方便验证卸载即摘除。
    let plugin = cordis_core::plugin_fn("listeners", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.on("seen", |_call| Box::pin(async { None }));
            ctx.on("seen", |_call| Box::pin(async { None }));
            // bail：第一个非 None 返回值即短路。
            ctx.on("bail-demo", |_call| Box::pin(async { None }));
            ctx.on("bail-demo", |_call| {
                Box::pin(async { Some(Dyn::new("early".to_string())) })
            });
            ctx.on("bail-demo", |_call| {
                Box::pin(async { Some(Dyn::new("late".to_string())) })
            });
            // waterfall：环绕中间件，改写下游结果。
            ctx.on("chain", |call| {
                Box::pin(async move {
                    let downstream = call.next.call(call.args.clone()).await;
                    let n = downstream
                        .and_then(|v| v.downcast::<u32>())
                        .map(|v| *v)
                        .unwrap_or(0);
                    Some(Dyn::new(n + 1))
                })
            });
            Ok(())
        })
    });

    let fiber = ctx.plugin(plugin, Config::Null);
    fiber.settle().await;

    assert_eq!(ctx.hook_count("seen"), 2);
    ctx.emit("seen", vec![Dyn::new(1u32)]).await;

    let bailed = ctx.bail("bail-demo", vec![]).await;
    assert_eq!(*bailed.unwrap().downcast::<String>().unwrap(), "early");

    let chained = ctx
        .waterfall(
            "chain",
            vec![],
            Some(Box::pin(async { Some(Dyn::new(41u32)) })),
            None,
        )
        .await;
    assert_eq!(*chained.unwrap().downcast::<u32>().unwrap(), 42);

    // 卸载即摘除：hook 表必须清空。
    fiber.dispose().await;
    assert_eq!(ctx.hook_count("seen"), 0);
    assert_eq!(ctx.hook_count("bail-demo"), 0);
    assert_eq!(ctx.hook_count("chain"), 0);
}

/// `ctx.inject(deps, cb)`：迟绑定可选能力——依赖齐了才跑，依赖消失自动卸载。
/// 对应 `registry.ts:166-176`。
#[tokio::test]
async fn late_binding_inject_is_optional() {
    let app = App::new();
    let runs = Arc::new(AtomicUsize::new(0));

    let counter = runs.clone();
    let optional = app
        .root
        .inject(Inject::of(&["optional-capability"]), move |_ctx: Ctx| {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });
    optional.settle().await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "缺依赖时不该运行");
    assert_eq!(optional.state(), cordis_core::FiberState::Pending);

    // 补上提供者 → 自动激活。
    let provider = cordis_core::plugin_fn("capability", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("optional-capability", Dyn::new(1u8))?;
            Ok(())
        })
    });
    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    optional.settle().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // 提供者消失 → 自动回到 PENDING。
    provider_fiber.dispose().await;
    optional.settle().await;
    assert_eq!(optional.state(), cordis_core::FiberState::Pending);
}

/// `internal/plugin` 的观察者能在**依赖解析之前**追加 `inject`。
/// 这是 loader 行级 `inject` 的落点（`loader/src/index.ts:129-135`），
/// 也是"必须同步派发"的原因。
#[tokio::test]
async fn plugin_observer_can_augment_inject_before_resolution() {
    let app = App::new();
    let fired = Arc::new(AtomicUsize::new(0));

    let store = app.store.clone();
    let fired_slot = fired.clone();
    let root = app.root.clone();
    store.on_plugin(&root, move |fiber| {
        fired_slot.fetch_add(1, Ordering::SeqCst);
        // 模拟 loader：按配置行给这个 fiber 补一条依赖。
        fiber.add_inject("late-service");
    });

    let plugin = cordis_core::plugin_fn("augmented", Inject::default(), noop_apply());
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    assert_eq!(fired.load(Ordering::SeqCst), 1);
    assert_eq!(
        fiber.state(),
        cordis_core::FiberState::Pending,
        "补进来的依赖还没就绪"
    );
    assert!(fiber.declares_inject("late-service"));
}

/// 服务接口的版本协商与方法分发。
///
/// 这三条断言就是"接口能跨版本稳定演进"的全部内容：
/// 拿同版本能调、拿更高版本会被明确拒绝、调不存在的方法会列出可用方法。
#[tokio::test]
async fn iface_version_negotiation_and_method_dispatch() {
    use cordis_core::{ExternService, ExternValue, IfaceRef, IfaceVersionMismatch};
    use serde_json::json;
    use std::sync::Arc;

    struct Greeter;

    impl ExternService for Greeter {
        fn service(&self) -> &str {
            "greeter"
        }
        fn version(&self) -> u32 {
            2
        }
        fn methods(&self) -> Vec<String> {
            vec!["hello".into(), "shout".into()]
        }
        fn call(
            &self,
            _caller_meta: Option<Config>,
            method: String,
            args: Config,
        ) -> cordis_core::BoxFuture<Result<Config>> {
            Box::pin(async move {
                let who = args.get("who").and_then(|v| v.as_str()).unwrap_or("world");
                Ok(match method.as_str() {
                    "hello" => json!({ "text": format!("hello {who}") }),
                    "shout" => json!({ "text": format!("HELLO {who}!") }),
                    _ => return Err("unreachable".into()),
                })
            })
        }
    }

    let app = App::new();
    let provider = cordis_core::plugin_fn("provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            // 提供方声明自己是 v2
            ctx.provide_iface("greeter", Arc::new(Greeter))?;
            Ok(())
        })
    });
    let fiber = app.root.plugin(provider, Config::Null);
    fiber.settle().await;

    // 1) 请求方用 v1（更旧）→ 兼容，正常调用。
    let older = app
        .root
        .call_iface("greeter", 1, "hello", json!({ "who": "cordis" }))
        .await
        .unwrap();
    assert_eq!(
        older.get("text").and_then(|v| v.as_str()),
        Some("hello cordis")
    );

    // 2) 请求方用 v3（更新）→ 明确拒绝，且错误里能区分出"版本不匹配"这一类。
    let error = app
        .root
        .call_iface("greeter", 3, "hello", json!({}))
        .await
        .unwrap_err();
    assert!(
        error.downcast_ref::<IfaceVersionMismatch>().is_some(),
        "版本不匹配必须是可识别的类型，而不是一句字符串：{error}"
    );
    assert!(error.to_string().contains("请求接口版本 3"));

    // 3) 方法不存在 → 错误里必须列出可用方法（可执行的诊断）。
    let error = app
        .root
        .call_iface("greeter", 1, "whisper", json!({}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("没有方法 \"whisper\""),
        "{error}"
    );
    assert!(
        error.to_string().contains("shout"),
        "必须列出可用方法：{error}"
    );

    // 4) 纯数据服务被当成接口调用 → 给出"该怎么改"的提示。
    let data = cordis_core::plugin_fn("data", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("plain", Dyn::new(ExternValue(json!({ "k": 1 }))))?;
            Ok(())
        })
    });
    let data_fiber = app.root.plugin(data, Config::Null);
    data_fiber.settle().await;
    let error = app
        .root
        .call_iface("plain", 1, "x", json!({}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("provide_iface"), "{error}");

    // 5) 接口也能被 `get` 到：调用方能做能力发现（拿版本与方法清单）。
    let discovered = app.root.require_as::<IfaceRef>("greeter").unwrap();
    assert_eq!(discovered.0.version(), 2);
    assert_eq!(discovered.0.methods().len(), 2);
}

/// 无 fiber 的定义点（root / 宿主侧基础设施）直接读全局表，不做注入检查。
///
/// 对应 `reflect.ts:79-81`。少了这条，`root.get('某插件提供的服务')` 会莫名失败。
#[tokio::test]
async fn fiberless_definition_site_reads_global_store() {
    let app = App::new();
    let plugin = cordis_core::plugin_fn("provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("exposed", Dyn::new("value".to_string()))?;
            Ok(())
        })
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;

    // 从 root 读：它是无 fiber 的定义点，直接命中全局表。
    assert_eq!(
        &*app.root.require_as::<String>("exposed").unwrap(),
        "value",
        "root 是 fiber-less 定义点，应当能直接读到全局表里的实现"
    );

    // 而从一个**别的插件**里读同一个名字，就必须先声明 inject。
    let nosy = cordis_core::plugin_fn("nosy", Inject::default(), |_ctx: Ctx, _cfg| {
        Box::pin(async { Ok(()) })
    });
    let nosy_fiber = app.root.plugin(nosy, Config::Null);
    nosy_fiber.settle().await;
    let nosy_ctx = nosy_fiber.ctx();
    assert!(
        nosy_ctx.require("exposed").is_err(),
        "插件没声明 inject 就不能读该服务（这正是 cordis 的依赖声明强制）"
    );
}

/// 事件派发模式常量与「bail 值」判定。
#[test]
fn mode_names_match_cordis() {
    assert_eq!(Mode::Emit.as_str(), "emit");
    assert_eq!(Mode::Parallel.as_str(), "parallel");
    assert_eq!(Mode::Serial.as_str(), "serial");
    assert_eq!(Mode::Bail.as_str(), "bail");
    assert_eq!(Mode::Waterfall.as_str(), "waterfall");
    // `Option::None` 对应 JS 的 null/undefined；`Some(false)` 也**不是** bail 值
    // （JS 的 `value !== false`）。完整规格见 `tests/event_semantics.rs`。
    assert!(!cordis_core::is_bailed(&None));
    assert!(!cordis_core::is_bailed(&Some(Dyn::new(false))));
    assert!(cordis_core::is_bailed(&Some(Dyn::new(()))));
}
