//! M7 红测试：`components::registry`——provider **原地替换**（G7 的最小版）。
//!
//! 契约（§6.2 的 broker 只是*提议*，这里只做最小版，不做加权/轮询/drain）：
//!
//! - 槽位在服务表里的**提供者身份是注册表自己**（不是注册它的插件）⇒ `replace` 能过
//!   `Store::set` 的提供者守卫，而槽位的 `provider_uid` **不变**；
//! - 于是依赖者的 epoch 一动不动（A7：换适配器 ≠ 重启消费方），但它在自己的**提交视图**上
//!   读到的是**当下**的值——冻结的是身份，不是值；
//! - `put` 返回的 disposer 真的把条目从服务表里摘掉（依赖者跟着回到 Pending）；
//! - 空表、以及**已卸载**的注册表都必须优雅降级（可读错误，不是 panic，也不是"空值实现"）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use cordis_components::registry;
use cordis_core::{plugin_fn, App, Config, Ctx, Dyn, ExternValue, FiberState, Inject};
use serde_json::json;

/// 读槽位里的 `model` 字段（读不到就是 `"?"`）。
fn read_model(ctx: &Ctx) -> String {
    ctx.require_as::<ExternValue>("llm@1")
        .ok()
        .and_then(|value| {
            value
                .0
                .get("model")
                .and_then(|model| model.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "?".to_string())
}

fn value(model: &str) -> Dyn {
    Dyn::new(ExternValue(json!({ "model": model })))
}

static T1_APPLIES: AtomicU32 = AtomicU32::new(0);
static T1_READS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// M7-1（**A7**）：换适配器不改依赖者的 epoch，但新值必须可见。
///
/// 两个断言缺一不可：只断言 epoch 不变，空实现（什么都不做）也能过。
#[tokio::test]
async fn consumer_epoch_is_unchanged_when_a_named_impl_is_replaced() {
    let app = App::new();
    let registry_fiber = app.plugin(registry::plugin(), Config::Null).await.unwrap();
    assert_eq!(registry_fiber.state(), FiberState::Active);
    let handle = registry::handle(&app.root).expect("注册表必须已挂载");
    let disposer = handle.put("llm@1", value("a")).expect("槽位注册必须成功");

    // 消费方：声明 `inject("llm@1")`，并用自己的 ctx（提交视图）反复读值。
    let consumer = plugin_fn("agent-loop", Inject::of(&["llm@1"]), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            T1_APPLIES.fetch_add(1, Ordering::SeqCst);
            let mine = ctx.downgrade();
            ctx.on("t1/read", move |_call| {
                let mine = mine.clone();
                Box::pin(async move {
                    if let Some(ctx) = mine.upgrade() {
                        T1_READS.lock().unwrap().push(read_model(&ctx));
                    }
                    None
                })
            });
            Ok(())
        })
    });
    let consumer_fiber = app.plugin(consumer, Config::Null).await.unwrap();
    assert_eq!(consumer_fiber.state(), FiberState::Active);
    let epoch = consumer_fiber.running_epoch().expect("已激活 ⇒ 有 epoch");
    assert_eq!(T1_APPLIES.load(Ordering::SeqCst), 1);

    app.root.emit("t1/read", vec![]).await;

    // **原地换值**：一次同步写，没有任何注册/卸载动作。
    handle
        .replace("llm@1", value("b"))
        .expect("已注册的槽位必须能原地换值");
    consumer_fiber.settle().await; // 就算给它时间收敛，也不该重启

    assert_eq!(
        consumer_fiber.running_epoch(),
        Some(epoch),
        "换适配器不得改变依赖者的 epoch（A7）"
    );
    assert_eq!(
        consumer_fiber.desired_epoch(),
        Some(epoch),
        "连期望的 epoch 都不该动"
    );
    assert_eq!(T1_APPLIES.load(Ordering::SeqCst), 1, "不得重挂消费方");

    app.root.emit("t1/read", vec![]).await;
    assert_eq!(
        T1_READS.lock().unwrap().clone(),
        vec!["a".to_string(), "b".to_string()],
        "依赖者必须在**同一个提交视图**上读到当下的值"
    );

    disposer.run().await;
}

/// M7-2：`replace` 是**一次同步写**（`Store::set`），不是重新注册。
#[tokio::test]
async fn replacing_is_a_single_synchronous_write() {
    let app = App::new();
    let registry_fiber = app.plugin(registry::plugin(), Config::Null).await.unwrap();
    let handle = registry::handle(&app.root).unwrap();
    let _disposer = handle.put("llm@1", value("a")).unwrap();
    let epoch = registry_fiber.running_epoch();
    let effects = registry_fiber.effect_count();

    // 签名即证据：`replace` 是同步函数（没有 await 点）；同一 tick 内就能读到新值。
    handle.replace("llm@1", value("b")).unwrap();
    assert_eq!(read_model(&app.root), "b", "同步写 ⇒ 返回即可见");

    // 连写 5 次：不该累积出任何重建 —— 注册表自己既不重挂，effect 表也不长。
    for index in 0..5 {
        handle
            .replace("llm@1", value(&format!("m{index}")))
            .expect("反复原地换值都必须成功");
    }
    registry_fiber.settle().await;
    assert_eq!(
        registry_fiber.running_epoch(),
        epoch,
        "连写 5 次不得让注册表重挂"
    );
    assert_eq!(
        registry_fiber.effect_count(),
        effects,
        "原地换值不得增加 effect（它只是一次写，不是一次注册）"
    );
    assert_eq!(read_model(&app.root), "m4", "最后一次写赢");
    assert_eq!(
        handle.names(),
        vec!["llm@1".to_string()],
        "表里也不该多出条目"
    );
    // 原生句柄读到的同样是**当下**的值（不是注册时的快照）。
    let via_handle = handle
        .get("llm@1")
        .and_then(|value| value.downcast::<ExternValue>())
        .expect("句柄必须能读到槽位");
    assert_eq!(via_handle.0["model"], json!("m4"));
}

