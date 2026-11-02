//! 示例插件 A：提供一个纯数据服务，同时提供**一个带方法的接口**。
//!
//! 它的存在是为了证明三件事：
//! 1. 一个动态库可以往宿主里注册服务并被其它动态库消费；
//! 2. 服务可以**有方法**（`greeting.text` / `greeting.shout`），不只是 JSON 值——
//!    这是插件生态真正成立的前提；
//! 3. 卸载它会让所有依赖者**自动**卸载，不需要任何手动编排。

use serde_json::{json, Value};
use std::sync::Arc;

cordis_abi::cordis_plugin! {
    name: "greeting",
    apply: |ctx, config| {
        let text = config
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("hello from the abi")
            .to_string();
        ctx.log(1, &format!("greeting: 装载，提供 greeting = {text:?}"));

        // 跨 ABI 的服务值统一是 JSON。宿主只认 `ExternValue`，
        // 非 JSON 化的 Rust 类型在 `ctx.get` 时会得到一条**可执行的**错误提示。
        ctx.provide("greeting.text", json!({ "text": text }))?;

        // 带方法的接口。`version` 是接口版本：**接口只能追加方法**，
        // 所以用 v1 编译的调用方在 v2 的提供方上照常工作。
        let for_text = text.clone();
        let for_shout = text.clone();
        ctx.provide_iface(
            "greeting",
            2,
            vec![
                (
                    "text",
                    Arc::new(move |_args: Value, _meta: Option<Value>| Ok(json!({ "text": for_text }))),
                ),
                (
                    "shout",
                    Arc::new(move |args: Value, _meta: Option<Value>| {
                        let who = args.get("who").and_then(Value::as_str).unwrap_or("world");
                        Ok(json!({ "text": format!("{} {who}!", for_shout.to_uppercase()) }))
                    }),
                ),
                (
                    "stats",
                    Arc::new(move |_args: Value, _meta: Option<Value>| {
                        // 这个方法比 text 晚引入。v1 的调用方看不到它也不受影响——
                        // 这就是"接口只追加"带来的兼容性。
                        Ok(json!({ "methods": ["text", "shout", "stats", "audit"], "version": 2 }))
                    }),
                ),
                (
                    // M3：把**提供者侧读到的**合并元数据回抛给调用方。
                    //
                    // 它经 ABI 载荷的可选字段 `meta` 到达 SDK（`IfaceFn` 的第二个参数）：
                    // `d(k) ⊕ ι(k)`，右偏——调用者的 `inject` 声明被 context 携带的
                    // intercept 元数据覆盖。内核只传递，怎么用是提供者的事。
                    "audit",
                    Arc::new(move |_args: Value, meta: Option<Value>| Ok(json!({ "meta": meta }))),
                ),
            ],
        )?;

        ctx.effect(move || {
            eprintln!("[greeting] cleanup 回调被执行（说明 fiber 卸载时回调和句柄都被回收）");
        })?;
        Ok(())
    },
}
