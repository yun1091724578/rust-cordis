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
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
    let flush_log = log.clone();
    ctx.effect(move || {
        let _ = flush_log.flush();
    })?;
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
    let register_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let register_err_cb = register_err.clone();
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
                Err(message) => {
                    *register_err_cb
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.clone());
                    format!("err: {message}")
                }
            };
            *registered_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = outcome;
        },
    )?;
    if let Some(err) = register_err
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return Err(format!("向 tools@1 登记 harness-echo 失败: {err}"));
    }

    // 紧随目录登记之后立刻挂好撤销 effect：避免后续 isolate / use_child / intercept_ctx
    // 任一步失败时残留幽灵目录条目阻塞下次重载。
    let ctx_cleanup = ctx;
    if let Err(err) = ctx.effect(move || {
        let _ = ctx_cleanup.call_iface(
            "tools@1",
            1,
            "unregister",
            json!([0, "harness-echo"]),
            |_| {},
        );
    }) {
        let _ = ctx.call_iface(
            "tools@1",
            1,
            "unregister",
            json!([0, "harness-echo"]),
            |_| {},
        );
        return Err(err);
    }

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
                let streamer = Arc::new(Streamer::new(
                    CtxStreamOut(ctx),
                    dispatch_id,
                    total,
                    stream_sent.clone(),
                    stream_blocked.clone(),
                ));
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

/// 最小流输出抽象：生产走 `sdk::Ctx`，单测可注入可控窗口/并发 `on_credit` 的假宿主。
trait StreamOut: Send + Sync + 'static {
    fn push(
        &self,
        dispatch_id: u64,
        value: Value,
        on_credit: Box<dyn FnOnce(sdk::StreamCredit) + Send + 'static>,
    ) -> Result<sdk::StreamPush, String>;
    fn close(&self, dispatch_id: u64);
}

#[derive(Clone, Copy)]
struct CtxStreamOut(sdk::Ctx);

impl StreamOut for CtxStreamOut {
    fn push(
        &self,
        dispatch_id: u64,
        value: Value,
        on_credit: Box<dyn FnOnce(sdk::StreamCredit) + Send + 'static>,
    ) -> Result<sdk::StreamPush, String> {
        self.0.stream_push(dispatch_id, value, on_credit)
    }

    fn close(&self, dispatch_id: u64) {
        let _ = self.0.stream_close(dispatch_id);
    }
}

struct StreamState {
    /// **每条流独立**的已发送块计数（避免多条流复用同一计数器导致第二条流发 0 块）。
    sent: usize,
    done: bool,
    /// 单飞推进标志：防止 `pump` 与并发 `on_credit` 回调同时推进同一条流造成重复/跳号。
    pumping: bool,
    pending_pump: bool,
}

/// 一次流式调用的推进器（真 DLL 侧）：拿到一个额度就推一块。
///
/// 每条流拥有独立的 `Mutex<StreamState>` 串行化推进序号与单飞状态；
/// `total_sent` / `blocked` 仅用于插件级全局诊断遥测。
struct Streamer<O: StreamOut> {
    out: O,
    dispatch_id: u64,
    total: usize,
    total_sent: Arc<AtomicU64>,
    blocked: Arc<AtomicU64>,
    state: Mutex<StreamState>,
}

impl<O: StreamOut> Streamer<O> {
    fn new(
        out: O,
        dispatch_id: u64,
        total: usize,
        total_sent: Arc<AtomicU64>,
        blocked: Arc<AtomicU64>,
    ) -> Self {
        Self {
            out,
            dispatch_id,
            total,
            total_sent,
            blocked,
            state: Mutex::new(StreamState {
                sent: 0,
                done: false,
                pumping: false,
                pending_pump: false,
            }),
        }
    }

    fn mark_done(&self) {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        st.done = true;
    }