/// M7-3：`put` 返回的 disposer **真的摘掉条目**（依赖者跟着回到 Pending）。
#[tokio::test]
async fn disposer_removes_the_entry() {
    let app = App::new();
    app.plugin(registry::plugin(), Config::Null).await.unwrap();
    let handle = registry::handle(&app.root).unwrap();

    // 先挂消费方：槽位还没注册 ⇒ 依赖未齐、停在 Pending。
    let consumer = plugin_fn("consumer", Inject::of(&["llm@1"]), |_ctx: Ctx, _cfg| {
        Box::pin(async { Ok(()) })
    });
    let consumer_fiber = app.plugin(consumer, Config::Null).await.unwrap();
    assert_eq!(
        consumer_fiber.state(),
        FiberState::Pending,
        "槽位还没注册 ⇒ 依赖未齐"
    );

    let disposer = handle.put("llm@1", value("a")).unwrap();
    consumer_fiber.settle().await;
    assert_eq!(
        consumer_fiber.state(),
        FiberState::Active,
        "槽位就位 ⇒ 消费方被拉起"
    );
    assert_eq!(handle.names(), vec!["llm@1".to_string()]);
    assert_eq!(
        handle
            .get("llm@1")
            .and_then(|value| value.downcast::<ExternValue>())
            .map(|exposed| exposed.0["model"].clone()),
        Some(json!("a"))
    );

    disposer.run().await;

    assert!(
        app.root.require("llm@1").is_err(),
        "条目必须真的从服务表里消失"
    );
    assert!(handle.get("llm@1").is_none(), "摘除之后读不到值");
    assert!(handle.names().is_empty(), "表里也不该留下名字");
    consumer_fiber.settle().await;
    assert_eq!(
        consumer_fiber.state(),
        FiberState::Pending,
        "依赖者跟着回到未激活"
    );
}

/// M7-4：空表与已卸载的注册表都要**优雅降级**。
#[tokio::test]
async fn empty_registry_degrades_gracefully() {
    let app = App::new();
    let registry_fiber = app.plugin(registry::plugin(), Config::Null).await.unwrap();
    let handle = registry::handle(&app.root).expect("注册表必须已挂载");

    // 空表：三个入口都给出"没有"的可读答案，而不是 panic。
    assert!(handle.names().is_empty());
    assert!(handle.get("nope").is_none());
    let err = handle.replace("nope", value("a")).unwrap_err().to_string();
    assert!(
        err.contains("nope") && err.contains("put"),
        "换一个没注册过的槽位要说清怎么办：{err}"
    );

    // 卸载注册表：句柄还在调用方手里（`Arc`），也必须优雅降级——它只持 `Weak<Context>`。
    registry_fiber.dispose().await;
    assert!(handle.names().is_empty(), "卸载之后不得再报出槽位");
    assert!(handle.get("llm@1").is_none());
    let err = handle.put("llm@1", value("a")).unwrap_err().to_string();
    assert!(err.contains("注册表"), "卸载之后注册要说清原因：{err}");
}

/// M7-附加：typed/JSON 面（T2 走的那条路）与原生句柄**共用同一张表**。
#[tokio::test]
async fn typed_registry_face_shares_the_table_with_the_native_handle() {
    let app = App::new();
    app.plugin(registry::plugin(), Config::Null).await.unwrap();
    let handle = registry::handle(&app.root).unwrap();
    let _disposer = handle.put("llm@1", value("native")).unwrap();

    // JSON 面读得到原生侧注册的槽位。
    let got = app
        .root
        .call_iface("registry@1", 1, "get", json!(["llm@1"]))
        .await
        .unwrap();
    assert_eq!(got["model"], json!("native"));

    // JSON 面换值 ⇒ 原生侧立刻看到（同一张表、同一个实现）。
    app.root
        .call_iface(
            "registry@1",
            1,
            "replace",
            json!(["llm@1", { "model": "json" }]),
        )
        .await
        .unwrap();
    assert_eq!(read_model(&app.root), "json");

    // 空表查询在 JSON 面上是 `null`（不是错误）。
    let missing = app
        .root
        .call_iface("registry@1", 1, "get", json!(["mcp@1"]))
        .await
        .unwrap();
    assert!(missing.is_null());

    // JSON 面注册的槽位，原生侧同样看得见。
    let put = app
        .root
        .call_iface(
            "registry@1",
            1,
            "put",
            json!(["mcp@1", { "url": "http://x" }]),
        )
        .await
        .unwrap();
    assert!(put.is_null(), "`put` 的返回是 `null`（Rust 的 `()`）");
    assert_eq!(
        handle.names(),
        vec!["llm@1".to_string(), "mcp@1".to_string()]
    );

    let names = app
        .root
        .call_iface("registry@1", 1, "names", json!([]))
        .await
        .unwrap();
    assert_eq!(names, json!(["llm@1", "mcp@1"]));

    // 方法不存在 ⇒ 列出可用方法（M6 的适配器合同，这里顺带验收）。
    let err = app
        .root
        .call_iface("registry@1", 1, "nope", json!([]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("replace") && err.contains("nope"), "{err}");
}
