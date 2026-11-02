//! harness-spike 的**真 DLL 层**（D9：这类断言只允许出现在二进制里）。
//!
//! 运行（**前置构建写死**——`cargo run` 不会构建 cdylib，见 D10）：
//!
//! ```text
//! cargo build -p plugin-harness-tool
//! cargo run -p harness-spike
//! ```
//!
//! 插件目录取自 **exe 目录**（不是 cwd）；`[lib] name = "harness_tool"` ⇒ `harness_tool.dll`。
//!
//! # 这一层断言什么
//! - **A4**：动态库插件挂 `@native:` 子组件（在它自己派生的**隔离域**里）、设 intercept
//!   （提供者侧读回 use-site 元数据）、做**异步**工具（挂起后由宿主放行）、自我卸载；
//! - **A9**：它拦截 `tools/before-invoke`（waterfall），`next` 回来后**改写结果**；
//! - **A10**：调用方取消一次在飞的挂起 ⇒ 派发以"没有意见"结算，且**不残留**（`pending_len == 0`）；
//! - **A11**：`sessions` 是 **T2（DLL）**、append-only、`seq` 连续；一次**真重载**之后
//!   `seq` 仍连续（只能来自"从日志恢复"）；
//! - 外加 M5 的**预挂载探测**（装载之前就能列出它会装什么，此时 `loaded_instances() == 0`）。
//!
//! 无 DLL 的那五条（作用域隔离 / typed 服务 / typed 事件 / replay 确定性 / 原地换适配器）
//! 在 `tests/e2e.rs`。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis_abi::{Blob, Out, STATUS_OK};
use cordis_core::{
    App, CancelToken, Config, Ctx, Dyn, Error, ExternValue, FiberState, Inject, Mode, Plugin,
    Result,
};
use cordis_host::{
    host_api, loaded_instances, loader, pending_len, probe_capabilities, set_native_registry,
    staged_leaked, streams_len, NativeRegistry,
};
use harness_spike::{
    boot_agents, boot_core_in, mount_tool, replay_llm, scripted_llm, Agent, AgentService, Core,
    LlmService, Plan, Report,
};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<()> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .map(PathBuf::from)
        .expect("可执行文件必须有父目录");
    let config_dir = exe_dir.join("harness-spike");
    std::fs::create_dir_all(&config_dir)?;
    let dll = exe_dir.join(format!(
        "{}harness_tool{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    if !dll.is_file() {
        return Err(format!(
            "找不到 {}：先跑 `cargo build -p plugin-harness-tool`（`cargo run` 不会构建 cdylib）",
            dll.display()
        )
        .into());
    }
    // 会话日志每次运行都从零开始（上一轮的记录会被重载验证当成"历史"）。
    let log_path = config_dir.join("sessions.jsonl");
    let _ = std::fs::remove_file(&log_path);
    let config_path = write_config(&config_dir, &log_path)?;

    // 清掉历史暂存副本（与 demo 同一纪律：它们按定义就是垃圾）。
    for stale in staged_copies(&exe_dir) {
        let _ = std::fs::remove_file(&stale);
    }

    let created = run_spike(&exe_dir, &dll, &config_path, &log_path).await?;

    println!("=== 10. 真删除：暂存副本回收、泄漏台账归零（D9 的老规矩）===");
    // 与 demo 同一手法：给"最后一个握手"留出轮询窗口（观察者/任务放手是异步的）。
    let mut remaining: Vec<PathBuf> = created.clone();
    for _ in 0..100 {
        remaining.retain(|path| path.exists());
        if remaining.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        remaining.is_empty(),
        "插件全部卸载后，暂存副本必须真的从磁盘消失（G11）：仍存在 {remaining:?}；\
         存活实例 = {}，未认领台账 = {}",
        loaded_instances(),
        staged_leaked()
    );
    assert_eq!(staged_leaked(), 0, "暂存泄漏台账必须归零");
    println!(
        "  {} 个 .rN 副本随插件卸载从磁盘消失（loaded_instances={}、stagedLeaked=0）",
        created.len(),
        loaded_instances()
    );

    println!("\n全部断言通过（真 DLL 层）。");
    Ok(())
}

