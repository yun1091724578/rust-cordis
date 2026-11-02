//! M10b：**Weak 审计**——内核里"谁强持谁"是可执行契约，不是注释。
//!
//! M4 把 `Context.fiber` 改成了 `Weak<Fiber>`（派生 ctx 不再把 fiber 的引用计数抬高），
//! 根 fiber 的强引用在根 ctx 的 `root_keepalive` 上（`Store` 只持弱引用）。这条测试盯住的
//! 就是这一类引用环：**任何一处忘记用 `Weak` 的改动**（比如某个诊断面把 ctx 存进了全局表）
//! 都会让它红——而那种泄漏在运行期只表现为"内存缓慢上涨"，是最难查的一类。
//!
//! 判据（M10b 的题面）：`fiber.dispose()` 之后，它自己的 `Context` 与它派生出来的
//! 作用域 `Context` 都必须**回收**（`Weak::strong_count == 0`），而 `App::root`
//! 的上下文照旧活着（根 ctx 的 keepalive 持有根 fiber，根 fiber 弱持根 ctx）。
//!
//! 另一个方向：丢弃一个**空 App** 必须释放它的 `Store`。`Store → root_fiber → Store`
//! 与 `Store → hooks → Hook → root ctx → Store` 都曾构成强引用环。

use std::sync::Weak;

use cordis_core::{
    plugin_fn, App, Call, Config, Context, Ctx, Disposable, Dyn, FiberState, Inject, Ret,
};

/// 一个**派生 ctx** 的弱引用观测点：在 apply 里把作用域 ctx 换出来，供外面观测。
static SCOPED: std::sync::OnceLock<Weak<Context>> = std::sync::OnceLock::new();

#[tokio::test]
async fn ctx_is_not_leaked_after_fiber_dispose() {
    let app = App::new();
    let plugin = plugin_fn("leak-probe", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("probe", Dyn::new(1u32))?;
            // ① 作用域 ctx（M4）：注册一个监听器，让"钩子表 → 闭包 → ctx"这条链真的存在。
            let scoped = ctx.filter_subtree();
            let mine = scoped.downgrade();
            scoped.on("probe/ping", move |_call: Call| {
                let _mine = mine.clone(); // 闭包**故意**持有派生 ctx 的弱引用
                Box::pin(async { None::<Dyn> }) as cordis_core::BoxFuture<Ret>
            });
            let _ = SCOPED.set(scoped.downgrade());
            Ok(())
        })
    });
    let fiber = app.plugin(plugin, Config::Null).await.unwrap();
    assert_eq!(fiber.state(), FiberState::Active);

    // ② fiber 自己那个 ctx 的弱引用（外面只留 Weak，不留强引用）。
    let weak_own = {
        let own: Ctx = fiber.ctx();
        let weak = own.downgrade();
        drop(own);
        weak
    };
    let weak_scoped = SCOPED.get().expect("apply 里登记过作用域 ctx").clone();
    let weak_root = app.root.downgrade();

    let effects_before = fiber.effect_count();
    assert_eq!(effects_before, 2, "提供者与监听器各一个 effect");
    fiber.dispose().await;
    // `set_state` 会 spawn `internal/status` 派发；它们短暂持有 ctx 克隆，
    // 让出一拍之后才是"是否仍有长寿命强引用"的判据。
    tokio::task::yield_now().await;

    // ③ 契约：fiber 的 ctx 与它的派生作用域都回收了……
    assert_eq!(
        Weak::strong_count(&weak_own),
        0,
        "dispose 之后 fiber 自己的 Context 必须被回收（有谁在强持它 = 引用环）"
    );
    assert_eq!(
        Weak::strong_count(&weak_scoped),
        0,
        "派生的作用域 ctx 同样必须回收（闭包里的 Weak 不算强持）"
    );
    // ④ ……而树根的上下文照旧活着（根 ctx 的 keepalive 持有根 fiber）。
    //    这条对照断言的意义：别把"什么都回收了"当成通过——那样 root 也会被回收。
    assert!(
        weak_root.upgrade().is_some(),
        "root 上下文必须仍然活着（root keepalive 持有根 fiber）"
    );
    assert!(
        app.root.require("probe").is_err(),
        "插件提供的服务随它一起消失"
    );
}

