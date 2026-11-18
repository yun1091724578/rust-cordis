//! >1 MiB 的**业务服务值**必须能跨 ABI 读回（O8 验收回归）。
//!
//! 背景：宿主对 `provide`/`set` 的**写入**没有字节上限，值多大都照收；而插件读回
//! 它时走的是 `ctx.get` / `ctx.require` 的 `STATUS_TOO_SMALL` 扩容重试。O8 一度
//! 给这条重试加了 1 MiB 上限（`MAX_RESPONSE_RETRY_BYTES`），于是合法的大服务值
//! **改前读得回、改后读不回**；更糟的是 `Ctx::get` 用 `.ok()?` 把"值太大"静默
//! 压成 `None`——插件读到的是"服务不存在"，而不是一条能被诊断的错误。
//!
//! 这里的形态就是 ABI 层的等价验证：宿主先把 1.5 MiB 的值写进 store，再让一个
//! **走真实 SDK + 真实 `HostApi`** 的插件 apply 通过 `ctx.get` / `ctx.require`
//! 读回，并把读到的东西原样 `provide` 回来——宿主拿回的字节必须与写进去的**逐字
//! 相同**。`ctx.require` 的真 DLL 覆盖没有示例插件可用（`hello` 只用了 `ctx.get`），
//! 所以它在这一层补上；真 DLL 的 `ctx.get` 覆盖见
//! `tests/consumer-service/tests/abi_service_large_value.rs`。

use cordis_abi::sdk;
use cordis_abi::{Blob, HostApi, Out};
use cordis_core::{App, Config, Ctx, Dyn, ExternValue, FnPlugin, Inject, Plugin};
use cordis_host::{apply_via_with_error_format, host_api, CallFn};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 宿主侧 `ctx_get` / `ctx_require` 被**调用了几次**。
///
/// 扩容重试在宿主侧是可数的：每次重试都是一次真实调用（起始 16 KiB 缓冲 ⇒
/// `STATUS_TOO_SMALL` ⇒ 按申报长度再调一次）。所以这两个计数就是"重试确实发生过"
/// 的可观测证据，不需要另造一条只有测试才走的旁路。
static GET_CALLS: AtomicUsize = AtomicUsize::new(0);
static REQUIRE_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn counting_get(handle: u64, name: Blob, out: *mut Out) -> i32 {
    GET_CALLS.fetch_add(1, Ordering::SeqCst);
    match host_api().ctx_get {
        Some(real) => real(handle, name, out),
        None => cordis_abi::STATUS_UNSUPPORTED,
    }
}

