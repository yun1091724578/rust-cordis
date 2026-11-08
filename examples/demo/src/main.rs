//! 端到端演示：Rust 原生插件 + 从动态库"任意安装"的 ABI 插件，共用同一套内核。
//!
//! 运行：`cargo run -p demo`
//!
//! 演示的断言分组（失败会直接 panic，不用肉眼看输出）：
//! 1. 配置驱动装载：`cordis.yml` + `cordis.patch.yml` → 两棵 ABI 插件 fiber；
//! 2. 跨 ABI 依赖注入：`hello` 声明 `inject: ["greeting"]` 后才被激活；
//! 3. 跨 ABI **服务方法调用**：`hello` 调用 `greeting.shout()`，并验证走的是同步快速路径；
//! 4. 跨 ABI 事件回调：宿主派发 `hello/ping` → 插件里的闭包真的被调用；
//! 5. 依赖失效传播：卸载 `greeting` → `hello` **自动**卸载（无手动编排）；
//! 6. 热重载：改一次插件文件 → 只重挂 `greeting`，但 `hello` 因为依赖换了提供者
//!    也**自动**重载（这是内核 epoch 机制的直接证据，装载器完全不知道依赖关系）；
//! 7. ABI v2 能力（M1）：派生隔离域、`@native:` 子组件挂载/回收、派生句柄撤销
//!    （ABI 形状插件经 `apply_via` 构造；真卸载/真撤销断言在 demo 侧，D9）；
//! 8. 入站异步（M2）：监听器返回挂起，宿主 await `ctx_fulfill` 的结算（挂起表归零）；
//! 9. M2b/M2c 的 SDK 面：拦截 waterfall（`on_async` + `next` + `fulfill`）、
//!    `ctx.abort` 自我取消与调用方令牌撤销（取消桥）——插件经 SDK 的构建路径构造；
//! 10. 暂存副本的生命周期（M0b，G11）：demo 退出前所有 `.rN` 副本**真的**从磁盘
//!     消失，泄漏台账归零——真删除断言按 D9 只进 demo/spike 二进制；
//! 11. use-site 元数据（M3）：调用者的 `inject` 声明与调用点上叠的 intercept 层
//!     合并（右偏）后**跨 ABI** 到达提供者，由真 DLL 的 `audit` 方法回抛。

