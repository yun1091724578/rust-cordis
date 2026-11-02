//! M1（ABI 能力五项）：派生 realm 子 context 与**派生句柄位布局**。
//!
//! 全部无 DLL（D9）：ABI 插件经 [`cordis_host::apply_via`] + 手写 `CallFn`
//! 构造（`host::CallFn` 文档里"不经动态库的 ABI 形状插件"夹具），
//! 宿主函数经 [`cordis_host::host_api`] 的**真表**调用。
//!
//! 位布局（§4 M1，D8 的 seam 常量镜像在此钉死）：
//! - 实例句柄：`bit63=0 | bits[62:32]=index | bits[31:0]=generation`；
//! - 派生句柄：`bit63=1 | bits[62:0]=单调 seq（从 1 起，永不复用）`；
//! - 解析顺序：先实例表（bit63=1 必然 miss），后派生表。
//!
//! 夹具铁律（D10）：fake `CallFn` 内**禁止 assert**（panic 跨 `extern "C"`
//! 即 abort），只把结局记进 static `Mutex`；跨线程状态不用 `thread_local`。

use cordis_abi::{Blob, Out, OP_APPLY, OP_DISPOSE, STATUS_ERR, STATUS_OK};
use cordis_core::{App, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Plugin};
use cordis_host::{apply_via, host_api};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// 派生句柄的 bit63 标记（位布局契约；与 `cordis_host::DERIVED_HANDLE_TAG` 同值）。
const TAG: u64 = 1 << 63;

/// 把一次宿主调用的结局记成 `status:text` 的可读快照。
unsafe fn probe3(
    f: Option<unsafe extern "C" fn(u64, Blob, Blob, *mut Out) -> i32>,
    handle: u64,
    a: &[u8],
    b: &[u8],
) -> (i32, String) {
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    match f {
        Some(f) => {
            let status = f(handle, Blob::new(a), Blob::new(b), &mut out);
            let text = String::from_utf8_lossy(&buffer[..out.len]).into_owned();
            (status, text)
        }
        None => (-1, "宿主不支持这个函数（None）".into()),
    }
}

/// 从 `{"handle": <u64>}` 应答里取句柄。
fn handle_of(text: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("handle")?
        .as_u64()
}

fn abi_plugin(name: &'static str, call: cordis_host::CallFn) -> Arc<dyn Plugin> {
    Arc::new(cordis_core::FnPlugin {
        name,
        key: cordis_core::unique_key(name),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| apply_via(call, ctx, config, name)),
    })
}

// ------------------------------------------------------------------ 测试 1

