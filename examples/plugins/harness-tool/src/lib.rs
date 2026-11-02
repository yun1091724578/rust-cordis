//! harness-tool：harness spike 用的**真动态库插件**（T2/T3 档）。
//!
//! 它一个人干完 A4 / A9 / A11 三件验收：
//!
//! | 能力 | 怎么做的 | 验收 |
//! |---|---|---|
//! | 拦截 `tools/before-invoke` 并改写 | `on_async` + `next` + `fulfill`（M2b） | **A9** |
//! | 提供**异步**工具（挂起后由宿主放行） | `on_async("tools/invoke")` 记下 `dispatch_id`，`harness/release` 再 `fulfill`（M2） | **A4** |
//! | 派生隔离域 + 挂 `@native:` 子组件 | `isolate` + `from_host` + `use_child`（M1） | **A4** |
//! | 设 intercept（use-site 元数据） | `intercept_ctx("sessions@1", …)` 之后调用，**提供者自己**读回 `meta`（M3） | **A4** |
//! | 自我卸载 | `harness/unload` ⇒ `dispose_self()`（M1） | **A4** |
//! | `sessions@1`（append-only、`seq` 连续、flush 屏障） | 落盘 JSONL + `sync_all`；装载时**从日志恢复** `seq` | **A11** |
//! | 把 T2 的 iface 登记进 T1 的目录（D2） | `provide_iface` + `tools@1` 的 JSON 面 `register` | — |
//!
//! # 为什么 `sessions` 必须是 T2（A11 的要害）
//! 装载器的重载会换出**新的插件对象**；若 `sessions` 是进程内的 T1 插件，重载后内存里的
//! `next_seq` 天然连续 ⇒ **假通过**。这里的 `seq` 只能从**日志**恢复，
//! 于是"重载后 seq 仍连续"才真的证明了"可从日志恢复"。
//!
//! # 状态都在 `apply` 的闭包里（没有全局静态）
//! 重载换的是**新实例**：任何进程级静态都会把两个实例的状态混在一起，让"新实例从头
//! 恢复"这件事失去意义。所以这里一个 `static` 都没有。

use cordis_abi::cordis_plugin;
use cordis_abi::sdk::{self, PluginResult};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

cordis_plugin! {
    name: "harness-tool",
    // 目录由 T1 提供（本插件往它里面登记 iface 工具）。
    inject: ["tools@1"],
    // M5 的"声明即能力"：这一行让 `cordis-cli inspect` / 装载报告在**装载前**
    // 就能列出这个 dll 会装什么。**新接口必须同步加进来**——没声明的键解析不到
    // （`NoInject`），这是"声明即能力"在消费侧的兑现。
    provision: ["sessions@1", "tool.harness-echo@1", "harness.status@1", "harness.stream@1"],
    apply: apply,
}

