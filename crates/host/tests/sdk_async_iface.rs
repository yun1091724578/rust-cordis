//! **可挂起的接口方法**（ADR 0001 §4 的"待补"项）：T2 插件提供的服务方法也能挂起。
//!
//! # 为什么这条现在就要有
//! harness 里最自然的写法是**服务**而不是事件：模型适配器、工具、会话日志都是
//! "有个方法可以调"，而它们**天然是异步的**（网络、子进程、磁盘）。ABI v2 的异步入站
//! 早就支持"一次挂起 + 一次结算"（`STATUS_PENDING` + `ctx_fulfill`），宿主侧也一直在
//! 等它（`PluginIface::call` → `pending.settle("接口方法")`）——缺的只是 **SDK 面**：
//! `provide_iface` 只登记同步方法体（`Handler`），插件没法把一个接口方法答成"稍后给"。
//!
//! # 为什么这里只有**一个**测试
//! `sdk::build_plugin_*` 的 apply 槽是**进程级**的（真插件走 `cordis_plugin!`，每个宏调用
//! 生成自己的入口，不受影响；进程内构造的 ABI 形状插件则一次只该有一个）。
//! 于是"一个进程一个插件" ⇒ 一个测试覆盖全部七条断言，而不是三个测试各造一个。
//!
//! 断言七条：
//! 1. 挂起路径：方法答 `AsyncReply::Pending` ⇒ 宿主等 `ctx.fulfill` ⇒ 值原路返回，且**挂起表归零**；
//! 2. 同步路径零变更：同一个接口里的另一种方法照旧立即返回；
//! 3. 同一条异步通道也能**立刻**给答案（`AsyncReply::Reply`，不挂起）；
//! 4. 超过内联 16 KiB 上限的结果必须显式失败，不能伪装成功或重放；
//! 5. 方法名是契约：未知方法要列出可用方法；
//! 6. 版本协商照旧：v2 请求打到 v1 上被明确拒绝；
//! 7. 卸载排水：还挂着接口调用的插件被拆掉 ⇒ 那次调用**以错误结算**（不是永远挂着）。
//!
//! 夹具纪律（D10）：插件体里**只记账不 assert**（panic 会被 SDK 截成 STATUS_ERR）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis_abi::sdk;
use cordis_core::{App, Config, FiberState, Inject, Plugin};
use cordis_host::{apply_via_with_error_format, host_api, pending_len, CallFn};
use serde_json::{json, Value};

async fn until<F: Fn() -> bool>(what: &str, check: F) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("等待超时：{what}");
}

