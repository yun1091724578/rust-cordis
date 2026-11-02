//! 示例插件 B：依赖另一个动态库，并**调用它的服务方法**。
//!
//! 这是"任意第三方插件"最该被验证的形态：
//! - `greeting` 没有装载 → 本插件停在未激活状态，**不报错**；
//! - `greeting` 装载了 → 宿主自动把它拉起来；
//! - 起来之后，它会通过 ABI **跨边界调用 `greeting` 的方法**；
//! - `greeting` 被卸载（或热重载）→ 本插件自动卸载并重载。

use serde_json::{json, Value};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

static ACTIVATIONS: AtomicU64 = AtomicU64::new(0);

/// 当前 volatile `note` 的值（读宿主推过来的副本；没声明/没值就是 `<无>`）。
fn volatile_note() -> String {
    cordis_abi::sdk::volatile("note")
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "<无>".to_string())
}

cordis_abi::cordis_plugin! {
    name: "hello",
    inject: ["greeting"],
    // M4-5：`note` 是 **volatile** 字段——只改它的配置变更**不会重挂本插件**
    // （宿主把新值推过来，本插件重新读副本）。其余字段变了照旧重挂。
    volatile: ["note"],
    apply: |ctx, config| {
        let times = config.get("times").and_then(Value::as_u64).unwrap_or(1);
        let inline_slot: Arc<Mutex<String>> = Arc::new(Mutex::new("<还没调用>".to_string()));

        // ① 纯数据服务的读取。
        let greeting = ctx
            .get("greeting.text")
            .and_then(|value| value.get("text").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "<缺失>".to_string());
        // 诊断：激活序号。用于确认"apply 是不是被跑了多次"。
        // 真插件不需要这个，这里保留它是因为它把跨 ABI 生命周期变成了可读的轨迹。
        let serial = ACTIVATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        // volatile 值的读法（M4-5）：随取随读，不自己缓存——宿主推新值后读到的就是新的。
        let note = volatile_note();
        ctx.log(
            1,
            &format!("hello: 激活 #{serial}，greeting.text = {greeting:?}，times = {times}，note = {note:?}"),
        );
        ctx.provide("hello.note", json!({ "note": note }))?;

        // volatile 推送之后宿主会发 `loader/volatile-update`（owner-filter：本插件在那一行的
        // 子树里，所以收得到）。这里**重新读**副本并更新服务值——ABI 侧的"引用"就是这样用的：
        // 值由宿主推，插件只在被告知时读。
        let note_ctx = *ctx;
        ctx.on("loader/volatile-update", move |_args| {
            let note = volatile_note();
            eprintln!("[hello] volatile note → {note:?}（实例没重挂）");
            note_ctx.set("hello.note", json!({ "note": note }))?;
            Ok(json!({ "message": format!("hello.note 已更新为 {note:?}") }))
        })?;

        // ② **跨 ABI 的服务方法调用**，逻辑上就是 `greeting.shout({who})`。
        //
        //    宿主会先尝试**同步完成**：提供方的接口方法是同步函数，所以这里必定走
        //    同步快速路径，续延会在 `call_iface` 返回之前就被调用。
        //    下面第 ③ 步直接读 `inline_slot` 就是对这一点的断言——
        //    如果宿主走了异步兜底，读到的还会是初始值。
        let slot = inline_slot.clone();
        ctx.call_iface("greeting", 2, "shout", json!({ "who": "hello" }), move |result| {
            let rendered = match result {
                Ok(value) => value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("<没有 text 字段>")
                    .to_string(),
                Err(message) => format!("调用失败: {message}"),
            };
            *slot.lock().unwrap() = rendered;
        })?;

        // ③ 立刻读：能读到值就证明走的是同步快速路径。
        let inline = inline_slot.lock().unwrap().clone();

        // 顺带验证版本协商：拿 v3（高于提供方的 v2）必须被**明确拒绝**。
        let version_probe: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let probe_slot = version_probe.clone();
        ctx.call_iface("greeting", 3, "text", json!({}), move |result| {
            *probe_slot.lock().unwrap() = match result {
                Ok(_) => "意外：高版本请求居然成功了".to_string(),
                Err(message) => message,
            };
        })?;
        let version_error = version_probe.lock().unwrap().clone();

        // ④ 监听事件（宿主在派发时同步回调进来）。
        let for_listener = greeting.clone();
        ctx.on("hello/ping", move |args| {
            let who = args
                .first()
                .and_then(|value| value.get("who"))
                .and_then(Value::as_str)
                .unwrap_or("nobody");
            eprintln!("[hello] 收到 hello/ping，来自 {who}");
            Ok(json!({ "message": format!("{for_listener} → {who}") }))
        })?;

        // M5-1：演示"坏插件行为"——监听器里直接 panic。
        // SDK 会把它截在 ABI 边界**之内**（"C" ABI 不可展开，逃出去就是整个进程 abort），
        // 宿主只看到一条可读错误：进程、邻居监听者、以及这一行本身都不受影响。
        ctx.on("hello/boom", |_args: Vec<Value>| -> Result<Value, String> {
            panic!("演示：监听器里 panic")
        })?;

        // ⑤ 派发**有返回值**的事件：C ABI 无法 await 的那类操作，走"发起 + 续延"。
        //    注意顺序：先注册自己的监听器，再派发，这样 bail 的结果里就包含本插件
        //    自己贡献的那一个值——于是"跨 ABI 的有返回值派发"被完整验证了一遍。
        let dispatch_slot: Arc<Mutex<String>> = Arc::new(Mutex::new("<未派发>".to_string()));
        let dispatch_probe = dispatch_slot.clone();
        ctx.dispatch("hello/ping", "bail", vec![json!({ "who": "self" })], move |result| {
            *dispatch_probe.lock().unwrap() = match &result {
                Ok(value) => value.to_string(),
                Err(message) => format!("派发失败: {message}"),
            };
        })?;
        let dispatch_result = dispatch_slot.lock().unwrap().clone();

        ctx.provide(
            "hello.reply",
            json!({
                "message": format!("{greeting} × {times}"),
                "inline_call": inline,
                "version_error": version_error,
                "dispatch_result": dispatch_result,
            }),
        )?;

        ctx.effect(move || {
            eprintln!("[hello] cleanup 回调被执行（依赖失效导致的卸载同样会走到这里）");
        })?;
        Ok(())
    },
}