/// 父 fiber 的 effect 栈不能成为已处置子 fiber 的强引用仓库。
#[tokio::test]
async fn parent_releases_disposed_children() {
    let app = App::new();
    let parent = app
        .plugin(
            plugin_fn("child-owner", Inject::default(), |_ctx: Ctx, _cfg| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        )
        .await
        .unwrap();

    let mut weak_children = Vec::new();
    for _ in 0..100 {
        let child = parent.ctx().plugin(
            plugin_fn("disposed-child", Inject::default(), |_ctx: Ctx, _cfg| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        );
        child.settle().await;
        weak_children.push(std::sync::Arc::downgrade(&child));
        child.dispose().await;
    }
    tokio::task::yield_now().await;

    let alive = weak_children
        .iter()
        .filter(|weak| weak.upgrade().is_some())
        .count();
    assert_eq!(
        alive, 0,
        "父 fiber 仍强持 {alive} 个已处置子 fiber（子 fiber 的 dispose effect 不应强持对象）"
    );
    assert_eq!(
        parent.effect_count(),
        0,
        "已处置子 fiber 的父级 effect 登记也必须被摘除，而不是留到父 fiber 卸载"
    );
}

/// 手动执行 effect 句柄后，恢复栈里的登记必须同步消失；否则长期存活的
/// fiber 每创建/释放一次临时监听器就多一个永不执行的空槽。
#[tokio::test]
async fn manually_released_effects_do_not_accumulate() {
    let app = App::new();
    let fiber = app
        .plugin(
            plugin_fn("effect-churn", Inject::default(), |_ctx: Ctx, _cfg| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        )
        .await
        .unwrap();

    for _ in 0..1_000 {
        let handle = fiber.effect(|_| Disposable::sync(|| {}));
        handle.run().await;
    }

    assert_eq!(
        fiber.effect_count(),
        0,
        "手动释放的 effect 仍留在恢复栈里：内存与关闭成本会随运行时长线性增长"
    );
}

/// 撞名的 `provide` 失败后不得留下恢复栈登记：调用方拿不到句柄，也就没人能撤销它。
/// 曾经每次失败都欠一条永不执行的空槽——长期运行里重试会让恢复栈无限增长。
#[tokio::test]
async fn failed_provide_does_not_leave_recovery_entries() {
    let app = App::new();
    let fiber = app
        .plugin(
            plugin_fn("dup-provide", Inject::default(), |_ctx: Ctx, _cfg| {
                Box::pin(async { Ok(()) })
            }),
            Config::Null,
        )
        .await
        .unwrap();

    let ctx = fiber.ctx();
    ctx.provide("dup", Dyn::new(1u32))
        .expect("首次注册必须成功");
    let baseline = fiber.effect_count();
    assert_eq!(baseline, 1, "一次成功的 provide 只该登记一个 effect");

    for _ in 0..10_000 {
        let err = match ctx.provide("dup", Dyn::new(2u32)) {
            Ok(_) => panic!("同名服务二次注册必须被拒"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("已被"),
            "错误信息要指出冲突：{err}"
        );
    }

    assert_eq!(
        fiber.effect_count(),
        baseline,
        "失败的 provide 在恢复栈里留下了空槽：内存与关闭成本会随重试次数线性增长"
    );
}

/// 空 `App` 也不能把 `Store` / 根 fiber / 内置 hook 留在环里。
#[test]
fn dropping_an_empty_app_releases_the_store() {
    let weak_store;
    {
        let app = App::new();
        assert!(
            app.store.root_fiber().is_some(),
            "App 活着时根 fiber 必须可升级"
        );
        weak_store = std::sync::Arc::downgrade(&app.store);
    }
    assert!(
        weak_store.upgrade().is_none(),
        "空 App 丢弃后 Store 仍被强引用（root_fiber 或内置 Hook 的环）"
    );
}

/// `App::new().root` 这种只留下 root 的用法仍保有 fiber 身份；丢掉 root 后整棵树回收。
#[test]
fn root_context_alone_keeps_and_releases_its_fiber() {
    let root = App::new().root;
    assert!(
        root.fiber_opt().is_some(),
        "根 ctx 的 keepalive 必须强持根 fiber"
    );
    let weak_store = std::sync::Arc::downgrade(&root.store);
    drop(root);
    assert!(
        weak_store.upgrade().is_none(),
        "最后一个 root ctx 丢弃后 Store 必须释放"
    );
}

/// 根级监听器不能把根 ctx / Store 重新吊成环。
#[test]
fn dropping_an_app_with_a_root_listener_releases_the_store() {
    let weak_store;
    {
        let app = App::new();
        let _listener = app.root.on("root/listen", |_call: Call| {
            Box::pin(async { None::<Dyn> }) as cordis_core::BoxFuture<Ret>
        });
        weak_store = std::sync::Arc::downgrade(&app.store);
    }
    assert!(
        weak_store.upgrade().is_none(),
        "根级 Hook 必须弱持根 ctx，否则 App 关闭后 Store 仍被环吊住"
    );
}
