//! M5-1：**ABI 边界的 panic 护栏**。
//!
//! # 为什么独占一个测试文件
//! 红阶段这里会发生 `extern "C"` 边界的 **abort**：Rust 的 "C" ABI 不可展开，
//! panic 一旦试图跨过它，进程立即终止——同一个测试二进制里的其它测试全部陪葬，
//! 红证据也就糊成一团。一个文件 = 只砸自己。
//!
//! # 场景
//! 插件在**卸载期**（fiber 处于 `UNLOADING`）再次注册 effect。这是真实会发生的形态：
//! cleanup 里想"再挂一个收尾动作"、或者延迟回调在卸载窗口里跑了进来。
//! 内核在这种情况下必须回一条**可读错误**，而不是让宿主进程消失。

use cordis_abi::{Blob, Out, OP_APPLY, OP_CLEANUP, STATUS_OK};
use cordis_core::{
    App, BoxFuture, Config, Ctx, Fiber, FiberState, Inject, Plugin, PluginKey, Result,
};
use cordis_host::{apply_via, host_api};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 一次 `ctx_effect` 调用的结局：`(状态码, 正文)`。
static EFFECT_CALLS: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

/// 在**宿主函数表**上真调一次 `ctx_effect`（与插件侧 SDK 走的是同一条路）。
///
/// # Safety
/// `handle` 必须是本进程里存活实例的句柄。
unsafe fn ctx_effect(handle: u64, cb: u64) -> (i32, String) {
    let api = host_api();
    let effect = api.ctx_effect.expect("宿主函数表必须带 ctx_effect");
    let mut buffer = [0u8; 256];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = effect(handle, cb, &mut out);
    let text = String::from_utf8_lossy(&buffer[..out.len]).into_owned();
    EFFECT_CALLS.lock().unwrap().push((status, text.clone()));
    (status, text)
}

/// "ABI 形状"的假插件：入口就是一个 `call` 函数指针，与 `LoadedPlugin` 走同一条路径。
struct AbiProbe {
    name: &'static str,
}

impl Plugin for AbiProbe {
    fn name(&self) -> &str {
        self.name
    }

    fn key(&self) -> PluginKey {
        Arc::from(self.name)
    }

    fn inject(&self) -> Inject {
        Inject::default()
    }

    fn apply(&self, ctx: Ctx, config: Config) -> BoxFuture<Result<()>> {
        apply_via(probe_call, ctx, config, self.name)
    }
}

/// 假 ABI 入口。**禁止 assert**（panic 跨 `extern "C"` 即 abort）——只记录，测试体断言。
unsafe extern "C" fn probe_call(handle: u64, op: u32, _a: Blob, _b: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            // apply 期注册一个 cleanup：卸载时宿主会回调 OP_CLEANUP 进来。
            ctx_effect(handle, 7);
            (*out).write(b"")
        }
        OP_CLEANUP => {
            // **卸载期**再注册一个 effect：此刻 fiber 是 UNLOADING。
            ctx_effect(handle, 8);
            (*out).write(b"")
        }
        _ => (*out).write(b""),
    }
}

async fn wait_for(fiber: &Arc<Fiber>, state: FiberState, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while fiber.state() != state {
        assert!(
            Instant::now() < deadline,
            "等 {what} 超时：fiber 停在 {:?}",
            fiber.state()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn register_effect_during_unloading_returns_error_not_panic() {
    let app = App::new();
    let fiber = app
        .root
        .plugin(Arc::new(AbiProbe { name: "abi-probe" }), Config::Null);
    wait_for(&fiber, FiberState::Active, "装载完成").await;

    // apply 期那次注册必须成功（先证明句柄与路径都是通的）。
    let apply_call = EFFECT_CALLS
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("apply 期应当注册过一次 effect");
    assert_eq!(
        apply_call.0, STATUS_OK,
        "apply 期注册 effect 应当成功：{}",
        apply_call.1
    );

    // 卸载：OP_CLEANUP 会在 fiber 处于 UNLOADING 时被回调进来，它再注册一次 effect。
    fiber.dispose().await;

    let calls = EFFECT_CALLS.lock().unwrap().clone();
    let (status, message) = calls
        .get(1)
        .cloned()
        .expect("卸载期应当再注册过一次 effect");
    assert_eq!(
        status,
        cordis_abi::STATUS_ERR,
        "卸载期注册 effect 必须是可读错误，而不是让宿主进程 abort：{message}"
    );
    assert!(
        message.contains("inactive context"),
        "错误消息要说清原因：{message}"
    );
    assert_eq!(fiber.effect_count(), 0, "被拒绝的 effect 不得留下任何残留");
    assert_eq!(cordis_host::loaded_instances(), 0, "卸载后不得残留实例句柄");
}