use cordis_abi::{
    sdk, Blob, Out, OP_APPLY, OP_CALLBACK, OP_DISPOSE, STATUS_ERR, STATUS_OK, STATUS_PENDING,
    STATUS_UNSUPPORTED,
};
use cordis_core::{
    App, CancelToken, Config, Ctx, Dyn, ExternValue, FiberState, Inject, Level, Mode, Result,
};
use cordis_host::{
    apply_via, apply_via_with_error_format, host_api, loader, pending_len, set_native_registry,
    staged_leaked, NativeRegistry,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `rust-side` 每次激活时"读到了什么"：**不在插件里直接打印**——激活发生在
/// 装载器的异步任务里，与「已挂载」日志存在真实竞态；消息入队，由 `run_demo`
/// 在既定的收敛点 flush（A2：输出必须逐字节可复现）。
static RUST_SIDE_MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 热重载段的日志缓冲：加载成功与依赖方再激活由不同异步线打印，真实顺序有抖动。
/// 这一段统一入队，收敛后按固定顺序 flush（A2：输出必须逐字节可复现）。
static HMR_LOG_CAPTURE: Mutex<Option<Vec<String>>> = Mutex::new(None);

fn emit_log_line(line: String) {
    if let Some(buffer) = HMR_LOG_CAPTURE.lock().unwrap().as_mut() {
        buffer.push(line);
    } else {
        println!("{line}");
    }
}

fn start_hmr_capture() {
    *HMR_LOG_CAPTURE.lock().unwrap() = Some(Vec::new());
}

fn flush_hmr_capture() {
    let mut lines = HMR_LOG_CAPTURE.lock().unwrap().take().unwrap_or_default();
    lines.sort_by_key(|line| hmr_log_rank(line));
    for line in lines {
        println!("{line}");
    }
}

fn hmr_log_rank(line: &str) -> u8 {
    if line.contains("greeting: 装载") {
        0
    } else if line.contains("hello: 激活 #2") {
        1
    } else if line.contains("热重载 greet") {
        2
    } else if line.contains("[probe]") {
        3
    } else {
        4
    }
}

/// M2b/M2c 组（=== 7.7 / 7.8）的记账：SDK 形状插件的回调是否已经开跑。
static T11: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// M3 组（=== 4.5）的记账：**真 DLL** 提供者回抛的合并元数据。
static M3_META: Mutex<Option<Value>> = Mutex::new(None);

#[tokio::main]
async fn main() -> Result<()> {
    // 示例/测试产物没有发布 sidecar；显式开发模式，生产宿主必须用 --source-policy。
    cordis_host::source::set_allow_unverified(true);
    let exe_dir = std::env::current_exe()?
        .parent()
        .map(PathBuf::from)
        .expect("可执行文件必须有父目录");
    // 二进制本身叫 `cordis-demo`；Unix 没有 `.exe` 后缀，配置目录不能同名。
    let config_dir = exe_dir.join("cordis-demo-config");
    std::fs::create_dir_all(&config_dir)?;

    // 演示用的两份配置：主配置 + 用户 patch 层。
    //
    // 主配置是**纯条目数组**；patch 层才是"操作数组"（所以它用 `insert`）。
    // 注意 patch 层里 `config` 是**整体替换**（cordis 的既有语义），
    // 所以即使只想改 `times`，也要把整份 config 写全——这是刻意的兼容，不是 bug。
    let main_yaml = concat!(
        "- id: greet\n",
        "  name: greeting\n",
        "  config:\n",
        "    text: \"hello, cordis-rs\"\n",
        "- id: hello\n",
        "  name: hello\n",
        "  config:\n",
        "    times: 3\n",
        "    note: \"主配置里的 note\"\n",
        "- id: optional-group\n",
        "  name: cordis:group\n",
        "  group: true\n",
        "  config: []\n",
    );
    let patch_yaml = concat!(
        "# 用户层：整体替换 hello 的 config\n",
        "- id: hello\n",
        "  name: hello\n",
        "  config:\n",
        "    times: 7\n",
        "    note: \"patch 里的 note\"\n",
        "# 往 group 行里插一行（insert + id 指向 group 行）\n",
        "- insert:\n",
        "    - id: extra\n",
        "      name: greeting\n",
        "      disabled: true\n",
        "  id: optional-group\n",
    );
    std::fs::write(config_dir.join("cordis.yml"), main_yaml)?;
    std::fs::write(config_dir.join("cordis.patch.yml"), patch_yaml)?;

    // M0b（G11）：先清掉**历史泄漏**——G11 修复之前的版本留下的 `.rN` 按定义
    // 就是垃圾（这次运行会用同名路径重新暂存并认领，反正也带不走它们）。
    for stale in staged_copies(&exe_dir) {
        let _ = std::fs::remove_file(&stale);
    }
    let staged_before = staged_copies(&exe_dir);

    // run_demo 返回**drop 之前**的暂存清单；main 在整体 drop 之后验证它们
    // 真的从磁盘消失——真删除断言（D9）只进 demo/spike。
    let created = run_demo(&exe_dir, &config_dir, &staged_before).await?;
    assert_staged_cleanup(&created).await;

    println!("\n全部断言通过。");
    Ok(())
}

/// demo 主体（M0b 起收进函数）：`app` 连同 Store 里的 `Arc<dyn Plugin>`、装载器
/// `Mounted` 记录，都在**返回时整体 drop**——"先 FreeLibrary、后删暂存副本"
/// （`LoadedPlugin` 字段声明序保证的顺序）就发生在这一刻。
///
/// 返回 drop 之前的暂存副本清单：此刻插件还握着它们（文件必然都在），
/// 清单要在 drop **之前**取好。
async fn run_demo(exe_dir: &Path, config_dir: &Path, before: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let app = App::new();
    app.store.set_log_sink(Arc::new(|level, name, message| {
        let tag = match level {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        };
        emit_log_line(format!("  [{tag}] {name}: {message}"));
    }));

    println!("=== 1. 先挂一个纯 Rust 插件，证明内核不关心插件来源 ===");
    let rust_plugin = cordis_core::plugin_fn(
        "rust-side",
        Inject::of(&["hello.reply"]),
        |ctx: Ctx, _cfg| {
            Box::pin(async move {
                let reply = ctx.require_as::<ExternValue>("hello.reply")?;
                let message = reply
                    .0
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                // 不在这里直接打印（见 `RUST_SIDE_MESSAGES` 的说明）。
                RUST_SIDE_MESSAGES.lock().unwrap().push(message.to_string());
                Ok(())
            })
        },
    );
    // 先挂它：此时 `hello.reply` 还不存在，它应当停在未激活状态。
    let rust_fiber = app.root.plugin(rust_plugin, Config::Null);
    rust_fiber.settle().await;
    assert_eq!(
        rust_fiber.state(),
        FiberState::Pending,
        "缺依赖时必须停在 PENDING 而不是报错"
    );
    println!("  [rust-side] 依赖没就绪 → PENDING（不报错，这是 cordis 的核心行为）");

    println!("=== 2. 配置驱动装载两个动态库插件（打开 HMR）===");
    app.plugin(
        loader::loader(),
        json!({
            "path": config_dir.join("cordis.yml").to_string_lossy(),
            "patches": [config_dir.join("cordis.patch.yml").to_string_lossy()],
            "pluginDir": exe_dir.to_string_lossy(),
            "hmr": { "intervalMs": 100 },
        }),
    )
    .await?;

    // 初始对账跑在装载器自己的任务上，rust-side 的再激活也在另一条异步线上。
    // 两个**收敛条件**都满足前不许打印，否则 INFO 日志 / rust-side 消息 / 报告行
    // 会互相竞态（A2 基线要求输出逐字节可复现）：
    // ① 初始对账跑完（`applied == 2` ⇒ 两条「已挂载」日志都已落盘）；
    // ② rust-side 被自动激活、消息入队。
    let mut reconcile_done = false;
    let mut rust_ready = false;
    for _ in 0..400 {
        if !reconcile_done {
            let applied = app
                .root
                .require_as::<ExternValue>("loader.report")?
                .0
                .get("reconcile")
                .and_then(|value| value.get("applied"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            reconcile_done = applied >= 2;
        }
        if !rust_ready {
            rust_ready = !RUST_SIDE_MESSAGES.lock().unwrap().is_empty();
        }
        if reconcile_done && rust_ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        reconcile_done && rust_ready,
        "初始对账与 rust-side 激活必须在时限内收敛"
    );
    rust_fiber.settle().await;
    if let Some(message) = RUST_SIDE_MESSAGES.lock().unwrap().first().cloned() {
        println!("  [rust-side] 我从 ABI 插件那里读到了：{message}");
    }

    let report = app.root.require_as::<ExternValue>("loader.report")?;
    println!("  装载报告: {}", report.0);
    assert_eq!(
        report
            .0
            .get("failed")
            .and_then(Value::as_array)
            .map(|a| a.len()),
        Some(0),
        "不应有条目装载失败"
    );

    println!("=== 3. 依赖链条被自动拉起 ===");
    rust_fiber.settle().await;
    assert_eq!(
        rust_fiber.state(),
        FiberState::Active,
        "hello.reply 出现后必须自动激活"
    );
    for (name, count, key) in app.store.inventory() {
        println!(
            "  runtime {name} × {count}  ({})",
            portable_runtime_key(&key)
        );
    }

    println!("=== 4. 跨 ABI 的服务方法调用 ===");
    let reply = app.root.require_as::<ExternValue>("hello.reply")?.0.clone();
    let inline = reply
        .get("inline_call")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version_error = reply
        .get("version_error")
        .and_then(Value::as_str)
        .unwrap_or("");
    println!("  hello 通过 ABI 调用 greeting.shout() 拿到：{inline:?}");
    // 能立刻拿到值，说明宿主走的是**同步快速路径**（提供方的接口方法是同步函数）。
    assert!(
        inline.contains("HELLO, CORDIS-RS"),
        "跨 ABI 方法调用没有返回预期结果：{inline:?}"
    );
    // 版本协商：拿 v3 调 v2 的接口必须被明确拒绝，而不是调用一个不存在的方法。
    println!("  v3 请求打到 v2 接口的结果：{version_error}");
    assert!(version_error.contains("请求接口版本 3"), "{version_error}");

    // 有返回值的事件派发（`bail`）也跨越了 ABI。这里的结果里含有本插件自己
    // 贡献的那个值 —— 说明"发起 + 续延"这条路径完整跑通了。
    let dispatch_result = reply
        .get("dispatch_result")
        .and_then(Value::as_str)
        .unwrap_or("");
    println!("  跨 ABI 的 bail 派发结果：{dispatch_result}");
    assert!(
        dispatch_result.contains("hello, cordis-rs → self"),
        "有返回值的事件派发没有带回监听器的结果：{dispatch_result:?}"
    );

    println!("=== 5. 跨 ABI 事件回调 ===");
    let seen = {
        let probe = cordis_core::plugin_fn("probe", Inject::default(), |ctx: Ctx, _cfg| {
            Box::pin(async move {
                ctx.on("hello/ping", |call| {
                    let who = call
                        .args
                        .first()
                        .and_then(|arg| arg.downcast::<ExternValue>())
                        .and_then(|v| v.0.get("who").and_then(Value::as_str).map(str::to_string));
                    emit_log_line(format!("  [probe] 同时观察到 hello/ping（who={who:?}）"));
                    Box::pin(async { None })
                });
                Ok(())
            })
        });
        let probe_fiber = app.root.plugin(probe, Config::Null);
        probe_fiber.settle().await;
        app.root.hook_count("hello/ping")
    };
    assert_eq!(seen, 2, "一个 ABI 监听器 + 一个 Rust 监听器");
    app.root
        .emit(
            "hello/ping",
            vec![Dyn::new(ExternValue(json!({ "who": "demo" })))],
        )
        .await;

    println!("=== 6. 热重载：只改 greeting 的文件，hello 也跟着重载 ===");
    let reloads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let recorded = reloads.clone();
        let probe =
            cordis_core::plugin_fn("hmr-probe", Inject::default(), move |ctx: Ctx, _cfg| {
                let recorded = recorded.clone();
                Box::pin(async move {
                    ctx.on("loader/reload", move |call| {
                        let label = call
                            .args
                            .first()
                            .and_then(|arg| arg.downcast::<ExternValue>())
                            .and_then(|v| {
                                v.0.get("label").and_then(Value::as_str).map(str::to_string)
                            })
                            .unwrap_or_default();
                        recorded.lock().unwrap().push(label);
                        Box::pin(async { None })
                    });
                    Ok(())
                })
            });
        let probe_fiber = app.root.plugin(probe, Config::Null);
        probe_fiber.settle().await;
    }

    let hello_before = app
        .store
        .fibers_by_plugin_name("hello")
        .first()
        .and_then(|fiber| fiber.running_epoch());
    assert!(hello_before.is_some(), "hello 应当已经在运行");

    // "重新编译"的最朴素等价物：把同一份字节写回去，只改 mtime。
    // 真实场景里这一步是 `cargo build`，装载器的判定逻辑完全相同。
    start_hmr_capture();
    let greeting_source = exe_dir.join(plugin_file("greeting"));
    let bytes = std::fs::read(&greeting_source)?;
    std::fs::write(&greeting_source, &bytes)?;
    println!("  触碰了 greeting");

    // 等重载发生（间隔 100ms，给足三秒）——同样两个收敛条件：reload 事件到了、
    // rust-side 的**再激活消息**也入队了。先 flush 消息、再打印 reload 行，
    // 顺序由这里钉死（不比墙钟）。
    let mut reload_seen = false;
    let mut reactivated = false;
    for _ in 0..60 {
        reload_seen = !reloads.lock().unwrap().is_empty();
        reactivated = RUST_SIDE_MESSAGES.lock().unwrap().len() >= 2;
        if reload_seen && reactivated {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        reload_seen && reactivated,
        "重载与 rust-side 再激活必须在时限内完成"
    );
    flush_hmr_capture();
    if let Some(message) = RUST_SIDE_MESSAGES.lock().unwrap().get(1).cloned() {
        println!("  [rust-side] 我从 ABI 插件那里读到了：{message}");
    }
    let recorded = reloads.lock().unwrap().clone();
    println!("  收到 loader/reload: {recorded:?}");
    assert_eq!(recorded.len(), 1, "只有被改动的那一行应该被重挂");
    assert_eq!(recorded[0], "greet");

    // ★ 关键断言：hello 的 epoch 变了 —— 说明它被内核自动重载了。
    //   装载器完全不知道 greeting 与 hello 之间的依赖关系，
    //   这个级联完全来自"epoch 里混入了提供者 uid"这一条机制。
    let hello_after = app
        .store
        .fibers_by_plugin_name("hello")
        .first()
        .and_then(|fiber| fiber.running_epoch());
    assert_ne!(
        hello_before, hello_after,
        "提供者换了新实例之后，依赖者必须被自动重载（epoch 里含提供者 uid）"
    );
    println!("  hello 的 running_epoch: {hello_before:?} → {hello_after:?}（被自动重载）");

    // 重载后能力必须还在。
    let reply = app.root.require_as::<ExternValue>("hello.reply")?.0.clone();
    assert!(
        reply.get("message").and_then(Value::as_str).is_some(),
        "重载后 hello.reply 必须仍然可用"
    );
    println!("  重载后 hello.reply = {:?}", reply.get("message"));

    println!("=== 6.5 volatile：只改 volatile 字段不重挂（M4-5）===");
    // `hello` 声明了 `volatile: ["note"]`：只改 `note` 的配置变更会被**原地提交**，
    // 宿主把新值推给插件（`OP_VOLATILE_APPLY`），实例继续活着。
    let epoch_before_volatile = app
        .store
        .fibers_by_plugin_name("hello")
        .first()
        .and_then(|fiber| fiber.running_epoch());
    assert_eq!(
        note_of(&app).as_deref(),
        Some("patch 里的 note"),
        "patch 层覆盖了 note"
    );

    let patch_yaml = concat!(
        "# 用户层：整体替换 hello 的 config\n",
        "- id: hello\n",
        "  name: hello\n",
        "  config:\n",
        "    times: 7\n",
        "    note: \"volatile 里的 note\"\n",
        "# 往 group 行里插一行（insert + id 指向 group 行）\n",
        "- insert:\n",
        "    - id: extra\n",
        "      name: greeting\n",
        "      disabled: true\n",
        "  id: optional-group\n",
    );
    std::fs::write(config_dir.join("cordis.patch.yml"), patch_yaml)?;

    // 等插件读到新值（宿主推 → 插件重新读副本 → 更新服务值）。
    let mut observed = None;
    for _ in 0..60 {
        observed = note_of(&app);
        if observed.as_deref() == Some("volatile 里的 note") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        observed.as_deref(),
        Some("volatile 里的 note"),
        "volatile 新值必须真的到插件里（宿主推 + 插件重读）"
    );
    // ★ 关键断言：实例**没有**重挂——epoch 不变，apply 也不会再跑（ACTIVATIONS 不变）。
    let epoch_after_volatile = app
        .store
        .fibers_by_plugin_name("hello")
        .first()
        .and_then(|fiber| fiber.running_epoch());
    assert_eq!(
        epoch_before_volatile, epoch_after_volatile,
        "只差 volatile 字段的配置变更**不许**重挂实例"
    );
    println!(
        "  hello.note = {:?}，running_epoch 未变（{epoch_after_volatile:?}）",
        observed
    );

    // M4-6：报告是**实时**诊断面——原地提交之后，声明行仍在、实例仍支持 volatile。
    let report = app.root.require_as::<ExternValue>("loader.report")?;
    assert_eq!(
        report.0["volatile"],
        json!({ "declared": 1, "supported": 1 }),
        "原地提交后「支持 volatile 的行数」不变：{}",
        report.0
    );
    assert_eq!(
        report.0["disabled"],
        json!(["extra"]),
        "patch 里插入的那一行仍然是休眠（报告随对账刷新）：{}",
        report.0
    );

    println!("=== 6.6 panic 边界：插件的 panic 被截在边界之内（M5-1）===");
    // ① ABI 路径：插件在监听器里 panic（`hello/boom`）。SDK 的 `catch_unwind` 在
    //    `extern "C"` 边界**之内**截住它并回一条 STATUS_ERR，宿主只记一条错误日志：
    //    进程活着、别的监听者照常跑、hello 那一行不重挂。
    let probes = Arc::new(Mutex::new(0usize));
    let probe = probes.clone();
    app.root.on("hello/boom", move |_call| {
        *probe.lock().unwrap() += 1;
        Box::pin(async { None })
    });
    let hello_fiber = app
        .store
        .fibers_by_plugin_name("hello")
        .into_iter()
        .next()
        .expect("hello 应当正在运行");
    let epoch_before = hello_fiber.running_epoch();
    app.root.emit("hello/boom", vec![]).await;
    assert_eq!(*probes.lock().unwrap(), 1, "邻居监听者必须照常收到事件");
    assert_eq!(
        hello_fiber.state(),
        FiberState::Active,
        "坏行为不该把整行带走"
    );
    assert_eq!(hello_fiber.running_epoch(), epoch_before, "也不该触发重挂");
    println!("  插件在监听器里 panic → 宿主只记一条错误，hello 仍在跑（epoch 未变）");

    // ② 原生路径：`apply` 里 panic ⇒ 该 fiber 收敛到 FAILED（而不是无声挂在 LOADING），
    //    已经在跑的邻居完全不受影响。
    let boom = cordis_core::plugin_fn("boom", Inject::default(), |_ctx, _cfg| {
        let future: cordis_core::BoxFuture<Result<()>> =
            Box::pin(async { panic!("演示：apply 里 panic") });
        future
    });
    let boom_fiber = app.root.plugin(boom, Config::Null);
    boom_fiber.settle().await;
    assert_eq!(
        boom_fiber.state(),
        FiberState::Failed,
        "apply panic 必须收敛到 FAILED"
    );
    assert!(
        boom_fiber.error().unwrap_or_default().contains("panic"),
        "失败的 fiber 要留下可读错误"
    );
    assert_eq!(hello_fiber.state(), FiberState::Active, "邻居不受影响");
    println!("  apply panic → 该 fiber FAILED，邻居仍 Active");

    println!("=== 6.7 M3：use-site 元数据到达提供者（声明 ⊕ intercept 链，右偏）===");
    {
        // 调用者是**原生**插件，提供者是**真 DLL**（greeting.dll）——元数据因此必须
        // 真的跨过 ABI 边界（载荷的可选字段 `meta`），而不是在进程内的某个表里兜一圈。
        // greeting 的 `audit` 方法把 provider 侧读到的那一个实参原样回抛，断言就落在它上面：
        // 同键冲突时 **use-site 的 `ι` 覆盖组件声明的 `d`**（论文 p.897-902 的右偏合并）。
        //
        // 位置说明：放在热重载（6）**之后**，这样"多挂一个 fiber"不会扰动前面那些
        // 确定性哈希（epoch 的混入含 fiber uid 序列）。这里也顺带证明：元数据活在
        // **调用点的链**上，跨过热重载照样送达。
        //
        // 这个调用者在组末被**真卸载**（见下面的 `dispose`）：它曾经因为一条产物回收缺陷
        // （附录 E 缺陷 5）而不敢卸——那条链在 M4 的 G18 之后已经断了，
        // 针对性回归在 `crates/host/tests/loader_release.rs`。
        let caller = cordis_core::plugin_fn(
            "m3-caller",
            Inject(vec![(
                "greeting".into(),
                json!({ "mode": "loud", "origin": "declaration" }),
            )]),
            |ctx: Ctx, _cfg| {
                Box::pin(async move {
                    let site =
                        ctx.intercept("greeting", json!({ "origin": "use-site", "urgent": true }));
                    let answer = site.call_iface("greeting", 2, "audit", json!(null)).await?;
                    *M3_META.lock().unwrap() = answer.get("meta").cloned();
                    Ok(())
                })
            },
        );
        let caller_fiber = app.root.plugin(caller, Config::Null);
        caller_fiber.settle().await;
        assert_eq!(
            caller_fiber.state(),
            FiberState::Active,
            "M3 调用者应装载成功"
        );
        let meta = M3_META
            .lock()
            .unwrap()
            .clone()
            .expect("提供者必须回抛它读到的 meta");
        assert_eq!(
            meta,
            json!({ "mode": "loud", "origin": "use-site", "urgent": true }),
            "提供者读到的必须是 merge(声明, use-site)，且 use-site 右偏"
        );
        println!("  greeting 的提供者读到 {meta}");
        println!(
            "  同一键 origin 上：声明里的 \"declaration\" 被 use-site 的 \"use-site\" 覆盖（右偏）"
        );

        // 收尾：把这个**依赖装载器行服务的原生 fiber** 真卸载掉。
        //
        // 这里曾经不敢卸（附录 E 缺陷 5：卸载它会拖住 greeting 的暂存副本不回收），
        // 现在可以了：M4 的 G18 把 `ctx → fiber` 改成 `Weak`，那条"被留下的 ctx 吊住
        // 提供者 fiber"的链断了。针对性回归在 `crates/host/tests/loader_release.rs`
        // 的三个变体里（判据是插件对象是否被 drop = 暂存副本能否删除）。
        caller_fiber.dispose().await;
        assert_eq!(
            caller_fiber.state(),
            FiberState::Disposed,
            "提供者还在，但这个调用者的 fiber 已经被拆掉（处置是终态）"
        );
    }

    println!("=== 7. 卸载 greeting → hello 自动卸载 ===");
    let greeting_fibers = app.store.fibers_by_plugin_name("greeting");
    assert!(!greeting_fibers.is_empty(), "greeting 应该已被装载");
    for fiber in &greeting_fibers {
        fiber.dispose().await;
    }
    let hello_fibers = app.store.fibers_by_plugin_name("hello");
    for fiber in &hello_fibers {
        fiber.settle().await;
    }
    assert!(
        hello_fibers
            .iter()
            .all(|fiber| fiber.state() == FiberState::Pending),
        "提供者卸载后，依赖者必须自动卸载（不是被删除，而是回到未激活）"
    );
    rust_fiber.settle().await;
    assert_eq!(
        rust_fiber.state(),
        FiberState::Pending,
        "依赖链末端也要跟着回退"
    );
    println!("  hello / rust-side 全部自动退回 PENDING");

    println!("=== 7.5 M1：ABI v2 能力——派生 realm / @native 子组件 / 派生句柄回收 ===");
    // M1 的能力用一个 **ABI 形状**的插件走通：`CallFn` + `apply_via`（不经动态库，
    // 与 host 测试同一条夹具路径）。夹具铁律（D10）：fake `CallFn` 内禁止 assert，
    // 结局只记账；全部断言在 demo 侧做。
    {
        /// 进程级原生组件注册表：`@native:demo-tool` 的解析面。
        struct DemoRegistry;

        impl NativeRegistry for DemoRegistry {
            fn native(&self, name: &str) -> Option<Arc<dyn cordis_core::Plugin>> {
                (name == "demo-tool").then(|| {
                    cordis_core::plugin_fn("demo-tool", Inject::default(), |ctx: Ctx, _cfg| {
                        Box::pin(async move {
                            ctx.provide(
                                "demo-tool.echo",
                                Dyn::new(ExternValue(json!({ "ok": true }))),
                            )?;
                            Ok(())
                        })
                    })
                })
            }
        }
        set_native_registry(Arc::new(DemoRegistry));

        T9.lock().unwrap().clear();
        let abi_plugin = Arc::new(cordis_core::FnPlugin {
            name: "m1-abi",
            key: cordis_core::unique_key("m1-abi"),
            inject: Inject::default(),
            validator: None,
            f: Arc::new(|ctx: Ctx, config: Config| apply_via(t9_call, ctx, config, "m1-abi")),
        });
        let abi_fiber = app.root.plugin(abi_plugin, Config::Null);
        abi_fiber.settle().await;
        assert_eq!(
            abi_fiber.state(),
            FiberState::Active,
            "ABI 形状插件应装载成功"
        );

        // ① apply 里的两条宿主调用（isolate / use）都成功。
        let log = T9.lock().unwrap().clone();
        assert_eq!(
            log.len(),
            2,
            "apply 里应恰有 isolate 与 use 两条记录：{log:?}"
        );
        assert!(
            log[0].starts_with(&format!("isolate:{STATUS_OK}:")),
            "ctx_isolate 必须成功：{}",
            log[0]
        );
        assert!(
            log[1].starts_with(&format!("use:{STATUS_OK}:")),
            "ctx_use(@native:demo-tool) 必须成功：{}",
            log[1]
        );
        let iso_handle =
            handle_of(log[0].splitn(3, ':').nth(2).unwrap_or("")).expect("isolate 应答里有 handle");
        let child_handle =
            handle_of(log[1].splitn(3, ':').nth(2).unwrap_or("")).expect("use 应答里有 handle");

        // ② 子组件真的上线：隔离域内读得到 `demo-tool.echo`，根域读不到（域隔离）。
        let mut isolated = None;
        for _ in 0..60 {
            isolated = derived_read(iso_handle, "demo-tool.echo");
            if isolated.as_deref() == Some("value") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            isolated.as_deref(),
            Some("value"),
            "隔离域内 demo-tool.echo 必须可读"
        );
        assert!(
            app.root.get("demo-tool.echo").is_none(),
            "根域不得看见隔离域的服务（realm 隔离的直接证据）"
        );
        println!("  ctx_isolate 派生隔离域；@native:demo-tool 子组件已挂上并被派发到该域");
        println!("  派生句柄读到 demo-tool.echo，根域读不到（隔离的直接证据）");

        // ③ 未注册的原生组件：可读错误，且名字在消息里（可定位）。
        let (status, text) =
            unsafe { probe3(host_api().ctx_use, iso_handle, b"@native:missing", b"null") };
        assert_eq!(status, STATUS_ERR, "未注册的原生组件必须失败：{text}");
        assert!(text.contains("@native:missing"), "错误必须带上名字：{text}");
        println!("  未注册的原生组件 → 可读错误（含 \"@native:missing\"）");

        // ④ ctx_dispose 两个分支：子组件**真卸载**（隔离域内读回 missing）；
        //    派生 ctx 句柄被**撤销**（再读报「已失效」，不是静默复活）。
        let target = format!("{{\"handle\":{child_handle}}}");
        let (status, text) =
            unsafe { probe1(host_api().ctx_dispose, iso_handle, target.as_bytes()) };
        assert_eq!(status, STATUS_OK, "ctx_dispose(child) 必须成功：{text}");
        let mut gone = false;
        for _ in 0..60 {
            gone = derived_read(iso_handle, "demo-tool.echo").as_deref() == Some("missing");
            if gone {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "子组件被 ctx_dispose 之后，隔离域内必须读回 missing");

        let target = format!("{{\"handle\":{iso_handle}}}");
        let (status, _) = unsafe { probe1(host_api().ctx_dispose, iso_handle, target.as_bytes()) };
        assert_eq!(status, STATUS_OK, "ctx_dispose(派生 ctx) 必须成功");
        let (status, text) = unsafe { probe1(host_api().ctx_get, iso_handle, b"demo-tool.echo") };
        assert_eq!(status, STATUS_ERR, "被撤销的派生句柄不得再解析：{text}");
        assert!(
            text.contains("已失效"),
            "撤销后的拒绝必须是一条可读错误：{text}"
        );
        println!(
            "  ctx_dispose：子组件真卸载（域内读回 missing）、派生句柄被撤销（再读报「已失效」）"
        );

        // ⑤ 正常生命周期收尾：拆插件 → OP_DISPOSE 恰好投递一次。
        abi_fiber.dispose().await;
        let mut disposed = false;
        for _ in 0..60 {
            disposed = T9.lock().unwrap().iter().any(|entry| entry == "disposed");
            if disposed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(disposed, "ABI 插件应收到 OP_DISPOSE（正常生命周期）");
    }

    println!("=== 7.6 M2：入站异步——监听器挂起，宿主 await ctx_fulfill 结算 ===");
    // 插件侧：`async/ask` 监听器返回 `STATUS_PENDING` 并记下 dispatch_id；
    // 之后 `async/settle` 监听器用 `ctx_fulfill` 把值送回来——
    // 异步的存在感只跨越**这一次回调**（D3）。
    {
        T10.lock().unwrap().clear();
        *T10_HANDLE.lock().unwrap() = 0;

        let async_plugin = Arc::new(cordis_core::FnPlugin {
            name: "m2-async",
            key: cordis_core::unique_key("m2-async"),
            inject: Inject::default(),
            validator: None,
            f: Arc::new(|ctx: Ctx, config: Config| apply_via(async_call, ctx, config, "m2-async")),
        });
        let async_fiber = app.root.plugin(async_plugin, Config::Null);
        async_fiber.settle().await;
        assert_eq!(
            async_fiber.state(),
            FiberState::Active,
            "M2 假插件应装载成功"
        );
        let async_handle = *T10_HANDLE.lock().unwrap();

        // 顺序由 `pending_len` 钉死（不比墙钟）：先挂起，再结算。
        let ask = {
            let ctx = app.root.clone();
            tokio::spawn(async move { ctx.bail("async/ask", vec![]).await })
        };
        let mut suspended = false;
        for _ in 0..200 {
            suspended = pending_len(async_handle) == 1;
            if suspended {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(suspended, "派发必须先进入挂起（宿主在等 ctx_fulfill）");

        app.root.emit("async/settle", vec![]).await;

        let answer = ask.await.expect("派发任务不得 panic");
        let answer = answer
            .and_then(|value| value.downcast::<ExternValue>())
            .map(|value| value.0.clone());
        assert_eq!(
            answer,
            Some(json!({ "answer": 42 })),
            "ctx_fulfill 的值必须带回派发方"
        );
        assert_eq!(pending_len(async_handle), 0, "结算后挂起表必须归零");
        println!("  派发挂起 → ctx_fulfill 送回 {{\"answer\":42}} → 挂起表归零");

        async_fiber.dispose().await;
    }

    println!("=== 7.7 M2b：waterfall 拦截（SDK 面：on_async + ctx.next + fulfill）===");
    {
        T11.lock().unwrap().clear();
        // 用 SDK 的**构建路径**造一个"ABI 形状插件"（不经动态库）：
        // apply 里注册三个监听器——waterfall 拦截、自我取消、挂起（7.8 复用同一实例）。
        unsafe { sdk::attach(host_api()) };
        let desc =
            sdk::build_plugin_with_volatile("m2-sdk", &[], &[], |ctx: &sdk::Ctx, _config| {
                let ctx = *ctx;
                ctx.on_async("wf/tool", move |_args, dispatch_id| {
                    // 调 next（可换实参）→ 下游完成后**改写**结果 → fulfill 终值。
                    ctx.next(dispatch_id, vec![], move |downstream| {
                        let mut value = downstream.unwrap_or(Value::Null);
                        if let Value::Object(map) = &mut value {
                            map.insert("intercepted".into(), json!(true));
                        }
                        let _ = ctx.fulfill(dispatch_id, value);
                    })?;
                    Ok(sdk::AsyncReply::Pending)
                })?;
                ctx.on_async("abort/self", move |_args, dispatch_id| {
                    // 插件在自己的回调里撤销这次派发（SDK 的 ctx.abort）。
                    ctx.abort(dispatch_id)?;
                    Ok(sdk::AsyncReply::Pending)
                })?;
                ctx.on_async("abort/token", |_args, _dispatch_id| {
                    T11.lock().unwrap().push("abort-token".into());
                    Ok(sdk::AsyncReply::Pending)
                })?;
                Ok(())
            });
        let call: cordis_host::CallFn = unsafe { (*desc).call };
        let error_format = unsafe { cordis_abi::PluginDesc::error_format_at(desc) };
        let sdk_plugin = Arc::new(cordis_core::FnPlugin {
            name: "m2-sdk",
            key: cordis_core::unique_key("m2-sdk"),
            inject: Inject::default(),
            validator: None,
            f: Arc::new(move |ctx: Ctx, config: Config| {
                apply_via_with_error_format(call, ctx, config, "m2-sdk", error_format)
            }),
        });
        let sdk_fiber = app.root.plugin(sdk_plugin, Config::Null);
        sdk_fiber.settle().await;
        assert_eq!(
            sdk_fiber.state(),
            FiberState::Active,
            "SDK 形状插件应装载成功"
        );

        // 原生 tail 是"工具的原始返回值"；SDK 拦截器把它改写成 intercepted。
        let tail =
            Box::pin(async { Some(Dyn::new(ExternValue(json!({ "tool": "echo", "ok": true })))) });
        let ret = app
            .root
            .waterfall("wf/tool", vec![], Some(tail), None)
            .await;
        let value = ret
            .and_then(|v| v.downcast::<ExternValue>())
            .map(|v| v.0.clone());
        assert_eq!(
            value,
            Some(json!({ "tool": "echo", "ok": true, "intercepted": true })),
            "SDK 拦截器必须拿到下游结果并改写它"
        );
        println!("  插件 ctx.next 拿到下游结果、改写后 ctx.fulfill 结算（同步快速路径，一次往返）");

        println!("=== 7.8 M2c：取消（插件自我取消 + 调用方令牌撤销）===");
        // ① 插件自我取消：在自己的回调里 ctx.abort(dispatch_id)。
        let self_ret = app.root.bail("abort/self", vec![]).await;
        assert!(self_ret.is_none(), "自我取消 ⇒ 这次派发以「已取消」结算");

        // ② 调用方撤销（**取消桥**）：持令牌派发，插件挂起后 token.cancel()。
        let token = CancelToken::new();
        let future = app.store.dispatch_cancellable(
            Mode::Bail,
            "abort/token",
            &app.root,
            vec![],
            None,
            None,
            Some(token.clone()),
        );
        let task = tokio::spawn(future);
        let mut started = false;
        for _ in 0..200 {
            started = T11.lock().unwrap().iter().any(|line| line == "abort-token");
            if started {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(started, "派发必须已进入插件的挂起回调（先挂起再撤销）");
        token.cancel();
        let ret = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("令牌撤销后必须在时限内结算")
            .expect("派发任务不得 panic");
        assert!(
            ret.is_none() && token.is_cancelled(),
            "调用方撤销 ⇒ 挂起续延以错误结算"
        );
        println!("  ctx.abort（插件自我取消）与令牌撤销（取消桥）都让挂起续延以「已取消」结算");

        sdk_fiber.dispose().await;
    }

    // `created` 是**测量时刻仍存活**的副本集合：HMR 换下来的旧副本会在旧实例
    // drop 后立即删除（提前回收后只剩当前 greeting + hello 两份）。
    // 这里不是"曾经创建过几个"的累计值。
    let created: Vec<PathBuf> = staged_copies(exe_dir)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect();
    assert!(
        !created.is_empty(),
        "装载 ×2 + 热重载 ×1 应当至少新建了暂存副本"
    );

    // M0b：显式卸载装载器 fiber。`Store` 的 commit 观察者与 `Ctx` 互相持强引用，
    // 只有 fiber dispose 清空 `Fiber.ctx` 才能打破这个环——环不死，watcher 任务
    // 就永远握着 `Arc<LoaderState>`（连同 `Mounted` 里的 `Arc<LoadedPlugin>`）。
    // dispose 触发 ctx 副作用的处置器：watcher 的 stop_flag 置位 → 任务退出 →
    // `Mounted` 记录放手 → 先 FreeLibrary、后删暂存副本。
    for fiber in app.store.fibers_by_plugin_name(loader::LOADER_ID) {
        fiber.dispose().await;
    }

    Ok(created)
}

/// M0b（G11 / D9）：**真删除断言**——只进 demo/spike 二进制，测试套件走纯记账。
///
/// 轮询是给装载器 watcher 留的：它每 100ms 醒一次、握着 `Arc<LoaderState>`，
/// app drop 后最多一个周期它才放手，随后库卸载、文件删除（给足 5 秒）。
async fn assert_staged_cleanup(created: &[PathBuf]) {
    let mut remaining: Vec<PathBuf> = created.to_vec();
    for _ in 0..100 {
        remaining.retain(|path| path.exists());
        if remaining.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        remaining.is_empty(),
        "插件全部卸载后，暂存副本必须真的从磁盘消失（G11）：仍存在 {remaining:?}；存活实例 = {}，未认领台账 = {}",
        cordis_host::loaded_instances(),
        staged_leaked()
    );
    assert_eq!(staged_leaked(), 0, "泄漏台账必须归零（每个副本都被认领）");
    println!("=== 8. 暂存副本的删除真的发生了（M0b，G11）===");
    println!(
        "  {} 个 .rN 副本随插件卸载从磁盘消失，stagedLeaked = 0。",
        created.len()
    );
}

// ---------------------------------------------------------- M1 组的夹具与助手

/// M1 组（=== 7.5）的记账：apply 里的 isolate/use 结局、OP_DISPOSE 通知。
/// 夹具铁律（D10）：fake `CallFn` 内禁止 assert（panic 跨 `extern "C"` 即 abort）。
static T9: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// M1 组的假 ABI 插件入口：`OP_APPLY` 里派生隔离域并挂 `@native:` 子组件。
unsafe extern "C" fn t9_call(handle: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let host = host_api();
            let (status, text) = probe3(host.ctx_isolate, handle, b"demo-tool.echo", b"");
            T9.lock().unwrap().push(format!("isolate:{status}:{text}"));
            if status == STATUS_OK {
                if let Some(derived) = handle_of(&text) {
                    let (st, body) = probe3(host.ctx_use, derived, b"@native:demo-tool", b"null");
                    T9.lock().unwrap().push(format!("use:{st}:{body}"));
                }
            }
            (*out).write(b"")
        }
        OP_DISPOSE => {
            T9.lock().unwrap().push("disposed".into());
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

/// M2 组（=== 7.6）的记账：监听器注册结局、结算调用结局。
static T10: Mutex<Vec<String>> = Mutex::new(Vec::new());
static T10_HANDLE: Mutex<u64> = Mutex::new(0);
static T10_PENDING_ID: Mutex<u64> = Mutex::new(0);

/// M2 组的假 ABI 插件入口：`async/ask` 挂起（记下 dispatch_id），
/// `async/settle` 用 `ctx_fulfill` 把它结算成 `{"answer":42}`。
unsafe extern "C" fn async_call(handle: u64, op: u32, a: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            *T10_HANDLE.lock().unwrap() = handle;
            // 宿主不支持就原样降级（**绝不在 `extern "C"` 里 panic**，D10 铁律）。
            let Some(on) = host_api().ctx_on else {
                return STATUS_UNSUPPORTED;
            };
            let mut ask_buffer = [0u8; 256];
            let mut ask_out = Out {
                ptr: ask_buffer.as_mut_ptr(),
                cap: ask_buffer.len(),
                len: 0,
            };
            let ask = on(handle, Blob::new(b"async/ask"), 7, &mut ask_out);
            let mut settle_buffer = [0u8; 256];
            let mut settle_out = Out {
                ptr: settle_buffer.as_mut_ptr(),
                cap: settle_buffer.len(),
                len: 0,
            };
            let settle = on(handle, Blob::new(b"async/settle"), 8, &mut settle_out);
            T10.lock().unwrap().push(format!("on:{ask}:{settle}"));
            (*out).write(b"")
        }
        OP_CALLBACK => {
            let payload: Value = serde_json::from_slice(a.as_bytes()).unwrap_or(Value::Null);
            let id = payload.get("id").and_then(Value::as_u64).unwrap_or(0);
            if id == 7 {
                *T10_PENDING_ID.lock().unwrap() = payload
                    .get("dispatch_id")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                STATUS_PENDING
            } else {
                let dispatch_id = *T10_PENDING_ID.lock().unwrap();
                let value = json!({ "answer": 42 }).to_string();
                let status = match host_api().ctx_fulfill {
                    None => -1,
                    Some(fulfill) => {
                        let mut buffer = [0u8; 256];
                        let mut fulfill_out = Out {
                            ptr: buffer.as_mut_ptr(),
                            cap: buffer.len(),
                            len: 0,
                        };
                        fulfill(
                            handle,
                            dispatch_id,
                            Blob::new(value.as_bytes()),
                            &mut fulfill_out,
                        )
                    }
                };
                T10.lock().unwrap().push(format!("fulfill:{status}"));
                (*out).write(b"")
            }
        }
        OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

/// 从 `{"handle": <u64>}` 应答里取句柄。
fn handle_of(text: &str) -> Option<u64> {
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("handle")?
        .as_u64()
}

/// 三参宿主调用（`ctx, blob, blob, out`：isolate / use）的结局：`status` + 输出文本。
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
            (
                status,
                String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
            )
        }
        None => (-1, "宿主不支持这个函数（None）".into()),
    }
}

/// 单 blob 宿主调用（`ctx, blob, out`：`ctx_get` / `ctx_dispose`）的结局。
unsafe fn probe1(
    f: Option<unsafe extern "C" fn(u64, Blob, *mut Out) -> i32>,
    handle: u64,
    a: &[u8],
) -> (i32, String) {
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    match f {
        Some(f) => {
            let status = f(handle, Blob::new(a), &mut out);
            (
                status,
                String::from_utf8_lossy(&buffer[..out.len]).into_owned(),
            )
        }
        None => (-1, "宿主不支持这个函数（None）".into()),
    }
}

/// 经派生句柄读一个名字，把应答归类成 `value` / `missing` / `error:<status>`。
fn derived_read(handle: u64, name: &str) -> Option<String> {
    let (status, text) = unsafe { probe1(host_api().ctx_get, handle, name.as_bytes()) };
    if status != STATUS_OK {
        return Some(format!("error:{status}"));
    }
    let value: Value = serde_json::from_str(&text).ok()?;
    if value.get("missing").and_then(Value::as_bool) == Some(true) {
        Some("missing".into())
    } else if value.get("value").is_some() {
        Some("value".into())
    } else {
        Some(format!("other:{text}"))
    }
}

/// `dir` 里的暂存副本（`<stem>.r<digits><DLL_SUFFIX>`）清单，排序后返回。
fn staged_copies(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_staged_copy)
        })
        .collect();
    found.sort();
    found
}

fn is_staged_copy(name: &str) -> bool {
    let Some((stem, digits)) = name
        .strip_suffix(std::env::consts::DLL_SUFFIX)
        .and_then(|stem| stem.rsplit_once(".r"))
    else {
        return false;
    };
    !stem.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn plugin_file(stem: &str) -> String {
    format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// 运行时键里的库路径是本机绝对路径；快照只保留平台无关的 `<名>.r<修订号>`。
fn portable_runtime_key(key: &str) -> String {
    let Some((prefix, path)) = key.rsplit_once("::") else {
        return key.to_string();
    };
    let file = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let file = file
        .strip_suffix(std::env::consts::DLL_SUFFIX)
        .unwrap_or(&file);
    let file = file
        .strip_prefix(std::env::consts::DLL_PREFIX)
        .unwrap_or(file);
    // 暂存文件名带有进程内唯一前缀（`<stem>.s<pid>-<seq>.r<rev>`），
    // 快照只保留语义稳定的 `<stem>.r<rev>`。
    let file = strip_stage_nonce(file);
    format!("{prefix}::{file}")
}

fn strip_stage_nonce(file: &str) -> String {
    let Some((stem, revision)) = file.rsplit_once(".r") else {
        return file.to_string();
    };
    let Some((base, tag)) = stem.rsplit_once('.') else {
        return file.to_string();
    };
    let Some(rest) = tag.strip_prefix('s') else {
        return file.to_string();
    };
    let mut parts = rest.split('-');
    let (Some(pid), Some(seq), None) = (parts.next(), parts.next(), parts.next()) else {
        return file.to_string();
    };
    if pid.is_empty()
        || seq.is_empty()
        || !pid.bytes().all(|b| b.is_ascii_digit())
        || !seq.bytes().all(|b| b.is_ascii_digit())
    {
        return file.to_string();
    }
    format!("{base}.r{revision}")
}

/// 读 `hello` 插件当前报告的 volatile note（`hello.note` 服务由插件在推送后更新）。
fn note_of(app: &cordis_core::App) -> Option<String> {
    app.root
        .get("hello.note")
        .and_then(|value| value.downcast::<ExternValue>())
        .and_then(|value| {
            value
                .0
                .get("note")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}