#[tokio::test]
async fn suspendable_iface_methods_share_everything_with_the_event_path() {
    // ---------------------------------------------------------------- 夹具
    let armed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let quick = Arc::new(AtomicU64::new(0));
    let handle = Arc::new(AtomicU64::new(0));
    let release_event = "slow/release";

    unsafe { sdk::attach(host_api()) };
    let desc = {
        let armed = armed.clone();
        let quick = quick.clone();
        let handle = handle.clone();
        sdk::build_plugin_with_volatile(
            "sdk-async-iface",
            &[],
            &[],
            move |ctx: &sdk::Ctx, _config| {
                let ctx = *ctx;
                handle.store(ctx.handle(), Ordering::SeqCst);
                // 外层是 `Fn`（每个实例都要能再跑一次）⇒ 内层闭包用到的句柄一律**每次调用现克隆**。
                let armed_work = armed.clone();
                let quick_work = quick.clone();
                ctx.provide_iface_async(
                    "slow",
                    1,
                    vec![
                        (
                            // 可挂起：记下 `dispatch_id`，稍后由放行事件结算。
                            "work",
                            Arc::new(move |args: Value, _meta: Option<Value>, dispatch_id: u64| {
                                if args.get("immediate").and_then(Value::as_bool) == Some(true) {
                                    // 同一条通道也能**立刻**给答案（不必挂起）。
                                    return Ok(sdk::AsyncReply::Reply(
                                        json!({ "immediate": true }),
                                    ));
                                }
                                armed_work.lock().unwrap().push(dispatch_id);
                                Ok(sdk::AsyncReply::Pending)
                            }),
                        ),
                        (
                            // 同一个接口里的同步方法：照旧立即返回。
                            "quick",
                            Arc::new(
                                move |_args: Value, _meta: Option<Value>, _dispatch_id: u64| {
                                    quick_work.fetch_add(1, Ordering::SeqCst);
                                    Ok(sdk::AsyncReply::Reply(json!({ "quick": true })))
                                },
                            ),
                        ),
                        (
                            // 超过内联 16 KiB 上限的结果：必须报可读错误，不能伪装成功。
                            "big",
                            Arc::new(
                                move |_args: Value, _meta: Option<Value>, _dispatch_id: u64| {
                                    Ok(sdk::AsyncReply::Reply(json!({
                                        "data": "x".repeat(20_000)
                                    })))
                                },
                            ),
                        ),
                    ],
                )?;
                let armed_release = armed.clone();
                let ctx_release = ctx;
                ctx.on(release_event, move |_args: Vec<Value>| {
                    // 取锁 → 放锁 → 再结算（持锁回调宿主 = 同线程死锁）。
                    let ids: Vec<u64> = {
                        let mut guard = armed_release.lock().unwrap();
                        guard.drain(..).collect()
                    };
                    for id in ids {
                        let _ = ctx_release.fulfill(id, json!({ "done": true, "by": "slow@1" }));
                    }
                    Ok(json!(null))
                })?;
                Ok(())
            },
        )
    };
    let call: CallFn = unsafe { (*desc).call };
    let error_format = unsafe { cordis_abi::PluginDesc::error_format_at(desc) };
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "sdk-async-iface",
        key: cordis_core::unique_key("sdk-async-iface"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: cordis_core::Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, "sdk-async-iface", error_format)
        }),
    });
    let app = App::new();
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(
        fiber.state(),
        FiberState::Active,
        "ABI 形状插件应当装载成功"
    );
    // D10：句柄由插件自报，不断言全局计数。
    let handle = handle.load(Ordering::SeqCst);
    assert!(handle > 0, "插件必须自报实例句柄");

    // ---------------------------------------------------------------- ① 挂起 → 结算
    assert_eq!(pending_len(handle), 0, "起步时没有在飞派发");
    let call = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.call_iface("slow", 1, "work", json!({ "n": 1 })).await })
    };
    until("接口方法进入挂起", || pending_len(handle) == 1).await;
    assert_eq!(armed.lock().unwrap().len(), 1, "插件侧也应记下这次挂起");

    app.root.emit(release_event, vec![]).await;
    let answer = call
        .await
        .expect("调用任务不得 panic")
        .expect("调用应当成功");
    assert_eq!(
        answer,
        json!({ "done": true, "by": "slow@1" }),
        "结算的值必须原路返回"
    );
    assert_eq!(pending_len(handle), 0, "结算之后挂起表必须归零");

    // ---------------------------------------------------------------- ② 同步方法不受影响
    let answer = app
        .root
        .call_iface("slow", 1, "quick", json!([]))
        .await
        .expect("同步方法应当成功");
    assert_eq!(answer, json!({ "quick": true }));
    assert_eq!(quick.load(Ordering::SeqCst), 1, "方法体真的跑了");
    assert_eq!(pending_len(handle), 0, "同步路径不该留下挂起");

    // ---------------------------------------------------------------- ③ 不挂起也能立刻答复
    let answer = app
        .root
        .call_iface("slow", 1, "work", json!({ "immediate": true }))
        .await
        .expect("立刻答复的异步方法应当成功");
    assert_eq!(answer, json!({ "immediate": true }));
    assert_eq!(pending_len(handle), 0);

    // ------------------------------------------------ ③b 超过内联上限的结果必须显式失败
    let message = tokio::time::timeout(
        Duration::from_secs(5),
        app.root.call_iface("slow", 1, "big", json!([])),
    )
    .await
    .expect("大结果必须在时限内结算")
    .expect_err("超过 16 KiB 的内联结果不能伪装成功")
    .to_string();
    assert!(
        message.contains("16 KiB") && message.contains("不可重试"),
        "必须说明容量上限与不重放语义：{message}"
    );
    assert_eq!(pending_len(handle), 0, "失败的内联结果不得残留挂起");

    // ---------------------------------------------------------------- ④ 未知方法列出可用方法
    let message = app
        .root
        .call_iface("slow", 1, "nope", json!([]))
        .await
        .expect_err("未知方法必须失败")
        .to_string();
    assert!(message.contains("nope"), "{message}");
    assert!(
        message.contains("work") && message.contains("quick"),
        "要列出可用方法：{message}"
    );

    // ---------------------------------------------------------------- ⑤ 版本协商照旧
    let message = app
        .root
        .call_iface("slow", 2, "work", json!([]))
        .await
        .expect_err("高版本请求必须失败")
        .to_string();
    assert!(message.contains("版本"), "{message}");

    // ---------------------------------------------------------------- ⑥ 卸载排水
    // 再挂一次，然后把插件拆掉：排水路径必须让这次挂起**以错误结算**。
    let call = {
        let ctx = app.root.clone();
        tokio::spawn(async move { ctx.call_iface("slow", 1, "work", json!({})).await })
    };
    until("接口方法再次进入挂起", || {
        pending_len(handle) == 1
    })
    .await;
    fiber.dispose().await;
    let outcome = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("卸载必须让挂起的调用结算（否则就是泄漏）")
        .expect("调用任务不得 panic");
    let message = match outcome {
        Ok(value) => format!("意外成功：{value}"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains("卸载") || message.contains("已取消"),
        "结算必须说明原因（插件已卸载）：{message}"
    );
    assert_eq!(pending_len(handle), 0, "排水之后挂起表必须归零");
}