unsafe extern "C" fn counting_require(handle: u64, name: Blob, out: *mut Out) -> i32 {
    REQUIRE_CALLS.fetch_add(1, Ordering::SeqCst);
    match host_api().ctx_require {
        Some(real) => real(handle, name, out),
        None => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// 真实宿主函数表的**计数包装**：只把 `ctx_get` / `ctx_require` 换成计数版，
/// 其余字段原样转发——插件走的仍是真实宿主路径。
fn counting_host() -> &'static HostApi {
    // SAFETY：与 `sdk::attach` 的 copy-on-attach 同一手法——源是一份仍有效的静态
    // 完整函数表，目的端是本次泄漏出去的独立副本，两者不重叠。
    let mut table = HostApi::default();
    unsafe {
        std::ptr::copy_nonoverlapping(host_api(), &mut table, 1);
        table.ctx_get = Some(counting_get);
        table.ctx_require = Some(counting_require);
    }
    Box::leak(Box::new(table))
}

/// 1.5 MiB：刚好越过 O8 误加到插件侧扩容重试上的 1 MiB 上限。
const PAYLOAD_BYTES: usize = 1_572_864;

/// 首尾各带一枚标记，长度恰好 `PAYLOAD_BYTES`：截断、补零、换值都能被看出来。
fn payload() -> String {
    let head = "BEGIN-";
    let tail = "-END";
    let mut text = String::with_capacity(PAYLOAD_BYTES);
    text.push_str(head);
    text.push_str(&"a".repeat(PAYLOAD_BYTES - head.len() - tail.len()));
    text.push_str(tail);
    assert_eq!(text.len(), PAYLOAD_BYTES);
    text
}

/// 一条短诊断：大字符串绝不能整段进断言消息（失败输出会变成几 MB）。
fn brief(text: &str) -> String {
    let head: String = text.chars().take(24).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{} 字节，头 {head:?}，尾 {tail:?}", text.len())
}

fn service_of(app: &App, name: &str) -> Value {
    let value = app
        .root
        .require_as::<ExternValue>(name)
        .unwrap_or_else(|err| panic!("读跨 ABI 服务 {name} 失败: {err}"));
    value.0.clone()
}

#[tokio::test]
async fn a_service_value_over_one_mib_round_trips_through_get_and_require() {
    GET_CALLS.store(0, Ordering::SeqCst);
    REQUIRE_CALLS.store(0, Ordering::SeqCst);
    unsafe { sdk::attach(counting_host()) };
    let text = payload();

    // 宿主侧写入：没有字节上限，值多大都照收。
    let desc = sdk::build_plugin(
        "sdk-large-value",
        &["big.value"],
        move |ctx: &sdk::Ctx, _config: sdk::Json| {
            let via_get = ctx.get("big.value");
            let len_of = |value: &Value| value.get("text").and_then(Value::as_str).map(str::len);
            ctx.provide(
                "probe.get",
                json!({ "ok": via_get.is_some(), "len": via_get.as_ref().and_then(len_of) }),
            )?;
            if let Some(value) = via_get {
                ctx.provide("probe.get.echo", value)?;
            }

            match ctx.require_detailed("big.value") {
                Ok(value) => {
                    ctx.provide(
                        "probe.require",
                        json!({ "ok": true, "len": len_of(&value) }),
                    )?;
                    ctx.provide("probe.require.echo", value)?;
                }
                Err(err) => ctx.provide(
                    "probe.require",
                    json!({ "ok": false, "code": err.code, "error": err.error }),
                )?,
            }
            Ok(())
        },
    );
    let call: CallFn = unsafe { (*desc).call };
    let error_format = unsafe { cordis_abi::PluginDesc::error_format_at(desc) };

    let app = App::new();
    let _service = app
        .root
        .provide("big.value", Dyn::new(ExternValue(json!({ "text": text }))))
        .expect("宿主写入大服务值不该失败");

    let plugin: Arc<dyn Plugin> = Arc::new(FnPlugin {
        name: "sdk-large-value",
        key: cordis_core::unique_key("sdk-large-value"),
        inject: Inject::of(&["big.value"]),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, "sdk-large-value", error_format)
        }),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    let error = fiber.error();
    assert!(
        error.is_none(),
        "插件 apply 必须成功（读大值不该是错误）：{error:?}"
    );

    // ① 两条读取入口都必须拿到值：`ctx.get` 不得把"值太大"吞成 None，
    //    `ctx.require` 不得报 `host_too_small`。
    let get = service_of(&app, "probe.get");
    let require = service_of(&app, "probe.require");
    assert_eq!(
        (get["ok"].clone(), require["ok"].clone()),
        (json!(true), json!(true)),
        ">1 MiB 的业务服务值必须读得回：get={get} require={require}"
    );
    assert_eq!(get["len"], json!(PAYLOAD_BYTES), "ctx.get 读回的长度不对");
    assert_eq!(
        require["len"],
        json!(PAYLOAD_BYTES),
        "ctx.require 读回的长度不对"
    );

    // ② 内容逐字相同：插件把读到的值原样 provide 回来，宿主再对一遍。
    let echo = service_of(&app, "probe.get.echo");
    let echoed = echo["text"]
        .as_str()
        .unwrap_or_else(|| panic!("ctx.get 读回的不是 {{text: …}}: {echo}"));
    assert_eq!(
        echoed.len(),
        PAYLOAD_BYTES,
        "ctx.get 读回的内容被截断：{}",
        brief(echoed)
    );
    assert!(
        echoed == text,
        "ctx.get 读回的内容不完整：{}",
        brief(echoed)
    );

    // ③ `ctx.require` 的内容同样逐字相同。
    let echoed = service_of(&app, "probe.require.echo")["text"]
        .as_str()
        .map(str::to_string)
        .expect("ctx.require 读回的不是 {text: …}");
    assert!(
        echoed == text,
        "ctx.require 读回的内容不完整：{}",
        brief(&echoed)
    );

    // ④ 扩容重试**确实发生过**：起始缓冲 16 KiB 放不下 1.5 MiB，所以每条读取入口都
    //    要读两次宿主（第一次 `STATUS_TOO_SMALL` + 申报长度，第二次真正写出来）。
    //    宿主侧调用次数就是这条链路的可观测量。将来若把起始缓冲放大到"一次就够"，
    //    这里会失败——那正是用例已静默退化成"没测重试"的信号。
    assert_eq!(
        (
            GET_CALLS.load(Ordering::SeqCst),
            REQUIRE_CALLS.load(Ordering::SeqCst)
        ),
        (2, 2),
        "ctx.get / ctx.require 各应读宿主两次（起始缓冲 + 一次扩容重试）；\
         只读到一次说明扩容重试没有被覆盖"
    );
}