static T1: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 假插件：OP_APPLY 里 isolate `iso-svc`，再用**派生句柄**读它——两条结局都落日志。
unsafe extern "C" fn t1_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let host = host_api();
            let (status, text) = probe3(host.ctx_isolate, handle, b"iso-svc", b"");
            T1.lock().unwrap().push(format!("isolate:{status}:{text}"));
            if status == STATUS_OK {
                if let Some(derived) = handle_of(&text) {
                    let get = host.ctx_get;
                    let mut buffer = [0u8; 1024];
                    let mut get_out = Out {
                        ptr: buffer.as_mut_ptr(),
                        cap: buffer.len(),
                        len: 0,
                    };
                    match get {
                        Some(get) => {
                            let st = get(derived, Blob::new(b"iso-svc"), &mut get_out);
                            let body = String::from_utf8_lossy(&buffer[..get_out.len]).into_owned();
                            T1.lock().unwrap().push(format!("derived-get:{st}:{body}"));
                        }
                        None => T1.lock().unwrap().push("derived-get:none".into()),
                    }
                }
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// M1 红测试 1：`ctx_isolate` 派生 realm 子 context，返回**可用**的派生句柄。
///
/// "可用"的 M1 语义（G18：派生 ctx 无 fiber，provide/on 留给 M4 的子 context）：
/// 通过派生句柄的 `ctx_get` 是合法调用——读的是**隔离域**：
/// 根域里现成的 `iso-svc`，隔离域内必须读作 `missing`（域隔离的直接证据），
/// 而同一名字从根 ctx 读得到（对照面）。
#[tokio::test]
async fn abi_isolate_derives_a_realm_and_returns_a_usable_handle() {
    T1.lock().unwrap().clear();

    let app = App::new();
    // 根域提供者：独立原生插件，保持 ACTIVE。
    let provider = cordis_core::plugin_fn("t1-provider", Inject::default(), |ctx: Ctx, _cfg| {
        Box::pin(async move {
            ctx.provide("iso-svc", Dyn::new(ExternValue(json!({ "v": 1 }))))?;
            Ok(())
        })
    });
    let provider_fiber = app.root.plugin(provider, Config::Null);
    provider_fiber.settle().await;
    assert_eq!(provider_fiber.state(), FiberState::Active);

    let fiber = app.root.plugin(abi_plugin("t1-abi", t1_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active, "假插件应装载成功");

    let log = T1.lock().unwrap().clone();
    assert_eq!(
        log.len(),
        2,
        "应有 isolate 与 derived-get 两条记录：{log:?}"
    );
    let isolate = &log[0];
    assert!(
        isolate.starts_with(&format!("isolate:{STATUS_OK}:")),
        "ctx_isolate 必须成功（宿主函数已注册），实际：{isolate}"
    );
    let derived =
        handle_of(isolate.splitn(3, ':').nth(2).unwrap_or("")).expect("isolate 应答里有 handle");
    assert_ne!(
        derived & TAG,
        0,
        "派生句柄 bit63 必须置位（位布局契约）：{derived:#x}"
    );

    // 可用：通过派生句柄的 get 是一次合法、格式完好的调用——且读的是隔离域。
    let read = &log[1];
    assert!(
        read.starts_with(&format!("derived-get:{STATUS_OK}:{{\"missing\":true}}")),
        "隔离域内 iso-svc 必须读作 missing（realm 隔离的直接证据），实际：{read}"
    );
    // 对照面：同一个名字在根域里读得到。
    let root_value = app
        .root
        .get("iso-svc")
        .and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone());
    assert_eq!(
        root_value,
        Some(json!({ "v": 1 })),
        "根域对照面：iso-svc 应可读"
    );
}

// ------------------------------------------------------------------ 测试 2

static T2_ISOLATE: Mutex<String> = Mutex::new(String::new());

unsafe extern "C" fn t2_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (_, text) = probe3(host_api().ctx_isolate, handle, b"iso-svc", b"");
            *T2_ISOLATE.lock().unwrap() = text;
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// M1 红测试 2：根实例注销 ⇒ 它派生的句柄全部**回收**（排水）。
///
/// 排水之后，派生句柄必须像陈旧实例句柄一样被拒绝（可读错误），
/// 而不是继续解析成一个已经不存在的上下文。
#[tokio::test]
async fn derived_handle_is_revoked_when_the_root_instance_unregisters() {
    T2_ISOLATE.lock().unwrap().clear();

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("t2-abi", t2_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let derived = handle_of(&T2_ISOLATE.lock().unwrap().clone()).expect("isolate 应答里有 handle");
    let get = host_api().ctx_get.expect("宿主必须提供 ctx_get");

    // 根实例还活着：派生句柄可用（读到 missing 也是一次合法调用）。
    let status = {
        let mut buffer = [0u8; 256];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        unsafe { get(derived, Blob::new(b"iso-svc"), &mut out) }
    };
    assert_eq!(status, STATUS_OK, "根实例存活期间派生句柄必须可用");

    // 卸载根实例：apply_via 的 effect 注销实例句柄 ⇒ 排水 owner==root 的派生句柄。
    fiber.dispose().await;

    let (status, text) = {
        let mut buffer = [0u8; 256];
        let mut out = Out {
            ptr: buffer.as_mut_ptr(),
            cap: buffer.len(),
            len: 0,
        };
        let status = unsafe { get(derived, Blob::new(b"iso-svc"), &mut out) };
        let text = String::from_utf8_lossy(&buffer[..out.len]).into_owned();
        (status, text)
    };
    assert_eq!(status, STATUS_ERR, "根实例注销后派生句柄必须被拒绝");
    assert!(text.contains("已失效"), "拒绝必须是一条可读错误：{text}");
}

// ------------------------------------------------------------------ 测试 3

static T3_OPS: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());
static T3_DERIVED: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn t3_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    T3_OPS.lock().unwrap().push((handle, op));
    match op {
        OP_APPLY => {
            let (status, text) = probe3(host_api().ctx_isolate, handle, b"iso-svc", b"");
            if status == STATUS_OK {
                if let Some(derived) = handle_of(&text) {
                    *T3_DERIVED.lock().unwrap() = derived;
                }
            }
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// M1 红测试 3：排水 ≠ 给派生句柄发 `OP_DISPOSE`。
///
/// 派生 ctx 没有自己的插件入口；把卸载通知发给派生句柄只会造出
/// "插件收到一个它从没注册过的句柄"这类幽灵调用。排水只撤销句柄；
/// `OP_DISPOSE` 只发给实例句柄，且恰好一次。
#[tokio::test]
async fn derived_handle_does_not_receive_op_dispose() {
    T3_OPS.lock().unwrap().clear();
    *T3_DERIVED.lock().unwrap() = 0;

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("t3-abi", t3_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let derived = *T3_DERIVED.lock().unwrap();
    assert_ne!(derived, 0, "插件应已 isolate 出派生句柄");
    let instance = T3_OPS
        .lock()
        .unwrap()
        .iter()
        .find(|(_, op)| *op == OP_APPLY)
        .map(|(handle, _)| *handle)
        .expect("插件应收到过 OP_APPLY（携带实例句柄）");

    fiber.dispose().await;

    let ops = T3_OPS.lock().unwrap().clone();
    let disposes: Vec<u64> = ops
        .iter()
        .filter(|(_, op)| *op == OP_DISPOSE)
        .map(|(h, _)| *h)
        .collect();
    assert_eq!(
        disposes,
        vec![instance],
        "OP_DISPOSE 必须恰好一次、且发给实例句柄 {instance:#x}：{ops:?}"
    );
    assert!(
        ops.iter().all(|(handle, _)| *handle != derived),
        "任何 op 都不得以派生句柄 {derived:#x} 为目标（排水 ≠ 发通知）：{ops:?}"
    );
}

// ------------------------------------------------------------------ 测试 4

static T4_ISOLATE: Mutex<String> = Mutex::new(String::new());

unsafe extern "C" fn t4_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let (_, text) = probe3(host_api().ctx_isolate, handle, b"iso-svc", b"");
            *T4_ISOLATE.lock().unwrap() = text;
            (*out).write(b"")
        }
        OP_DISPOSE => (*out).write(b""),
        _ => cordis_abi::STATUS_UNSUPPORTED,
    }
}

/// M1 红测试 4：派生句柄**永不**解析成实例（位布局钉死）。
///
/// bit63=1 的句柄进实例表必然 miss——这不是实现细节，是两个句柄命名空间
/// 不相交的保证。可观察面：只认实例的 op（`ctx_provide` / `ctx_effect`）
/// 拿到派生句柄必须失败，而**绝不能**静默落到根实例上执行
/// （那会把隔离域的服务悄悄写进根域——最危险的串线）。
#[tokio::test]
async fn a_derived_handle_never_resolves_as_an_instance() {
    T4_ISOLATE.lock().unwrap().clear();

    let app = App::new();
    let fiber = app.root.plugin(abi_plugin("t4-abi", t4_call), Config::Null);
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active);

    let derived = handle_of(&T4_ISOLATE.lock().unwrap().clone()).expect("isolate 应答里有 handle");

    // 位布局：bit63 置位、bits[62:0] 是从 1 起的单调 seq。
    assert_ne!(derived & TAG, 0, "bit63 必须置位：{derived:#x}");
    assert!(
        derived > TAG,
        "seq 从 1 起（bit63 之外必须非零）：{derived:#x}"
    );

    // 实例专属 op：provide。
    let (status, text) =
        unsafe { probe3(host_api().ctx_provide, derived, b"t4-svc", br#"{"x":1}"#) };
    assert_eq!(
        status, STATUS_ERR,
        "ctx_provide 不接受派生句柄：{status}:{text}"
    );
    assert!(
        app.root.get("t4-svc").is_none(),
        "失败必须是真失败——服务绝不能借道根实例落进根域"
    );

    // 实例专属 op：effect（清理注册）。
    let mut buffer = [0u8; 256];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let effect = host_api().ctx_effect.expect("宿主必须提供 ctx_effect");
    let status = unsafe { effect(derived, 9, &mut out) };
    assert_eq!(
        status, STATUS_ERR,
        "ctx_effect 不接受派生句柄（M1：派生域只读）"
    );
    fiber.dispose().await;
}