/// 宿主侧工具调用的载荷（与 `harness_spike::Invoke` 同形——跨 ABI 只有 JSON）。
fn invoke_fields(payload: &Value) -> (String, Value, Vec<Value>) {
    let tool = payload
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let input = payload.get("input").cloned().unwrap_or(Value::Null);
    let stages = payload
        .get("stages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    (tool, input, stages)
}

fn apply(ctx: &sdk::Ctx, config: Value) -> PluginResult {
    let ctx = *ctx;
    let log_path = config
        .get("log")
        .and_then(Value::as_str)
        .ok_or("harness-tool 需要配置 `log`（会话日志的路径）")?
        .to_string();
    let log = Arc::new(SessionLog::open(PathBuf::from(&log_path))?);
    let boot = log.append(json!({
        "kind": "boot",
        "plugin": "harness-tool",
        "layer": "dll",
    }))?;
    let boots = log.boots();
    ctx.log(
        1,
        &format!(
            "harness-tool: 第 {boots} 次装载（日志 {log_path}；boot 记录 seq={boot}，下一个 seq={}）",
            log.next_seq()
        ),
    );

    let intercepted = Arc::new(AtomicU64::new(0));
    let registered = Arc::new(Mutex::new("<还没登记>".to_string()));
    let last_meta = Arc::new(Mutex::new(Value::Null));
    // 挂起的异步工具调用：`(dispatch_id, 原始载荷)`——结算时要在原载荷上填 `output`。
    let armed: Arc<Mutex<Vec<(u64, Value)>>> = Arc::new(Mutex::new(Vec::new()));

    // ------------------------------------------------ ① A9：拦截 tools/before-invoke
    //
    // 这是 waterfall：**调用前**的链。`next` 把（可能已改写的）载荷交给下游，
    // 下游的决定回来之后我们**再改写一次结果**（A9 的"调 next 后改写"）。
    let intercept_count = intercepted.clone();
    let ctx_intercept = ctx;
    ctx.on_async("tools/before-invoke", move |args, dispatch_id| {
        // `on_async` 的处理器是 `Fn`（可能被调用多次）⇒ 每次调用自己克隆一份计数句柄。
        let count = intercept_count.clone();
        ctx_intercept.next(dispatch_id, args, move |downstream| {
            let mut value = downstream.unwrap_or(Value::Null);
            if let Value::Object(map) = &mut value {
                // ① 改写**调用参数**：执行者会看到这个标记。
                if let Some(Value::Object(input)) = map.get_mut("input") {
                    input.insert("guard".to_string(), json!("harness-tool"));
                }
                // ② 改写**结果**：压上自己这一环的名字（链上痕迹）。
                match map.get_mut("stages") {
                    Some(Value::Array(stages)) => stages.push(json!("harness-tool")),
                    _ => {
                        map.insert("stages".to_string(), json!(["harness-tool"]));
                    }
                }
            }
            count.fetch_add(1, Ordering::SeqCst);
            let _ = ctx_intercept.fulfill(dispatch_id, value);
        })?;
        Ok(sdk::AsyncReply::Pending)
    })?;

    // ------------------------------------------------ ② A4：异步工具（挂起 → 放行）
    let armed_invoke = armed.clone();
    ctx.on_async("tools/invoke", move |args, dispatch_id| {
        let payload = args.first().cloned().unwrap_or(Value::Null);
        let (tool, _input, _stages) = invoke_fields(&payload);
        if tool != "harness-ask" {
            // 不是我的工具 ⇒ **不表态**（`null` 即"没有意见"，bail 继续往后走）。
            return Ok(sdk::AsyncReply::Reply(Value::Null));
        }
        armed_invoke
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((dispatch_id, payload));
        Ok(sdk::AsyncReply::Pending)
    })?;

    // 放行：宿主发 `harness/release` 才结算（**由调用方控制时序**，不睡墙钟）。
    let armed_release = armed.clone();
    let ctx_release = ctx;
    ctx.on("harness/release", move |_args: Vec<Value>| {
        // 取锁 → 放锁 → 再结算（持锁回调宿主 = 同线程死锁）。
        let pending: Vec<(u64, Value)> = {
            let mut guard = armed_release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.drain(..).collect()
        };
        for (dispatch_id, payload) in pending {
            let (tool, input, mut stages) = invoke_fields(&payload);
            stages.push(json!("harness-tool"));
            let value = json!({
                "agent": payload.get("agent").cloned().unwrap_or(Value::Null),
                "tool": tool,
                "input": input,
                "output": { "answer": 42, "by": "harness-tool.dll" },
                "stages": stages,
            });
            let _ = ctx_release.fulfill(dispatch_id, value);
        }
        Ok(json!({ "released": true }))
    })?;

    // ------------------------------------------------ ③ A11：sessions@1（T2 的载体）
    let append_log = log.clone();
    let append_meta = last_meta.clone();
    ctx.provide_iface(
        "sessions@1",
        1,
        vec![
            (
                // 参数信封与 T1 服务逐字同形（`[record]`），返回**分配到的 seq**。
                "append",
                Arc::new(move |args: Value, meta: Option<Value>| {
                    if let Some(seen) = meta {
                        // M3：**提供者侧**读到的 use-site 元数据（`d(k) ⊕ ι(k)`）。
                        *append_meta
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = seen;
                    }
                    let record = args.get(0).cloned().unwrap_or(Value::Null);
                    append_log.append(record).map(|seq| json!(seq))
                }),
            ),
            (
                "tail",
                Arc::new({
                    let tail_log = log.clone();
                    move |args: Value, _meta: Option<Value>| {
                        let count = args.get(0).and_then(Value::as_u64).unwrap_or(0) as u32;
                        tail_log.tail(count).map(Value::Array)
                    }
                }),
            ),
            (
                "next_seq",
                Arc::new({
                    let seq_log = log.clone();
                    move |_args: Value, _meta: Option<Value>| Ok(json!(seq_log.next_seq()))
                }),
            ),
        ],
    )?;

    // ------------------------------------------------ ④ D2：iface 工具 + 登记进 T1 目录
    ctx.provide_iface(
        "tool.harness-echo@1",
        1,
        vec![(
            // 目录约定：iface 工具只有一个方法 `call`，入参是**裸 JSON**（不走信封）。
            "call",
            Arc::new(|args: Value, _meta: Option<Value>| {
                Ok(json!({ "tool": "harness-echo", "via": "iface", "echo": args }))
            }),
        )],
    )?;
    let registered_slot = registered.clone();
    ctx.call_iface(
        "tools@1",
        1,
        "register",
        json!([
            0,
            "harness-echo",
            "harness-tool.dll 提供的 iface 工具",
            "tool.harness-echo@1"
        ]),
        move |result| {
            let outcome = match result {
                Ok(_) => "ok".to_string(),
                Err(message) => format!("err: {message}"),
            };
            *registered_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = outcome;
        },
    )?;

    // ------------------------------------------------ ⑤ A4：派生隔离域 + @native 子组件
    //
    // `isolate` 的语义是**按服务名**分域：`key` 必须是要藏起来的那个名字
    // （契约：`@native:harness-child` 提供 `harness-child.hello`），
    // 空 label = **行私有域**（非空则是跨行共享的具名域）。
    let isolate = ctx.isolate("harness-child.hello", "")?;
    let host = sdk::attached_host().ok_or("宿主表还没附着（attach 没跑？）")?;
    // 派生句柄指回一个 ctx：之后所有动作都发生在**那个域里**。
    let isolated = sdk::Ctx::from_host(host, isolate);
    let child = isolated.use_child(
        "@native:harness-child",
        json!({ "note": "挂在隔离域里的子组件" }),
    )?;

    // ------------------------------------------------ ⑥ A4：设 intercept + 自我卸载
    //
    // `intercept_ctx` 派生一个叠了 `ι` 的 ctx；用它调用 `sessions@1`，
    // 提供者（就是本插件）会在第二个参数里读到合并后的元数据。
    let meta_error: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let meta_error_in = meta_error.clone();
    ctx.intercept_ctx(
        "sessions@1",
        json!({ "tenant": "harness", "trace": "spike" }),
    )?
    .call_iface(
        "sessions@1",
        1,
        "append",
        json!([{ "kind": "meta-probe" }]),
        move |result| {
            if let Err(message) = result {
                *meta_error_in
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = json!(message);
            }
        },
    )?;

    let ctx_unload = ctx;
    ctx.on("harness/unload", move |_args: Vec<Value>| {
        ctx_unload.dispose_self()?;
        Ok(json!({ "unloading": true }))
    })?;

    // ------------------------------------------------ ⑦ 收尾：一个 effect 干两件事
    //
    // ① **撤销目录登记**：目录是 T1 插件的状态，内核只回收"你自己 provide 的东西"。
    //    不撤销的话，重载后的新实例会撞上"自己留下的"那条重名登记。
    // ② flush：不是 spawn 一个后台刷盘任务——卸载时**同步**刷干净，语义才说得清。
    //    （cleanup 跑在句柄注销**之前**，所以这一刻还能调宿主，见 `apply_via` 的契约。）
    let cleanup_log = log.clone();
    let ctx_cleanup = ctx;
    ctx.effect(move || {
        let _ = ctx_cleanup.call_iface(
            "tools@1",
            1,
            "unregister",
            json!([0, "harness-echo"]),
            |_| {},
        );
        let _ = cleanup_log.flush();
    })?;

    // ------------------------------------------------ ⑧ 把这一切报给宿主（诊断面）
    //    流的两个计数器先造：诊断面要报它们，而流式接口（⑨）也要用同一对。
    let stream_sent = Arc::new(AtomicU64::new(0));
    let stream_blocked = Arc::new(AtomicU64::new(0));
    let status = Arc::new(Status {
        log: log.clone(),
        handle: ctx.handle(),
        isolate,
        child,
        intercepted: intercepted.clone(),
        registered: registered.clone(),
        last_meta: last_meta.clone(),
        meta_error: meta_error.clone(),
        armed: armed.clone(),
        stream_sent: stream_sent.clone(),
        stream_blocked: stream_blocked.clone(),
    });
    let status_iface = status.clone();
    ctx.provide_iface(
        "harness.status@1",
        1,
        vec![(
            "status",
            Arc::new(move |_args: Value, _meta: Option<Value>| Ok(status_iface.snapshot())),
        )],
    )?;
    ctx.provide("harness.status", status.snapshot())?;

    // ------------------------------------------------ ⑨ 流（ADR 0001 §5）
    //
    // 真 DLL 的流式提供者：**方法体只把窗口灌满**（宿主的窗口是 4），剩下的靠在
    // 宿主归还额度时的回调里继续推——这正是"插件不能在 FFI 里 await"的落点：
    // 推不动就挂起（`AsyncReply::Pending`），宿主每被取走一块就回调一次 `on_credit`。
    ctx.provide_iface_async(
        "harness.stream@1",
        1,
        vec![(
            "stream",
            Arc::new(move |args: Value, _meta: Option<Value>, dispatch_id: u64| {
                let total = args.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
                let streamer = Arc::new(Streamer {
                    ctx,
                    dispatch_id,
                    total,
                    sent: stream_sent.clone(),
                    blocked: stream_blocked.clone(),
                    done: AtomicBool::new(false),
                });
                Ok(if streamer.pump() {
                    // 被窗口挡住：挂起，等宿主把额度送回来（`on_credit`）。
                    sdk::AsyncReply::Pending
                } else {
                    sdk::AsyncReply::Reply(Value::Null)
                })
            }),
        )],
    )?;
    ctx.log(
        1,
        "harness-tool: 拦截器 / 异步工具 / sessions / iface 工具 / 隔离域都已就位",
    );
    Ok(())
}

/// 一次流式调用的推进器（真 DLL 侧）：拿到一个额度就推一块。
///
/// 递归形态不是巧合：**额度回调就是"又能推了"的信号**，所以推进就发生在回调里
/// （消费者取走一块 ⇒ 宿主回调 ⇒ 这里推下一块）。`Arc<Self>` + `AtomicU64`
/// 让它能被回调闭包反复进入，而 `Fn` 闭包不能 move 出捕获值（E0507）的坑也避开了。
struct Streamer {
    ctx: sdk::Ctx,
    dispatch_id: u64,
    total: usize,
    sent: Arc<AtomicU64>,
    blocked: Arc<AtomicU64>,
    done: AtomicBool,
}

impl Streamer {
    /// 尽量把窗口灌满。返回 `true` = 被窗口挡住（这次调用要挂起）。
    fn pump(self: &Arc<Self>) -> bool {
        loop {
            if self.done.load(Ordering::SeqCst) {
                return false;
            }
            let next = self.sent.load(Ordering::SeqCst) as usize + 1;
            if next > self.total {
                self.done.store(true, Ordering::SeqCst);
                let _ = self.ctx.stream_close(self.dispatch_id);
                return false;
            }
            let me = self.clone();
            let outcome = self
                .ctx
                .stream_push(self.dispatch_id, json!(next), move |credit| match credit {
                    // 又有位置了：接着推（回调发生在宿主归还额度的那一刻）。
                    sdk::StreamCredit::Window(_) => {
                        me.pump();
                    }
                    // 流已经不在了（消费者收场 / 插件被卸载）：停手，也别再收尾。
                    sdk::StreamCredit::Gone => me.done.store(true, Ordering::SeqCst),
                });
            match outcome {
                Ok(sdk::StreamPush::Sent) => {
                    self.sent.fetch_add(1, Ordering::SeqCst);
                }
                Ok(sdk::StreamPush::Pending) => {
                    self.blocked.fetch_add(1, Ordering::SeqCst);
                    return true;
                }
                // Gone（流没了）或错误（宿主太旧）：停手。
                _ => {
                    self.done.store(true, Ordering::SeqCst);
                    return false;
                }
            }
        }
    }
}

/// 报给宿主的诊断面（**活的**：每次调用现算）。
struct Status {
    log: Arc<SessionLog>,
    handle: u64,
    isolate: u64,
    child: u64,
    intercepted: Arc<AtomicU64>,
    registered: Arc<Mutex<String>>,
    last_meta: Arc<Mutex<Value>>,
    meta_error: Arc<Mutex<Value>>,
    armed: Arc<Mutex<Vec<(u64, Value)>>>,
    /// 流（ADR 0001 §5）：推成功的块数 / 被窗口挡住的次数——"背压真的发生过"的证据。
    stream_sent: Arc<AtomicU64>,
    stream_blocked: Arc<AtomicU64>,
}

impl Status {
    fn snapshot(&self) -> Value {
        let lock = |slot: &Mutex<Value>| {
            slot.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        };
        json!({
            "boot": self.log.boots(),
            "seq": self.log.next_seq(),
            "handle": self.handle,
            "isolate": self.isolate,
            "child": self.child,
            "intercepted": self.intercepted.load(Ordering::SeqCst),
            "registered": self.registered.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone(),
            "meta": lock(&self.last_meta),
            "meta_error": lock(&self.meta_error),
            "armed": self.armed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(),
            "stream_sent": self.stream_sent.load(Ordering::SeqCst),
            "stream_blocked": self.stream_blocked.load(Ordering::SeqCst),
            "log": self.log.path.to_string_lossy(),
        })
    }
}

/// append-only 的会话日志（A11）：**落盘即真相**。
///
/// 恢复只读文件：`next_seq` 从最后一条记录的 `seq` 推出来。于是"重载后 seq 连续"
/// 等价于"新实例真的从日志恢复了"，而不是"内存里那个计数器碰巧还在"。
struct SessionLog {
    path: PathBuf,
    state: Mutex<LogState>,
}

struct LogState {
    file: std::fs::File,
    /// 下一个待分配的 `seq`（从 1 起）。
    next: u64,
    boots: u64,
}

impl SessionLog {
    fn open(path: PathBuf) -> Result<Self, String> {
        let mut next = 1u64;
        let mut boots = 0u64;
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|err| format!("读会话日志 {} 失败: {err}", path.display()))?;
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let value: Value = serde_json::from_str(line).map_err(|err| {
                    format!("会话日志 {} 里有一行不是 JSON: {err}", path.display())
                })?;
                if let Some(seq) = value.get("seq").and_then(Value::as_u64) {
                    next = next.max(seq + 1);
                }
                if value.get("kind").and_then(Value::as_str) == Some("boot") {
                    boots += 1;
                }
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("打开会话日志 {} 失败: {err}", path.display()))?;
        Ok(Self {
            path,
            state: Mutex::new(LogState { file, next, boots }),
        })
    }

    /// 追加一条记录，返回它的 `seq`。**返回即已落盘**（flush 屏障）。
    fn append(&self, record: Value) -> Result<u64, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let seq = state.next;
        let mut record = record;
        if let Value::Object(map) = &mut record {
            map.insert("seq".to_string(), json!(seq));
            if map.get("kind").and_then(Value::as_str) == Some("boot") {
                state.boots += 1;
            }
        }
        let line =
            serde_json::to_string(&record).map_err(|err| format!("会话记录无法序列化: {err}"))?;
        let describe =
            |err: std::io::Error, what: &str| format!("{what} {} 失败: {err}", self.path.display());
        state
            .file
            .write_all(line.as_bytes())
            .map_err(|err| describe(err, "写会话日志"))?;
        state
            .file
            .write_all(b"\n")
            .map_err(|err| describe(err, "写会话日志"))?;
        // 屏障：write + flush + sync_all，一个都不能少（后者才是"到了盘上"）。
        state
            .file
            .flush()
            .map_err(|err| describe(err, "flush 会话日志"))?;
        state
            .file
            .sync_all()
            .map_err(|err| describe(err, "sync 会话日志"))?;
        state.next = seq + 1;
        Ok(seq)
    }

    fn flush(&self) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .file
            .sync_all()
            .map_err(|err| format!("sync 会话日志 {} 失败: {err}", self.path.display()))
    }

    fn next_seq(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .next
    }

    fn boots(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .boots
    }

    /// 末尾 `count` 条：**从文件读**，读到的就是盘上的真相。
    fn tail(&self, count: u32) -> Result<Vec<Value>, String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|err| format!("读会话日志 {} 失败: {err}", self.path.display()))?;
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(count as usize);
        lines[start..]
            .iter()
            .map(|line| {
                serde_json::from_str::<Value>(line).map_err(|err| {
                    format!("会话日志 {} 里有一行不是 JSON: {err}", self.path.display())
                })
            })
            .collect()
    }
}