    /// 尽量把窗口灌满。返回 `true` = 被窗口挡住（这次调用要挂起）。
    fn pump(self: &Arc<Self>) -> bool {
        {
            let mut st = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if st.done {
                return false;
            }
            if st.pumping {
                st.pending_pump = true;
                return false;
            }
            st.pumping = true;
        }
        loop {
            let next = {
                let mut st = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if st.done {
                    st.pumping = false;
                    return false;
                }
                let next = st.sent + 1;
                if next > self.total {
                    st.done = true;
                    st.pumping = false;
                    None
                } else {
                    Some(next)
                }
            };
            let Some(next) = next else {
                self.out.close(self.dispatch_id);
                return false;
            };
            let me = self.clone();
            let outcome = self.out.push(
                self.dispatch_id,
                json!(next),
                Box::new(move |credit| match credit {
                    // 又有位置了：接着推（回调发生在宿主归还额度的那一刻）。
                    sdk::StreamCredit::Window(_) => {
                        me.pump();
                    }
                    // 流已经不在了（消费者收场 / 插件被卸载）：停手，也别再收尾。
                    sdk::StreamCredit::Gone => me.mark_done(),
                }),
            );
            match outcome {
                Ok(sdk::StreamPush::Sent) => {
                    let mut st = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    st.sent += 1;
                    st.pending_pump = false;
                    drop(st);
                    self.total_sent.fetch_add(1, Ordering::SeqCst);
                }
                Ok(sdk::StreamPush::Pending) => {
                    self.blocked.fetch_add(1, Ordering::SeqCst);
                    let mut st = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if st.pending_pump && !st.done {
                        // `on_credit` 在 `push` 返回之前已经并发到达：立即重推当前序号。
                        st.pending_pump = false;
                        continue;
                    }
                    st.pumping = false;
                    return true;
                }
                // Gone（流没了）或错误（宿主太旧）：停手。
                _ => {
                    let mut st = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    st.done = true;
                    st.pumping = false;
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
        let mut needs_trailing_newline = false;
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|err| format!("读会话日志 {} 失败: {err}", path.display()))?;
            if !text.is_empty() && !text.ends_with('\n') {
                needs_trailing_newline = true;
            }
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let value: Value = serde_json::from_str(line).map_err(|err| {
                    format!("会话日志 {} 里有一行不是 JSON: {err}", path.display())
                })?;
                if let Some(seq) = value.get("seq").and_then(Value::as_u64) {
                    let candidate = seq.checked_add(1).ok_or_else(|| {
                        format!(
                            "会话日志 {} 中的 seq={seq} 已达到 u64::MAX 上限",
                            path.display()
                        )
                    })?;
                    next = next.max(candidate);
                }
                if value.get("kind").and_then(Value::as_str) == Some("boot") {
                    boots = boots.saturating_add(1);
                }
            }
        }
        // Windows 上 Rust std 的 `OpenOptions::append(true)` 只向 CreateFileW 请求
        // `FILE_APPEND_DATA`（会忽略 `.write(true)`），导致写失败后的 `File::set_len(prev_len)`
        // 回滚返回 `ERROR_ACCESS_DENIED`。`SessionLog` 所有写入都受同一把 `Mutex` 串行保护，
        // 使用 `.read(true).write(true)` 并在写入前显式 `seek(SeekFrom::End(0))` 可在三平台
        // 同时保证追加语义与 `set_len` 原子回滚能力。
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| format!("打开会话日志 {} 失败: {err}", path.display()))?;
        if needs_trailing_newline {
            file.seek(SeekFrom::End(0))
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.flush())
                .and_then(|_| file.sync_all())
                .map_err(|err| format!("补齐会话日志 {} 尾部换行失败: {err}", path.display()))?;
        }
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
        let next_seq = seq
            .checked_add(1)
            .ok_or_else(|| "会话日志 seq 已达到 u64::MAX 上限".to_string())?;
        let mut record = record;
        let Some(map) = record.as_object_mut() else {
            return Err("会话记录必须是 JSON 对象（否则重载恢复时无法提取 seq）".to_string());
        };
        map.insert("seq".to_string(), json!(seq));
        let is_boot = map.get("kind").and_then(Value::as_str) == Some("boot");
        let mut line =
            serde_json::to_string(&record).map_err(|err| format!("会话记录无法序列化: {err}"))?;
        line.push('\n');
        let describe =
            |err: std::io::Error, what: &str| format!("{what} {} 失败: {err}", self.path.display());
        let prev_len = state
            .file
            .seek(SeekFrom::End(0))
            .map_err(|err| describe(err, "定位会话日志末尾"))?;
        let write_res = state
            .file
            .write_all(line.as_bytes())
            .map_err(|err| describe(err, "写会话日志"))
            .and_then(|_| {
                // 屏障：write + flush + sync_all，一个都不能少（后者才是"到了盘上"）。
                state
                    .file
                    .flush()
                    .map_err(|err| describe(err, "flush 会话日志"))
            })
            .and_then(|_| {
                state
                    .file
                    .sync_all()
                    .map_err(|err| describe(err, "sync 会话日志"))
            });
        if let Err(err) = write_res {
            let _ = state.file.set_len(prev_len);
            let _ = state.file.seek(SeekFrom::End(0));
            let _ = state.file.sync_all();
            return Err(err);
        }
        state.next = next_seq;
        if is_boot {
            state.boots = state.boots.saturating_add(1);
        }
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

    /// 末尾 `count` 条：**从文件读**，持状态锁与 `append` 互斥避免并发读到半截行。
    fn tail(&self, count: u32) -> Result<Vec<Value>, String> {
        let _guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;

    type CreditCallback = Box<dyn FnOnce(sdk::StreamCredit) + Send + 'static>;

    struct MockStreamOut {
        capacity: usize,
        items: Mutex<Vec<Value>>,
        in_window: Mutex<usize>,
        waiters: Mutex<VecDeque<CreditCallback>>,
        closed: AtomicBool,
        reentrant_credit_on_first_pending: AtomicBool,
    }

    impl MockStreamOut {
        fn new(capacity: usize, reentrant_credit: bool) -> Arc<Self> {
            Arc::new(Self {
                capacity,
                items: Mutex::new(Vec::new()),
                in_window: Mutex::new(0),
                waiters: Mutex::new(VecDeque::new()),
                closed: AtomicBool::new(false),
                reentrant_credit_on_first_pending: AtomicBool::new(reentrant_credit),
            })
        }

        fn drain_one(&self) {
            let waiter = {
                let mut w = self.in_window.lock().unwrap();
                if *w > 0 {
                    *w -= 1;
                }
                self.waiters.lock().unwrap().pop_front()
            };
            if let Some(cb) = waiter {
                cb(sdk::StreamCredit::Window(1));
            }
        }
    }

    impl StreamOut for Arc<MockStreamOut> {
        fn push(
            &self,
            _dispatch_id: u64,
            value: Value,
            on_credit: Box<dyn FnOnce(sdk::StreamCredit) + Send + 'static>,
        ) -> Result<sdk::StreamPush, String> {
            let mut w = self.in_window.lock().unwrap();
            if *w < self.capacity {
                *w += 1;
                self.items.lock().unwrap().push(value);
                Ok(sdk::StreamPush::Sent)
            } else {
                // Simulate reentrant/concurrent `on_credit` arriving BEFORE `push` returns `Pending`.
                if self
                    .reentrant_credit_on_first_pending
                    .swap(false, Ordering::SeqCst)
                {
                    *w -= 1;
                    drop(w);
                    on_credit(sdk::StreamCredit::Window(1));
                } else {
                    drop(w);
                    self.waiters.lock().unwrap().push_back(on_credit);
                }
                Ok(sdk::StreamPush::Pending)
            }
        }

        fn close(&self, _dispatch_id: u64) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn r2_p2_multiple_streams_have_independent_counters_and_serialized_pumps() {
        let total_sent = Arc::new(AtomicU64::new(0));
        let blocked = Arc::new(AtomicU64::new(0));

        // Stream 1: n=3 with window=2 (exercises backpressure + on_credit).
        let out1 = MockStreamOut::new(2, false);
        let s1 = Arc::new(Streamer::new(
            out1.clone(),
            1,
            3,
            total_sent.clone(),
            blocked.clone(),
        ));
        assert!(s1.pump(), "stream 1 must pause on full window");
        assert_eq!(*out1.items.lock().unwrap(), vec![json!(1), json!(2)]);
        out1.drain_one();
        assert_eq!(
            *out1.items.lock().unwrap(),
            vec![json!(1), json!(2), json!(3)]
        );
        assert!(out1.closed.load(Ordering::SeqCst));

        // Stream 2: n=3 sharing global `total_sent` MUST ALSO emit [1, 2, 3] (not 0 items!).
        let out2 = MockStreamOut::new(2, true);
        let s2 = Arc::new(Streamer::new(
            out2.clone(),
            2,
            5,
            total_sent.clone(),
            blocked.clone(),
        ));
        let _ = s2.pump();
        for _ in 0..10 {
            if out2.closed.load(Ordering::SeqCst) {
                break;
            }
            out2.drain_one();
        }
        assert_eq!(
            *out2.items.lock().unwrap(),
            vec![json!(1), json!(2), json!(3), json!(4), json!(5)],
            "第二条流及并发 on_credit 重入不得丢号、重号或提前结束"
        );
        assert!(out2.closed.load(Ordering::SeqCst));
        assert_eq!(total_sent.load(Ordering::SeqCst), 8);
    }

    #[test]
    fn r3_p2_session_log_rejects_non_object_and_serializes_concurrent_tail() {
        let dir =
            std::env::temp_dir().join(format!("cordis-r3-session-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.jsonl");

        let log = Arc::new(SessionLog::open(path.clone()).unwrap());
        assert_eq!(log.append(json!({ "kind": "boot" })).unwrap(), 1);
        assert_eq!(log.boots(), 1);
        // Non-object JSON must be rejected so seq cannot drift across reload.
        assert!(
            log.append(json!("not-an-object")).is_err(),
            "非 JSON 对象记录必须被拒绝，防止重载后 seq 重复"
        );
        assert_eq!(log.next_seq(), 2);

        // Concurrent append + tail must never observe torn/partial JSON lines.
        let writer_log = Arc::clone(&log);
        let reader_log = Arc::clone(&log);
        let writer = std::thread::spawn(move || {
            for i in 0..30u64 {
                writer_log
                    .append(json!({ "kind": "step", "i": i, "payload": "x".repeat(256) }))
                    .unwrap();
            }
        });
        let reader = std::thread::spawn(move || {
            for _ in 0..60 {
                let rows = reader_log
                    .tail(10)
                    .expect("concurrent tail must never fail");
                assert!(!rows.is_empty());
            }
        });
        writer.join().unwrap();
        reader.join().unwrap();

        // Reopen from disk and confirm seq continuity.
        let reopened = SessionLog::open(path).unwrap();
        assert_eq!(reopened.next_seq(), 32);
        assert_eq!(reopened.boots(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r4_p1_session_log_repairs_missing_trailing_newline_and_guards_seq_overflow() {
        let dir =
            std::env::temp_dir().join(format!("cordis-r4-session-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Existing file missing trailing `\n` must NOT concatenate the next JSONL line.
        let no_nl = dir.join("no-nl.jsonl");
        std::fs::write(&no_nl, r#"{"kind":"boot","seq":1}"#).unwrap();
        let log = SessionLog::open(no_nl.clone()).unwrap();
        assert_eq!(log.next_seq(), 2);
        assert_eq!(log.append(json!({ "kind": "step" })).unwrap(), 2);
        let rows = log
            .tail(10)
            .expect("tail after missing-newline recovery must parse cleanly");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["seq"], json!(1));
        assert_eq!(rows[1]["seq"], json!(2));
        let reopened = SessionLog::open(no_nl).unwrap();
        assert_eq!(reopened.next_seq(), 3);

        // 2. Existing log containing seq == u64::MAX must fail closed without overflow panic.
        let overflow_file = dir.join("overflow.jsonl");
        std::fs::write(
            &overflow_file,
            format!("{{\"kind\":\"boot\",\"seq\":{}}}\n", u64::MAX),
        )
        .unwrap();
        assert!(
            SessionLog::open(overflow_file).is_err(),
            "seq == u64::MAX 的历史日志必须返回可读错误而非溢出 panic"
        );

        // 3. Direct append when next == u64::MAX must return Err without panicking or writing.
        let near_max = dir.join("near-max.jsonl");
        std::fs::write(
            &near_max,
            format!("{{\"kind\":\"boot\",\"seq\":{}}}\n", u64::MAX - 1),
        )
        .unwrap();
        let log_max = SessionLog::open(near_max.clone()).unwrap();
        assert_eq!(log_max.next_seq(), u64::MAX);
        assert!(
            log_max.append(json!({ "kind": "overflow" })).is_err(),
            "next == u64::MAX 时 append 必须返回错误而非溢出"
        );
        let rows_after = log_max.tail(10).unwrap();
        assert_eq!(rows_after.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r5_p2_session_log_file_handle_supports_set_len_rollback_on_windows() {
        let dir = std::env::temp_dir().join(format!(
            "cordis-r5-session-log-set-len-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollback.jsonl");

        let log = SessionLog::open(path.clone()).unwrap();
        assert_eq!(log.append(json!({ "kind": "boot" })).unwrap(), 1);
        let prev_len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(log.append(json!({ "kind": "step", "x": 1 })).unwrap(), 2);
        assert!(std::fs::metadata(&path).unwrap().len() > prev_len);

        // Directly verify that the open handle held by `SessionLog` has sufficient access rights
        // (`FILE_WRITE_DATA` on Windows) for `File::set_len(prev_len)` rollback on write error!
        {
            let state = log.state.lock().unwrap();
            state
                .file
                .set_len(prev_len)
                .expect("SessionLog handle must permit set_len rollback on all platforms");
            state.file.sync_all().unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), prev_len);
        let rows = log.tail(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["seq"], json!(1));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
