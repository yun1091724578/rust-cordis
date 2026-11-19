//! WP2b：按 fiber 枚举可调用服务（契约锁的数据面）。

use cordis_core::{
    plugin_fn, App, BoxFuture, Config, Ctx, Disposable, Dyn, ExternService, Inject, Result,
    ServiceInfo, UID_DISPOSED,
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Notify;

struct Greeter {
    version: u32,
}

impl ExternService for Greeter {
    fn service(&self) -> &str {
        "greeter"
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn methods(&self) -> Vec<String> {
        // 故意乱序并带重复：导出面必须是稳定的名字集合。
        vec![
            "shout".to_string(),
            "hello".to_string(),
            "shout".to_string(),
        ]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        _method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        Box::pin(async { Ok(json!(null)) })
    }
}

fn provider(
    name: &'static str,
    key: &'static str,
    adapter_key: &'static str,
    data: &'static str,
    version: u32,
) -> Arc<dyn cordis_core::Plugin> {
    plugin_fn(name, Inject::default(), move |ctx: Ctx, _config: Config| {
        Box::pin(async move {
            ctx.provide_iface(key, Arc::new(Greeter { version }))?;
            // 第二条注册通道：显式 JSON 面（typed 服务的 `with_adapter` 形态）。
            ctx.provide(
                adapter_key,
                Dyn::new(json!(null)).with_adapter(Arc::new(Greeter { version: 9 })),
            )?;
            ctx.provide(data, Dyn::new(json!({ "ok": true })))?;
            Ok(())
        })
    })
}

#[tokio::test]
async fn services_of_lists_only_callable_services_of_that_fiber() {
    let app = App::new();
    let first = app.root.plugin(
        provider(
            "first",
            "greeter.first@1",
            "adapted.first",
            "plain.first",
            2,
        ),
        Config::Null,
    );
    let second = app.root.plugin(
        provider(
            "second",
            "greeter.second@1",
            "adapted.second",
            "plain.second",
            1,
        ),
        Config::Null,
    );
    first.settle().await;
    second.settle().await;

    let expected = vec![
        ServiceInfo {
            name: "adapted.first".to_string(),
            version: 9,
            methods: vec!["hello".to_string(), "shout".to_string()],
        },
        ServiceInfo {
            name: "greeter.first@1".to_string(),
            version: 2,
            methods: vec!["hello".to_string(), "shout".to_string()],
        },
    ];
    assert_eq!(app.store.services_of(&first), expected);
    assert_eq!(
        app.store.services_of(&second),
        vec![
            ServiceInfo {
                name: "adapted.second".to_string(),
                version: 9,
                methods: vec!["hello".to_string(), "shout".to_string()],
            },
            ServiceInfo {
                name: "greeter.second@1".to_string(),
                version: 1,
                methods: vec!["hello".to_string(), "shout".to_string()],
            },
        ],
        "两个实例的服务不能串线"
    );

    first.dispose().await;
    assert!(
        app.store.impls_owned_by(&first).is_empty(),
        "卸载必须摘掉提供者条目"
    );
    assert!(
        app.store.services_of(&first).is_empty(),
        "UID_DISPOSED 的 fiber 必须返回空，而不是枚举残留条目"
    );
    assert_eq!(
        app.store.services_of(&second).len(),
        2,
        "销毁一个实例不能影响另一个"
    );
}

#[tokio::test]
async fn services_of_is_empty_while_a_disposed_fiber_still_has_registered_impls() {
    let app = App::new();
    let gate = Arc::new(Notify::new());
    let blocking_gate = gate.clone();
    let fiber = app.root.plugin(
        plugin_fn("blocking", Inject::default(), move |ctx: Ctx, _| {
            let gate = blocking_gate.clone();
            Box::pin(async move {
                ctx.provide_iface("blocking@1", Arc::new(Greeter { version: 1 }))?;
                // 后注册的 effect 先卸载：它把提供者 disposer 挡在恢复栈上，
                // 于是 uid 已是 UID_DISPOSED、impl 条目却还在表里。
                ctx.effect(move |_| {
                    Disposable::new(move || {
                        Box::pin(async move {
                            gate.notified().await;
                        })
                    })
                });
                Ok(())
            })
        }),
        Config::Null,
    );
    fiber.settle().await;

    let disposing = {
        let fiber = fiber.clone();
        tokio::spawn(async move { fiber.dispose().await })
    };
    for _ in 0..100 {
        if fiber.uid() == UID_DISPOSED {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(fiber.uid(), UID_DISPOSED, "dispose 必须先置 uid");
    assert!(
        !app.store.impls_owned_by(&fiber).is_empty(),
        "判别性前提：清理被挡住时 impl 条目仍在表里"
    );
    assert!(
        app.store.services_of(&fiber).is_empty(),
        "服务导出必须看身份，不能枚举一个已销毁 fiber 的残留条目"
    );

    gate.notify_one();
    disposing.await.expect("dispose 任务不能 panic");
    assert!(app.store.impls_owned_by(&fiber).is_empty());
}
