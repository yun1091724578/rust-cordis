//! M3：use-site 元数据在**调用时**到达提供者（跨 ABI 载荷的可选字段 `meta`）。
//!
//! 通道 ①（论文的）：调用者侧 `Context::call_iface` 在派发前算
//! `merge(d_caller, ι_caller)`，经 `OP_CALLBACK` 载荷的可选字段 `meta` 下发，
//! 供**提供者**在被调用时查阅（策略留给提供者，内核不做裁决）。
//!
//! 两条性质在这里钉死：
//! - **use-site**：元数据取自**调用点**那个派生 ctx（`ctx_intercept` 派生的），
//!   所以插件用派生句柄调用时，`ctx_call_iface` 必须按派生 ctx 解析；
//! - **可选字段**：完全没有元数据时整个字段不出现（旧插件与旧宿主都无需改动）。
//!
//! 全部无 DLL（D9）：ABI 插件经 [`cordis_host::apply_via`] + 手写 `CallFn` 构造，
//! 宿主函数经 [`cordis_host::host_api`] 的**真表**调用。
//! 夹具铁律（D10）：fake `CallFn` 内**禁止 assert**，只把结局记进 static `Mutex`。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_OK, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Ctx, FiberState, Inject, Plugin};
use cordis_host::{apply_via, host_api};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 提供者侧方法分发器的回调 id（本测试自选；SDK 场景由注册表铸造）。
const IFACE_ID: u64 = 7;
/// 调用者侧的续延 id。
const CONTINUATION: u64 = 11;

/// 提供者看到的**每一次** iface 派发的完整载荷（顺序即调用顺序）。
static SEEN: Mutex<Vec<Value>> = Mutex::new(Vec::new());
/// 消费者 A（带 use-site）的宿主调用日志：`label:status:应答文本`。
static A_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 消费者 B（无元数据）的宿主调用日志。
static B_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn abi_plugin_with(
    name: &'static str,
    call: cordis_host::CallFn,
    inject: Inject,
) -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject,
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| apply_via(call, ctx, config, name)),
    })
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时：{what}");
}