/// spike 主体。返回**drop 之前**看到的暂存副本清单（main 在整体 drop 之后再验证回收）。
async fn run_spike(
    exe_dir: &Path,
    dll: &Path,
    config_path: &Path,
    log_path: &Path,
) -> Result<Vec<PathBuf>> {
    println!("=== 0. 装载**之前**的能力清单（M5 的预挂载探测）===");
    let capabilities = probe_capabilities(dll)?;
    println!(
        "  {} → inject={:?} provision={:?} volatile={:?}",
        capabilities.plugin, capabilities.inject, capabilities.provision, capabilities.volatile
    );
    assert!(
        capabilities.provision.contains(&"sessions@1".to_string()),
        "探测必须能读到 provision 声明（这正是 M5 的生产路径）"
    );
    assert_eq!(
        loaded_instances(),
        0,
        "探测是「解析 → 暂存 → 打开 → 探活 → 放手」，不许挂载任何实例"
    );
    println!("  此刻 loaded_instances() = 0：清单是**没挂载**就能拿到的");

    // §0.1 的清单里也有 `cordis-cli`：这里用它自己的库面审阅**同一份配置**——
    // 逐行列出 inject / provision / volatile，并断言 `mounted == 0`（A5 的判据）。
    // 用 `inspect_entries`（而不是 `inspect`）是为了显式给搜索目录：插件在 **exe 目录**，
    // 不在配置文件所在目录——这条 seam 与 CLI 的 `--plugin-dir` 是同一个。
    let text = std::fs::read_to_string(config_path)?;
    let entries = cordis_host::patch::parse_entries(&text).map_err(to_error)?;
    let inspected =
        cordis_cli::inspect_entries(&entries, &[exe_dir.to_path_buf()]).map_err(to_error)?;
    assert_eq!(inspected.mounted, 0, "inspect 永不挂载（A5）");
    assert_eq!(inspected.rows.len(), 1, "这份配置只有一行：{inspected:?}");
    assert!(
        inspected.rows[0]
            .provision
            .contains(&"sessions@1".to_string()),
        "行级 provision 必须能看到：{inspected:?}"
    );
    println!("  另外用 cordis-cli 的库面审阅同一份配置：");
    for line in inspected.render().lines() {
        println!("    {line}");
    }

    // ---------------------------------------------------------------- T1 侧
    println!("=== 1. T1 侧就位：registry（槽位）+ tools@1 + llm 槽位 ===");
    let app = App::new();
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let logs = logs.clone();
        app.store
            .set_log_sink(Arc::new(move |level, name, message| {
                logs.lock()
                    .unwrap()
                    .push(format!("{level:?}:{name}:{message}"));
            }));
    }
    let Core { app, registry } =
        boot_core_in(app, scripted_llm("scripted", plan_for_session())).await?;
    println!("  tools@1 与 llm@1（槽位）都装好了");

    // `@native:harness-child` 的解析面：进程级原生组件注册表（M1）。
    struct ChildRegistry;
    impl NativeRegistry for ChildRegistry {
        fn native(&self, name: &str) -> Option<Arc<dyn Plugin>> {
            (name == "harness-child").then(|| {
                cordis_core::plugin_fn("harness-child", Inject::default(), |ctx: Ctx, _cfg| {
                    Box::pin(async move {
                        ctx.provide(
                            "harness-child.hello",
                            Dyn::new(ExternValue(json!({ "child": true, "where": "isolated" }))),
                        )?;
                        Ok(())
                    })
                })
            })
        }
    }
    set_native_registry(Arc::new(ChildRegistry));

    // ---------------------------------------------------------------- 真 DLL
    println!("=== 2. 配置驱动装载 harness_tool.dll（真动态库 + 真 ABI）===");
    let loader_config: loader::LoaderConfig = serde_json::from_value(json!({
        "path": config_path.to_string_lossy(),
        "pluginDir": exe_dir.to_string_lossy(),
    }))?;
    let state_slot: Arc<Mutex<Option<Arc<loader::LoaderState>>>> = Arc::new(Mutex::new(None));
    // 装载器**本身**挂成一个插件 fiber（demo 的 `loader()` 就是这个形态）：每次装载的
    // 行 fiber 是它的**孩子**，于是收尾时拆它就等于把行 fiber 与装载器一起回收。
    // 这里自己写一层，是因为要拿到 `LoaderState`（A11 的**手动**重载要用）。
    let loader_host = {
        let slot = state_slot.clone();
        let config = loader_config.clone();
        cordis_core::plugin_fn(
            "harness-loader",
            Inject::default(),
            move |ctx: Ctx, _cfg: Config| {
                let slot = slot.clone();
                let config = config.clone();
                Box::pin(async move {
                    let state = loader::load_with_source(
                        ctx.clone(),
                        config,
                        Arc::new(loader::DiskPluginSource),
                    )
                    .await?;
                    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state);
                    Ok(())
                })
            },
        )
    };
    let loader_fiber = app.root.plugin(loader_host, Config::Null);
    loader_fiber.settle().await;
    assert_eq!(
        loader_fiber.state(),
        FiberState::Active,
        "装载器宿主必须激活"
    );
    let state = state_slot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .ok_or_else(|| -> Error { "装载器没有把状态放出来".into() })?;
    let fiber = state
        .fiber_of("harness")
        .ok_or_else(|| -> Error { "装载器没有登记 harness 这一行".into() })?;
    fiber.settle().await;
    assert_eq!(fiber.state(), FiberState::Active, "动态库插件必须装载成功");

    let report = app
        .root
        .require_as::<ExternValue>("loader.report")?
        .0
        .clone();
    assert_eq!(
        report
            .get("failed")
            .and_then(Value::as_array)
            .map(|list| list.len()),
        Some(0),
        "不应有条目装载失败：{report}"
    );
    println!("  报告里的能力清单：{}", report["capabilities"]);

    // ---------------------------------------------------------------- 插件自报面
    println!("=== 3. 插件自报的状态（宿主经 iface 读，活的值）===");
    let first = dll_status(&app).await?;
    let handle = first["handle"].as_u64().unwrap_or(0);
    println!(
        "  boot={} seq={} handle={} isolate={} child={} intercepted={} armed={}",
        first["boot"],
        first["seq"],
        first["handle"],
        first["isolate"],
        first["child"],
        first["intercepted"],
        first["armed"]
    );
    assert_eq!(first["boot"], json!(1), "第一次装载");
    assert_eq!(
        first["seq"],
        json!(3),
        "boot 记录占了 seq=1，M3 的 meta-probe 占了 seq=2 ⇒ 下一个是 3"
    );
    assert!(handle > 0, "插件必须自报句柄（后面用它看挂起表）：{first}");
    assert_eq!(
        first["registered"],
        json!("ok"),
        "D2：T2 的 iface 必须被 T1 的目录收下：{first}"
    );
    assert_eq!(
        first["meta"],
        json!({ "tenant": "harness", "trace": "spike" }),
        "M3：**提供者侧**必须读到调用点上叠的 intercept 元数据（`d(k) ⊕ ι(k)`）"
    );
    assert!(
        first["meta_error"].is_null(),
        "meta-probe 不该失败：{first}"
    );

    println!("=== 4. A4：派生隔离域里的 @native 子组件 ===");
    let isolate = first["isolate"].as_u64().unwrap_or(0);
    let child = first["child"].as_u64().unwrap_or(0);
    assert!(
        isolate > 0 && child > 0,
        "isolate 与 use_child 都必须成功：{first}"
    );
    let child_value = derived_json(child, "harness-child.hello")?;
    println!("  子组件（child 句柄）读到 harness-child.hello = {child_value}");
    assert_eq!(child_value["child"], json!(true));
    assert!(
        app.root.get("harness-child.hello").is_none(),
        "根域不得看见隔离域里的子组件（域隔离的直接证据）"
    );
    println!("  根域读 harness-child.hello → None（隔离的直接证据）");

    // ---------------------------------------------------------------- A7
    println!("=== 5. A7：原地换 llm 适配器 ⇒ agent-loop 的 epoch 不变 ===");
    let agents = boot_agents(&app.root, &["alpha"]).await?;
    let alpha = agents.get("alpha").expect("alpha 必须已挂上").ctx.clone();
    let epoch = agents["alpha"].fiber.running_epoch();
    println!(
        "  换之前：adapter = {}",
        agent_of(&alpha).adapter().await.map_err(to_error)?
    );
    registry.replace(LlmService::KEY, replay_llm("replay", vec![]))?;
    agents["alpha"].fiber.settle().await;
    assert_eq!(
        agents["alpha"].fiber.running_epoch(),
        epoch,
        "换适配器不得重挂消费方（A7）"
    );
    let swapped = agent_of(&alpha).adapter().await.map_err(to_error)?;
    assert_eq!(swapped, "replay", "新值必须可见");
    println!("  换之后：adapter = {swapped}，epoch 不变（{epoch:?}）");
    // 换回脚本适配器：它给出这场会话的三步计划。
    registry.replace(
        LlmService::KEY,
        scripted_llm("scripted", plan_for_session()),
    )?;
    agents["alpha"].fiber.settle().await;
    assert_eq!(
        agents["alpha"].fiber.running_epoch(),
        epoch,
        "再换一次同样不许重挂"
    );
    println!("  再换回 scripted：epoch 仍然不变");

    // ---------------------------------------------------------------- 会话
    println!("=== 6. 一场真会话（A9 拦截 + T2 iface 工具 + T2 异步工具 + sessions 记账）===");
    mount_tool(&alpha, "alpha", "search").await;
    let report = run_session(&app, alpha.clone(), handle, "把这两件事办完").await?;
    println!("  工具序列：{:?}", report.tools);
    println!("  未应答：{:?}（隔离的反证）", report.unanswered);
    println!("  拦截痕迹：{:?}", report.stages);
    println!("  会话 seq：{:?}", report.seqs);
    assert_eq!(report.tools, vec!["search", "harness-echo", "harness-ask"]);
    assert!(
        report.unanswered.is_empty(),
        "三步都必须有人应答：{report:?}"
    );
    assert_eq!(report.stages, vec!["harness-tool"], "A9：改写必须留下痕迹");
    assert_eq!(
        report.seqs,
        vec![3, 4, 5, 6, 7],
        "session-start + 三步 + session-end（boot 与 meta-probe 占了 1、2）"
    );
    assert_eq!(pending_len(handle), 0, "会话结束不许留下挂起");
    assert_eq!(
        dll_status(&app).await?["intercepted"],
        json!(3),
        "三步都过了拦截链"
    );
    let intercept_record = first_tool_record(log_path)?;
    assert_eq!(
        intercept_record["input"],
        json!({ "q": "cordis", "guard": "harness-tool" }),
        "A9：拦截器改写的**入参**必须到执行者手里（也进了日志）"
    );
    println!(
        "  日志里的第一条工具记录：tool={} input={} output={}",
        intercept_record["tool"], intercept_record["input"], intercept_record["output"]
    );
    let records = read_log(log_path)?;
    println!(
        "  会话日志：{} 行，seq = {:?}",
        records.len(),
        seqs_of(&records)
    );
    assert_eq!(
        seqs_of(&records),
        (1..=7).collect::<Vec<u64>>(),
        "seq 必须从 1 连续"
    );

    println!("=== 7. A10：取消一次在飞的工具调用 ===");
    let token = CancelToken::new();
    let inflight = app.store.dispatch_cancellable(
        Mode::Bail,
        "tools/invoke",
        &alpha,
        vec![Dyn::new(ExternValue(json!({
            "agent": "alpha",
            "tool": "harness-ask",
            "input": { "from": "cancel" },
            "output": null,
            "stages": [],
        })))],
        None,
        None,
        Some(token.clone()),
    );
    let task = tokio::spawn(inflight);
    wait_until("取消目标进入挂起", || pending_len(handle) == 1).await;
    token.cancel();
    let settled = task.await.expect("派发任务不得 panic");
    assert!(settled.is_none(), "被取消的监听器等于没有意见");
    assert_eq!(pending_len(handle), 0, "取消桥必须把挂起条目移出");
    let text = logs.lock().unwrap().join("\n");
    assert!(text.contains("已取消"), "结算必须说明是取消：{text}");
    println!(
        "  挂起（pending_len=1）→ token.cancel() → 结算为 None、pending_len=0、日志写明「已取消」"
    );
    // A10 在插件侧还留着一条**已作废**的武装条目（它在宿主结算之后才记下 id）。
    // `harness/release` 会 drain 掉所有武装条目 ⇒ 顺手冲干净，后面的会话才是干净的起点。
    app.root.emit("harness/release", vec![]).await;
    assert_eq!(
        dll_status(&app).await?["armed"],
        json!(0),
        "取消之后残留的武装条目必须被冲掉"
    );

    // ---------------------------------------------------------------- A11
    println!("=== 8. A11：一次真 DLL 重载，seq 仍连续、日志只增不改 ===");
    let before_bytes = std::fs::read(log_path)?;
    let before_status = dll_status(&app).await?;
    println!(
        "  重载前：boot={} seq={} 行数={}",
        before_status["boot"],
        before_status["seq"],
        read_log(log_path)?.len()
    );
    let epoch_before_reload = agents["alpha"].fiber.running_epoch();

    // "重新编译"的最朴素等价物：同一份字节写回去，只改 mtime（与 demo 的手法一致）。
    // 触发方式是**确定的**：触碰文件 + 手动驱动 `loader::reload_one`，不走墙钟 HMR。
    let bytes = std::fs::read(dll)?;
    std::fs::write(dll, &bytes)?;
    loader::reload_one(&app.root, &state, "harness").await?;

    let mut reloaded = Value::Null;
    for _ in 0..200 {
        if let Ok(status) = dll_status(&app).await {
            if status["boot"] == json!(2) {
                reloaded = status;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let fiber_after_reload_note = state.fiber_of("harness").is_some();
    assert!(fiber_after_reload_note, "重载后装载表里必须还是这一行");
    assert_eq!(
        reloaded["boot"],
        json!(2),
        "重载后必须是**新实例**（boot=2）：{reloaded}"
    );
    assert_eq!(
        reloaded["seq"],
        json!(10),
        "重载前的下一个 seq 是 8 ⇒ 新实例的 boot 记录拿到 8、meta-probe 拿到 9、下一个是 10。\
         换言之：**seq 从日志恢复**，不靠内存里那个计数器（它随实例一起没了）：{reloaded}"
    );
    let after_bytes = std::fs::read(log_path)?;
    assert!(
        after_bytes.starts_with(&before_bytes),
        "日志必须 append-only：重载只许**追加**"
    );
    assert_eq!(
        read_log(log_path)?.len(),
        9,
        "重载追加了 boot 与 meta-probe 两条记录"
    );
    println!(
        "  重载后：boot={} seq={} 行数={}；旧文件是新文件的前缀（append-only）",
        reloaded["boot"],
        reloaded["seq"],
        read_log(log_path)?.len()
    );
    // 新实例的 handle 是新的：之后所有"挂起表"断言都用新的。
    let handle = reloaded["handle"].as_u64().unwrap_or(handle);
    assert_eq!(pending_len(handle), 0, "新实例不该带着旧实例的挂起");

    // 提供者换了新实例 ⇒ 依赖者被**自动**重拉（这正是 cordis 的级联）。
    agents["alpha"].fiber.settle().await;
    assert_ne!(
        agents["alpha"].fiber.running_epoch(),
        epoch_before_reload,
        "sessions 的提供者换了实例 ⇒ 依赖它的 agent-loop 必须被自动重拉"
    );
    println!("  agent-loop 被自动重拉（epoch 变了）——它自己没做任何事");

    let second = run_session(&app, alpha.clone(), handle, "重载之后的第二场").await?;
    let third = run_session(&app, alpha.clone(), handle, "重载之后的第三场").await?;
    println!("  重载后第一场 seq：{:?}", second.seqs);
    println!("  重载后第二场 seq：{:?}", third.seqs);
    assert_eq!(second.seqs, vec![10, 11, 12, 13, 14], "跨重载仍然连续");
    assert_eq!(third.seqs, vec![15, 16, 17, 18, 19], "第三场接着第二场");
    let records = read_log(log_path)?;
    assert_eq!(
        seqs_of(&records),
        (1..=19).collect::<Vec<u64>>(),
        "整份日志的 seq 必须从 1 连续到 19（一次重载、三场会话）"
    );
    println!(
        "  日志：{} 行，seq = 1..{}（一次重载、三场会话）",
        records.len(),
        records.len()
    );

    // ---------------------------------------------------------------- 流（ADR 0001 §5）
    println!("=== 8.5 流：T2 推块 + 宿主窗口 4 + credit 背压（ADR 0001 §5）===");
    let mut stream = app
        .root
        .call_iface_stream("harness.stream@1", 1, "stream", json!({ "n": 20 }))
        .expect("开流必须成功");
    assert_eq!(streams_len(handle), 1, "开流之后宿主要记着这条流");
    let mut seen = Vec::new();
    while let Some(chunk) = stream.next().await.expect("这条流不该失败") {
        seen.push(chunk);
    }
    assert_eq!(
        seen,
        (1..=20).map(|index| json!(index)).collect::<Vec<Value>>(),
        "真 DLL 推的块必须按序、不丢不重"
    );
    assert_eq!(streams_len(handle), 0, "流结束后记录必须被摘");
    assert_eq!(pending_len(handle), 0, "流结束后不留挂起条目");
    let status = dll_status(&app).await?;
    let sent = status["stream_sent"].as_u64().unwrap_or(0);
    let blocked = status["stream_blocked"].as_u64().unwrap_or(0);
    assert_eq!(sent, 20, "每一块都推成功：{status}");
    assert!(blocked >= 1, "窗口必须真的挡过（否则背压是空的）：{status}");
    println!("  收到 20 块（按序）；DLL 被窗口挡过 {blocked} 次、推成功 {sent} 块");

    // ---------------------------------------------------------------- 自我卸载
    println!("=== 9. A4：插件自我卸载（dispose_self）===");
    let staged_while_loaded = staged_copies(exe_dir);
    assert!(
        staged_while_loaded.len() >= 2,
        "重载会带来第二份暂存副本：{staged_while_loaded:?}"
    );
    // 先记下这一行的 fiber：`ctx_dispose_self` 是异步的，loader 行先消失，
    // 不代表 effect / OP_DISPOSE / 实例注销已经完成。用同一个 fiber 的 dispose
    // 作为收敛点，避免把“行已摘除”误当成“清理已完成”。
    let row = state
        .fiber_of("harness")
        .expect("自我卸载前 harness 行必须存在");
    app.root.emit("harness/unload", vec![]).await;
    let mut gone = false;
    for _ in 0..200 {
        if state.fiber_of("harness").is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "自我卸载必须让装载器摘掉这一行");
    assert!(
        !state.mounted_labels().contains(&"harness".to_string()),
        "装载表里不该再留着这一行：{:?}",
        state.mounted_labels()
    );
    // 同一个 fiber 的 dispose 是幂等的：这里等完整的卸载链路，而不是等墙钟。
    row.dispose().await;
    assert!(
        row.cleanup_error().is_none(),
        "自我卸载清理不得报告异常：{:?}",
        row.cleanup_error()
    );
    assert_eq!(row.effect_count(), 0, "自我卸载必须清空恢复栈");
    assert_eq!(loaded_instances(), 0, "自我卸载必须注销实例");
    // 对账一次：让**报告**反映这次自我卸载（报告是实时诊断面，刷新发生在对账里）。
    let outcome = state.sync().await;
    let report = app
        .root
        .require_as::<ExternValue>("loader.report")?
        .0
        .clone();
    assert_eq!(
        report["disabled"],
        json!(["harness"]),
        "自我卸载必须被装载器认出来（树侧 disabled + 落盘）：{report}"
    );
    println!(
        "  装载表已摘除这一行；对账一次后报告里 disabled={}（应用 {} 项）",
        report["disabled"],
        outcome.applied()
    );
    let err = app
        .root
        .call_iface("sessions@1", 1, "next_seq", json!([]))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("sessions@1"),
        "卸载错误必须指向 sessions@1：{err}"
    );
    println!("  卸载后 sessions@1 不再可解析：{err}");
    // 目录（T1 那边的状态）里也不该留下它的登记：这不是内核能回收的东西，
    // 得由插件自己的 cleanup 显式撤销（撤销没做到，它就会吊住提供者的实例）。
    let listed = app
        .root
        .call_iface("tools@1", 1, "names", json!([[0]]))
        .await?;
    assert_eq!(
        listed,
        json!([]),
        "插件卸载后目录里不该留下它的登记：{listed}"
    );
    println!("  目录里也没有它的登记了（cleanup 里的 unregister 真的跑了）");
    // 日志是盘上的真相：插件走了，一条记录都不少。
    assert_eq!(read_log(log_path)?.len(), 19, "卸载不该动日志");
    println!("  日志仍是 19 行（插件走了，盘上的记录一条不少）");

    // 与 demo 同一条纪律（G11）：**装载器宿主 fiber 自己也要显式拆**。
    // 两个理由叠在一起：① `Store` 的 commit 观察者与 `Ctx` 互相持强引用，只有 fiber
    // dispose 清空 `Fiber.ctx` 才能打破那个环；② 行 fiber 是它的**孩子**（`Fiber::new`
    // 把"子卸载"注册成父的一个 effect），拆它才会把那些 effect 闭包（它们强持行 fiber）
    // 一起放掉——否则 `Arc<LoadedPlugin>` 永远活着，暂存副本也就永远不删。
    loader_fiber.dispose().await;

    Ok(staged_while_loaded)
}

/// 跑一场会话：它一定会在 `harness-ask` 那一步挂起，宿主在这里**放行**。
///
/// D3 的时序纪律：不睡墙钟——等的是 `pending_len == 1` 这个**可观测的收敛点**。
async fn run_session(app: &App, ctx: Ctx, handle: u64, prompt: &str) -> Result<Report> {
    let prompt = prompt.to_string();
    let task = tokio::spawn(async move { agent_of(&ctx).run(prompt).await.map_err(to_error) });
    let mut released = false;
    for _ in 0..400 {
        if pending_len(handle) == 1 {
            // **两侧都到位**才算"挂起 + 已武装"：宿主先登记挂起条目、插件的回调随后才
            // 记下 `dispatch_id`——只看 `pending_len` 会在插件还没武装时就放行（放了个空炮）。
            if let Ok(status) = dll_status(app).await {
                if status["armed"].as_u64().unwrap_or(0) >= 1 {
                    app.root.emit("harness/release", vec![]).await;
                    released = true;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        released,
        "会话里的异步工具必须真的挂起过（否则 M2 的挂起路径没被走到）"
    );
    let report = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .map_err(|_| -> Error { "会话超时：挂起的异步工具没有被放行？".into() })?
        .expect("会话任务不得 panic")?;
    Ok(report)
}

/// typed 访问器的薄包装。
fn agent_of(ctx: &Ctx) -> Arc<dyn Agent> {
    AgentService::of(ctx).expect("agent@1 必须已提供")
}

/// 这场会话的三步计划：T1 钩子 / T2 iface（经 T1 目录转发）/ T2 异步（挂起后放行）。
fn plan_for_session() -> Vec<Plan> {
    vec![
        Plan {
            tool: "search".to_string(),
            input: json!({ "q": "cordis" }),
        },
        Plan {
            tool: "harness-echo".to_string(),
            input: json!({ "say": "hi" }),
        },
        Plan {
            tool: "harness-ask".to_string(),
            input: json!({ "question": "答案是什么" }),
        },
    ]
}

/// 写本轮运行用的 `cordis.yml`（单行：`harness_tool`，config 里给会话日志路径）。
///
/// **每次运行都重写**：插件自我卸载会被装载器落盘成 `disabled: true`，
/// 不重写的话下一次运行就什么都装不上了（那一行会一直是休眠的）。
fn write_config(config_dir: &Path, log_path: &Path) -> Result<PathBuf> {
    // YAML 双引号串里 `\` 是转义符 ⇒ Windows 路径一律写成正斜杠。
    let log = log_path.to_string_lossy().replace('\\', "/");
    let yaml = format!("- id: harness\n  name: harness_tool\n  config:\n    log: \"{log}\"\n");
    let path = config_dir.join("cordis.yml");
    std::fs::write(&path, yaml)?;
    Ok(path)
}

/// 读 dll 的**活**状态（`harness.status@1` 的 iface）。
async fn dll_status(app: &App) -> Result<Value> {
    app.root
        .call_iface("harness.status@1", 1, "status", json!([]))
        .await
}

/// 读会话日志（append-only 的 JSONL）。
fn read_log(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line).map_err(|err| -> Error { err.to_string().into() })
        })
        .collect()
}

fn seqs_of(records: &[Value]) -> Vec<u64> {
    records
        .iter()
        .filter_map(|record| record["seq"].as_u64())
        .collect()
}

/// 日志里第一条 `kind == "tool"` 的记录。
fn first_tool_record(path: &Path) -> Result<Value> {
    read_log(path)?
        .into_iter()
        .find(|record| record["kind"] == json!("tool"))
        .ok_or_else(|| -> Error { "会话日志里没有工具记录".into() })
}

/// 轮询等待（不比墙钟：时序由**可观测的收敛点**钉死）。
async fn wait_until<F: Fn() -> bool>(what: &str, check: F) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("等待超时：{what}");
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

/// 经派生句柄读一个名字（与 demo 的同名工具同一套裸 FFI 探针）。
///
/// 用途：断言"隔离域里读得到、根域读不到"——`ctx_get` 是宿主面，宿主侧写这条断言
/// 才叫端到端（D9：真 DLL 的断言留在二进制里）。
fn derived_json(handle: u64, name: &str) -> Result<Value> {
    let mut buffer = [0u8; 1024];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let f = host_api()
        .ctx_get
        .ok_or_else(|| -> Error { "宿主没有 ctx_get".into() })?;
    let status = unsafe { f(handle, Blob::new(name.as_bytes()), &mut out) };
    if status != STATUS_OK {
        return Err(format!(
            "ctx_get({name}) 失败：status={status} {}",
            String::from_utf8_lossy(&buffer[..out.len])
        )
        .into());
    }
    let text = String::from_utf8_lossy(&buffer[..out.len]).into_owned();
    let value: Value = serde_json::from_str(&text).map_err(|err| -> Error {
        format!("ctx_get 的应答不是 JSON：{err}（{text}）").into()
    })?;
    if value.get("missing").and_then(Value::as_bool) == Some(true) {
        return Err(format!("{name} 不在这个域里").into());
    }
    Ok(value.get("value").cloned().unwrap_or(value))
}

fn to_error<E: std::fmt::Display>(err: E) -> Error {
    err.to_string().into()
}