/// 提供者：激活时暴露 `svc`（方法 `hello`），并记下每次方法派发的完整载荷。
unsafe extern "C" fn provider_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let host = host_api();
            let mut raw = [0u8; 1024];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            match host.ctx_provide_iface {
                Some(provide) => provide(
                    handle,
                    Blob::new(b"svc"),
                    1,
                    Blob::new(br#"["hello"]"#),
                    IFACE_ID,
                    &mut probe,
                ),
                None => STATUS_UNSUPPORTED,
            }
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            SEEN.lock().unwrap().push(payload);
            (*out).write(br#"{"ret":null}"#)
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 消费者 A：先在**派生 ctx** 上叠一层 `intercept`，再用派生句柄调用服务。
unsafe extern "C" fn site_consumer_call(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let host = host_api();
            let mut raw = [0u8; 1024];
            let mut probe = Out {
                ptr: raw.as_mut_ptr(),
                cap: raw.len(),
                len: 0,
            };
            match host.ctx_intercept {
                Some(intercept) => {
                    let status = intercept(
                        handle,
                        Blob::new(b"svc"),
                        Blob::new(br#"{"b":2}"#),
                        &mut probe,
                    );
                    A_LOG.lock().unwrap().push(format!(
                        "intercept:{status}:{}",
                        String::from_utf8_lossy(&raw[..probe.len])
                    ));
                    let derived = serde_json::from_slice::<Value>(&raw[..probe.len])
                        .ok()
                        .and_then(|answer| answer.get("handle").and_then(Value::as_u64));
                    match (status, derived, host.ctx_call_iface) {
                        (STATUS_OK, Some(derived), Some(call)) => {
                            let mut raw = [0u8; 1024];
                            let mut probe = Out {
                                ptr: raw.as_mut_ptr(),
                                cap: raw.len(),
                                len: 0,
                            };
                            let status = call(
                                derived,
                                Blob::new(b"svc"),
                                1,
                                Blob::new(b"hello"),
                                Blob::new(b"null"),
                                CONTINUATION,
                                &mut probe,
                            );
                            A_LOG.lock().unwrap().push(format!(
                                "call:{status}:{}",
                                String::from_utf8_lossy(&raw[..probe.len])
                            ));
                        }
                        _ => A_LOG.lock().unwrap().push("call:missing".into()),
                    }
                }
                None => A_LOG.lock().unwrap().push("intercept:none".into()),
            }
            (*out).write(b"")
        }
        OP_CALLBACK => (*out).write(br#"{"ret":null}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 消费者 B：从**自己的实例句柄**直接调用（没有任何 use-site 层，也没有声明配置）。
unsafe extern "C" fn plain_consumer_call(
    handle: u64,
    op: u32,
    _: Blob,
    _: Blob,
    out: *mut Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let host = host_api();
            match host.ctx_call_iface {
                Some(call) => {
                    let mut raw = [0u8; 1024];
                    let mut probe = Out {
                        ptr: raw.as_mut_ptr(),
                        cap: raw.len(),
                        len: 0,
                    };
                    let status = call(
                        handle,
                        Blob::new(b"svc"),
                        1,
                        Blob::new(b"hello"),
                        Blob::new(b"null"),
                        CONTINUATION,
                        &mut probe,
                    );
                    B_LOG.lock().unwrap().push(format!(
                        "call:{status}:{}",
                        String::from_utf8_lossy(&raw[..probe.len])
                    ));
                }
                None => B_LOG.lock().unwrap().push("call:none".into()),
            }
            (*out).write(b"")
        }
        OP_CALLBACK => (*out).write(br#"{"ret":null}"#),
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// M3 红测试 1（跨 ABI）：use-site 元数据在调用时到达提供者；没有元数据时字段不出现。
#[tokio::test]
async fn use_site_metadata_reaches_the_provider_at_call_time() {
    SEEN.lock().unwrap().clear();
    A_LOG.lock().unwrap().clear();
    B_LOG.lock().unwrap().clear();

    let app = App::new();
    let provider_fiber = app.root.plugin(
        abi_plugin_with("m3-provider", provider_call, Inject::default()),
        Config::Null,
    );
    provider_fiber.settle().await;
    assert_eq!(
        provider_fiber.state(),
        FiberState::Active,
        "提供者应装载成功"
    );

    // 消费者 A：行级声明 `{ a: 1 }` + 调用点上叠的 intercept 层 `{ b: 2 }`。
    let consumer_a = app.root.plugin(
        abi_plugin_with(
            "m3-consumer-a",
            site_consumer_call,
            Inject(vec![("svc".into(), json!({ "a": 1 }))]),
        ),
        Config::Null,
    );
    consumer_a.settle().await;
    assert_eq!(
        consumer_a.state(),
        FiberState::Active,
        "消费者 A 应装载成功；宿主调用日志：{:?}",
        A_LOG.lock().unwrap()
    );
    until("提供者收到 A 的调用", || {
        SEEN.lock().unwrap().len() == 1
    })
    .await;

    let log_a = A_LOG.lock().unwrap().clone();
    assert!(
        log_a[0].starts_with("intercept:0:"),
        "ctx_intercept 必须成功：{log_a:?}"
    );
    assert!(
        log_a[1].starts_with("call:0:"),
        "ctx_call_iface 必须成功（派生句柄要能调用）：{log_a:?}"
    );
    let answer: Value =
        serde_json::from_str(log_a[1].trim_start_matches("call:0:")).expect("应答是 JSON");
    assert_eq!(
        answer["pending"],
        json!(false),
        "同步快速路径：结果内联带回"
    );

    // 消费者 B：同一个服务、同样的调用形态，但没有任何元数据可下发。
    let consumer_b = app.root.plugin(
        abi_plugin_with("m3-consumer-b", plain_consumer_call, Inject::of(&["svc"])),
        Config::Null,
    );
    consumer_b.settle().await;
    assert_eq!(
        consumer_b.state(),
        FiberState::Active,
        "消费者 B 应装载成功；宿主调用日志：{:?}",
        B_LOG.lock().unwrap()
    );
    until("提供者收到 B 的调用", || {
        SEEN.lock().unwrap().len() == 2
    })
    .await;

    let seen = SEEN.lock().unwrap().clone();
    assert_eq!(seen[0]["kind"], json!("iface"));
    assert_eq!(seen[0]["event"], json!("svc"));
    assert_eq!(seen[0]["method"], json!("hello"));
    assert_eq!(
        seen[0]["meta"],
        json!({ "a": 1, "b": 2 }),
        "use-site 的 b=2 与声明的 a=1 都要到达提供者：{}",
        seen[0]
    );
    assert!(
        seen[1].get("meta").is_none(),
        "完全没有元数据时 `meta` 字段**不出现**（旧插件忽略未知字段即可）：{}",
        seen[1]
    );
    let log_b = B_LOG.lock().unwrap().clone();
    assert!(
        log_b[0].starts_with("call:0:"),
        "消费者 B 的调用也必须成功：{log_b:?}"
    );
}
