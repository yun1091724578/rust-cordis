//! 宿主提供给 T2 动态库插件的 xtern "C" 回调函数实现（从 lib.rs 拆分）。

use super::*;
// ------------------------------------------------------------ 宿主函数实现

pub(crate) unsafe fn blob_as_bytes<'a>(
    blob: Blob,
    field: &str,
) -> std::result::Result<&'a [u8], String> {
    if blob.len > isize::MAX as usize {
        return Err(format!("{field} 长度 {} 超出地址空间上限", blob.len));
    }
    if blob.ptr.is_null() && blob.len > 0 {
        return Err(format!("{field} 指针为空但长度非零 ({})", blob.len));
    }
    Ok(blob.as_bytes())
}

pub(crate) unsafe fn blob_to_string(
    blob: Blob,
    field: &str,
) -> std::result::Result<String, String> {
    let bytes = blob_as_bytes(blob, field)?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|err| format!("{field} 不是合法 UTF-8: {err}"))
}

pub(crate) unsafe fn blob_to_json(blob: Blob, field: &str) -> std::result::Result<Value, String> {
    let bytes = blob_as_bytes(blob, field)?;
    serde_json::from_slice(bytes).map_err(|err| format!("{field}不是合法 JSON: {err}"))
}

pub(crate) unsafe extern "C" fn host_ctx_get(handle: u64, name: Blob, out: *mut Out) -> i32 {
    let name = match blob_to_string(name, "get 的服务名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    // 统一解析（M1）：实例句柄与派生句柄同路——派生 ctx 读的是隔离域
    //（fiberless 的宽容读走内核的 unchecked root 语义，见 Store::resolve）。
    with_resolved(handle, out, |resolved| match resolved.ctx.get(&name) {
        None => Ok(json!({ "missing": true }).to_string()),
        Some(value) => encode_service(&name, value),
    })
}

/// 读一个**必需**服务（M2-1）：注入检查 + `committed` 快照，等价原生 `Ctx::require`。
///
/// 与 [`host_ctx_get`] 的两条差异：
/// 1. **要求声明 inject**：未声明就报 `cannot get property "…" without inject`——
///    宽容读把"没声明依赖"和"服务不存在"混成同一个 `missing`，那是最难查的一类静默错误；
/// 2. **读 committed 快照**：组件在**自己的拆除过程**中仍能读到正在离开的依赖，
///    而宽容读在提供者进入 UNLOADING 之后只能返回 `missing`。
pub(crate) unsafe extern "C" fn host_ctx_require(handle: u64, name: Blob, out: *mut Out) -> i32 {
    let name = match blob_to_string(name, "require 的服务名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    with_resolved_as(
        handle,
        out,
        "host_require",
        "resolve",
        |resolved| match resolved.ctx.require(&name) {
            Err(err) => Err(err.to_string()),
            Ok(value) => encode_service(&name, value),
        },
    )
}

/// 把一个服务值编码成 ABI 可见的 JSON。
///
/// `ctx_get` 与 `ctx_require` 共用它：**读取语义可以不同（宽容 vs 严格），
/// 但"什么对 ABI 可见"必须只有一条契约**——否则同一个服务会随读取入口
/// 时而可读、时而不可读。
///
/// M6：判据换成 [`Dyn::adapter`]——于是 `ctx.provide_iface`（T2 提供者）与
/// `#[cordis::service]` 的 typed 服务对 ABI 同样可见（后者也带 JSON 面），
/// 而且**能发现**（拿到方法清单与版本）就能**调用**（`ctx_call_iface` 走同一条路）。
fn encode_service(name: &str, value: Dyn) -> std::result::Result<String, String> {
    match value.downcast::<ExternValue>() {
        Some(exposed) => Ok(json!({ "value": exposed.0 }).to_string()),
        None => match value.adapter() {
            // 接口也能读：告知调用方"这是个带方法的服务"，并给出方法与版本。
            Some(iface) => Ok(json!({
                "value": { "iface": iface.service(), "version": iface.version(), "methods": iface.methods() }
            })
            .to_string()),
            None => Err(format!(
                "service \"{name}\" 对 ABI 不可见：提供方需要用 ExternValue（纯数据）\
                 或 ctx.provide_iface（带方法的服务）来注册"
            )),
        },
    }
}

/// 覆盖**自己提供的**服务值（M2-2）：对应 JS 的 `ctx.set(name, value)`。
///
/// 两条硬约束由内核的 `Store::set` 强制（与上游 `reflect.ts:164-175` 逐字一致）：
/// 没 provide 过 → `without provide`；实现者不是当前 fiber → `in multiple fibers`。
/// 换值**不通知依赖者**：依赖者的 epoch 只关心实现者身份，而 `set` 不改变身份。
pub(crate) unsafe extern "C" fn host_ctx_set(
    handle: u64,
    name: Blob,
    value_json: Blob,
    out: *mut Out,
) -> i32 {
    let name = match blob_to_string(name, "set 的服务名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let parsed: Value = match blob_to_json(value_json, "set 的载荷") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    with_instance(handle, out, |instance| {
        instance
            .ctx
            .set(&name, Dyn::new(ExternValue(parsed)))
            .map(|_| String::new())
            .map_err(|err| err.to_string())
    })
}

// ------------------------------------------------------------ ABI v2（M1）：五个尾追加能力

/// #12 `ctx_isolate`：派生 realm 子 context。
///
/// 子 context 继承调用方实例身份（回调 id 仍走原实例派发），但 `key` 进独立
/// 服务域。`label` 语义与 loader 的 `RowScopes` 一致：空 = 行私有域（`alloc`），
/// 非空 = 具名共享域（`named`）。
///
/// 返回 `{"handle": <u64>}`——bit63=1 的派生句柄。派生 ctx **没有 fiber**
/// （G18：子 context 带 fiber 是 M4 的事），所以 M1 的可用面是
/// `ctx_get`/`ctx_require` 的隔离域读；`provide`/`on` 请经 `ctx_use` 挂子组件。
pub(crate) unsafe extern "C" fn host_ctx_isolate(
    handle: u64,
    key: Blob,
    label: Blob,
    out: *mut Out,
) -> i32 {
    let key = match blob_to_string(key, "isolate 的 key") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let label = match blob_to_string(label, "isolate 的 label") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    with_resolved(handle, out, |resolved| {
        let realm = (!label.is_empty()).then(|| resolved.ctx.store.named_realm(&key, &label));
        let derived = resolved.ctx.isolate(&key, realm);
        let handle = register_derived(resolved.owner, derived, resolved.instance.clone(), None);
        Ok(json!({ "handle": handle }).to_string())
    })
}

/// #13 `ctx_intercept`：派生 intercept 子 context。
///
/// M1 只把 metadata 存进派生 ctx 的 intercept 链（祖先优先、后代覆盖），
/// 子 context（isolate/intercept/`ctx_use` 挂的子组件）经内核的
/// `resolve_intercept` 读到它；真正的消费点在 M3。
pub(crate) unsafe extern "C" fn host_ctx_intercept(
    handle: u64,
    key: Blob,
    metadata: Blob,
    out: *mut Out,
) -> i32 {
    let key = match blob_to_string(key, "intercept 的 key") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let metadata: Value = match blob_to_json(metadata, "intercept 的 metadata ") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    with_resolved(handle, out, |resolved| {
        let derived = resolved.ctx.intercept(&key, metadata);
        let handle = register_derived(resolved.owner, derived, resolved.instance.clone(), None);
        Ok(json!({ "handle": handle }).to_string())
    })
}

/// #14 `ctx_use`：挂一个子组件。
///
/// `url = "@native:<name>"` 走 [`NativeRegistry`]，其余交给进程级
/// [`PluginSource`]（[`set_plugin_source`]）。新 fiber 是**调用方 fiber 的
/// 一个 effect**（内核 `Fiber::new` 负责）：父 dispose ⇒ 子 dispose，
/// 宿主不需要为父子关系记账。返回子 fiber 的派生句柄。
pub(crate) unsafe extern "C" fn host_ctx_use(
    handle: u64,
    url: Blob,
    config: Blob,
    out: *mut Out,
) -> i32 {
    let url = match blob_to_string(url, "use 的 url") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let config: Value = match blob_to_json(config, "use 的 config ") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    with_resolved(handle, out, |resolved| {
        let plugin = resolve_component(&url)?;
        // 注意挂在**解析出的 ctx** 上：经派生句柄 use ⇒ 子组件落在隔离域内。
        let fiber = mount(&resolved.ctx, plugin, config, None);
        let handle = register_derived(
            resolved.owner,
            fiber.ctx(),
            resolved.instance.clone(),
            Some(Arc::downgrade(&fiber)),
        );
        Ok(json!({ "handle": handle }).to_string())
    })
}

/// #15 `ctx_dispose`：回收一个派生 ctx 句柄或子 fiber 句柄（`target = {"handle": …}`）。
///
/// 子 fiber 条目会真的 dispose 那个 fiber（loader 行因此走既有的自我卸载
/// 识别链）；isolate/intercept 条目只是撤销句柄（域不回收，与上游一致）。
pub(crate) unsafe extern "C" fn host_ctx_dispose(handle: u64, target: Blob, out: *mut Out) -> i32 {
    let parsed: Value = match blob_to_json(target, "dispose 的 target ") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let Some(raw) = parsed.get("handle").and_then(Value::as_u64) else {
        return fail(
            out,
            "dispose 的 target 需要 {\"handle\": <u64>} 形式的 JSON",
        );
    };
    if raw & DERIVED_HANDLE_TAG == 0 {
        return fail(
            out,
            "ctx_dispose 只回收派生句柄（bit63=1）；卸载自己请用 ctx_dispose_self",
        );
    }
    with_resolved(handle, out, move |resolved| {
        // 调用者只能回收**自己根实例**的派生句柄；派生句柄的 seq 可枚举，
        // 不加归属检查就能销毁别的插件的子 fiber。
        let mut table = derived_table().lock().unwrap();
        let entry = table
            .get(&raw)
            .ok_or_else(|| format!("派生句柄 {raw} 已失效（已回收，或根实例已注销排水）"))?;
        if entry.owner != resolved.owner {
            return Err(format!(
                "派生句柄 {raw} 不属于调用方根实例 {handle}，拒绝回收"
            ));
        }
        let live_fiber = entry.fiber.as_ref().and_then(Weak::upgrade);
        // 先在摘表前确认运行时：否则 remove 之后才发现无法 dispose，句柄就丢了。
        let runtime = if live_fiber.is_some() {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => Some(runtime),
                Err(_) => {
                    // upgrade 可能取得最后强引用，失败分支也必须先放锁再析构。
                    drop(table);
                    drop(live_fiber);
                    return Err("子 fiber 的卸载需要异步运行时（当前在同步上下文里）".into());
                }
            }
        } else {
            None
        };
        #[cfg(test)]
        derived_dispose_tests::before_remove(raw);
        let entry = table.remove(&raw).expect("validated under the same lock");
        drop(table);
        if let (Some(fiber), Some(runtime)) = (live_fiber, runtime) {
            runtime.spawn(async move { fiber.dispose().await });
        }
        drop(entry);
        Ok(String::new())
    })
}

/// #16 `ctx_dispose_self`：卸载调用者自己。
///
/// 拆的是调用插件自己的 fiber；loader 行的处置走既有机制——fiber dispose →
/// `internal/plugin` 观察者 → `mark_self_disposed`（摘装载表 + 树侧
/// `disabled: true` + 落盘）。**绝不静默**：不在 loader 行上的插件同样真拆
/// （fiber 及其全部 effect），只是没有行可标。
pub(crate) unsafe extern "C" fn host_ctx_dispose_self(handle: u64, out: *mut Out) -> i32 {
    with_resolved(handle, out, |resolved| {
        // 用**实例身份**的 fiber：经派生句柄自卸载，拆的也是拥有它的插件。
        let fiber = resolved
            .instance
            .ctx
            .fiber_opt()
            .ok_or("调用方实例没有 fiber，无法自我卸载")?;
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move { fiber.dispose().await });
                Ok(String::new())
            }
            Err(_) => Err("ctx_dispose_self 需要异步运行时（当前在同步上下文里）".into()),
        }
    })
}

/// `ctx_use` 的 url 解析：`@native:<name>` 与 `PluginSource` 两条路。
fn resolve_component(url: &str) -> std::result::Result<Arc<dyn Plugin>, String> {
    if let Some(name) = url.strip_prefix("@native:") {
        return native_registry()
            .and_then(|registry| registry.native(name))
            .ok_or_else(|| format!("@native:{name}：宿主没有注册这个原生组件（NativeRegistry）"));
    }
    let entry = patch::Entry {
        name: url.to_string(),
        ..patch::Entry::default()
    };
    let loaded = plugin_source()
        .load(&entry, &[], next_use_revision())
        .map_err(|err| format!("use \"{url}\" 失败: {err}"))?;
    Ok(loaded.plugin)
}

/// `ctx_use` 交给 `PluginSource` 的修订号（暂存副本文件名要用，进程内单调）。
pub(crate) fn next_use_revision() -> u64 {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    REVISION.fetch_add(1, Ordering::SeqCst) + 1
}

pub(crate) unsafe extern "C" fn host_ctx_provide(
    handle: u64,
    name: Blob,
    value_json: Blob,
    out: *mut Out,
) -> i32 {
    let name = match blob_to_string(name, "provide 的服务名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let parsed: Value = match blob_to_json(value_json, "provide 的载荷") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    with_instance(handle, out, |instance| {
        // 整个 provide 是调用者 fiber 的一个 effect：插件卸载时自动摘除，
        // 所以宿主这里不需要为它记账。
        instance
            .ctx
            .provide(&name, Dyn::new(ExternValue(parsed)))
            .map(|_| String::new())
            .map_err(|err| err.to_string())
    })
}

/// 一个由动态库插件提供的服务接口。
///
/// 它是内核 `ExternService` 的实现，但**完全不知道 ABI 细节**：
/// 方法调用被转成一次 `OP_CALLBACK`，带上是哪个服务、哪个方法、什么参数。
/// 于是"服务方法"成了契约，"函数指针"不是——这正是跨语言接口能稳定演进的原因。
pub(crate) struct PluginIface {
    service: String,
    version: u32,
    methods: Vec<String>,
    provider: Instance,
    /// 提供方的 ctx 句柄：回调时要用它把调用送回去。
    provider_handle: u64,
    /// 提供方注册的方法分发回调 id。
    dispatcher: u64,
}

impl ExternService for PluginIface {
    fn service(&self) -> &str {
        &self.service
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn methods(&self) -> Vec<String> {
        self.methods.clone()
    }

    fn has_method(&self, method: &str) -> bool {
        self.methods.iter().any(|known| known == method)
    }

    fn call(
        &self,
        caller_meta: Option<Config>,
        method: String,
        args: Config,
    ) -> BoxFuture<Result<Config>> {
        let provider = self.provider.clone();
        let provider_handle = self.provider_handle;
        let service = self.service.clone();
        let dispatcher = self.dispatcher;
        Box::pin(async move {
            let mut payload = json!({
                "kind": KIND_IFACE,
                "id": dispatcher,
                "event": service,
                "method": method,
                "args": [args],
            });
            // M3：调用点上的合并元数据 `d(k) ⊕ ι(k)`（右偏）作为**可选字段**下发。
            // 没有元数据时整个字段不出现——旧插件忽略未知字段，新插件读 `meta`。
            if let Some(meta) = caller_meta {
                payload["meta"] = meta;
            }
            // M2：接口方法可以挂起（返回 `STATUS_PENDING` 后由 `ctx_fulfill` 结算）；
            // M2b/M2c：接口调用不是 waterfall、不从调用方带令牌，但**父节点**照记
            //（它是被谁在回调里发起的，取消级联要追得到）。
            let parent = scope_top().map(|(_, id)| id);
            match unsafe {
                call_inbound(
                    &provider,
                    provider_handle,
                    OP_CALLBACK,
                    payload,
                    parent,
                    None,
                    None,
                    None,
                    true,
                    call_scope_top(),
                )
            } {
                Ok(Inbound::Ready(value)) => Ok(value),
                Ok(Inbound::Pending(pending)) => (*pending)
                    .settle("接口方法")
                    .await
                    .map_err(|failure| Error::from(failure.payload)),
                Err(message) => Err(Error::from(message.payload)),
            }
        })
    }

    /// 流式调用（ADR 0001 §5）：把 `sink` 挂到这次派发的记录上，**不等结算**就返回。
    ///
    /// - 插件直接给了一件值 ⇒ 它就是**长度 1 的流**（与内核默认实现同义；
    ///   插件若已经自己推完并收尾，这件返回值被忽略）；
    /// - 插件返回 `STATUS_PENDING` ⇒ 就这么返回：块稍后经 `ctx_stream_push` 到达，
    ///   收尾 / 失败 / 消费者离场都会由观察者把记录摘掉；
    /// - 插件同步失败 ⇒ 流以失败收场，同时把错误交给调用方。
    fn call_stream(
        &self,
        caller_meta: Option<Config>,
        method: String,
        args: Config,
        sink: StreamSink,
    ) -> BoxFuture<Result<()>> {
        let provider = self.provider.clone();
        let provider_handle = self.provider_handle;
        let service = self.service.clone();
        let dispatcher = self.dispatcher;
        Box::pin(async move {
            let mut payload = json!({
                "kind": KIND_IFACE,
                "id": dispatcher,
                "event": service,
                "method": method,
                "args": [args],
            });
            // M3：调用点上的合并元数据 `d(k) ⊕ ι(k)`（右偏）作为**可选字段**下发。
            if let Some(meta) = caller_meta {
                payload["meta"] = meta;
            }
            let parent = scope_top().map(|(_, id)| id);
            let record_sink = sink.clone();
            match unsafe {
                call_inbound(
                    &provider,
                    provider_handle,
                    OP_CALLBACK,
                    payload,
                    parent,
                    None,
                    None,
                    Some(record_sink),
                    false,
                    None,
                )
            } {
                Ok(Inbound::Ready(value)) => {
                    if !sink.is_finished() {
                        sink.close_with(value);
                    }
                    Ok(())
                }
                // 流式挂起：记录归流观察者所有（流收尾时摘除）。若配了派发看门狗，
                // 派生一个 settle 任务执行超时策略；超时会 revoke 记录并以错误收流。
                Ok(Inbound::Pending(pending)) => {
                    if provider.dispatch_policy.timeout_ms.is_some()
                        && tokio::runtime::Handle::try_current().is_ok()
                    {
                        spawn_stream_settle(&provider, pending).map_err(Error::from)?;
                    }
                    Ok(())
                }
                Err(message) => {
                    let payload = message.payload;
                    sink.fail_error(payload.clone());
                    Err(Error::from(payload))
                }
            }
        })
    }
}

#[allow(clippy::result_large_err)]
pub(crate) fn spawn_stream_settle(
    instance: &Instance,
    pending: Box<PendingContinuation>,
) -> std::result::Result<(), ErrorPayload> {
    let (cancel, exit) = {
        let table = instance.dispatches.lock().unwrap();
        if instance.closed.load(Ordering::SeqCst) {
            return Err(closed_admission_payload());
        }
        // 流可在 OP_CALLBACK 内正常收尾后仍返回 PENDING；此时无需看门狗。
        if !table.contains_key(&pending.dispatch_id) {
            return Ok(());
        }
        dispatch_task(instance, pending.dispatch_id)
    };
    let mut work = TaskWork {
        future: Box::pin(async move {
            let _ = (*pending).settle("流式调用").await;
        }),
        on_done: None::<fn(())>,
        _exit: exit,
    };
    tokio::runtime::Handle::current().spawn(async move {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {}
            _ = &mut work.future => {}
        }
        drop(work);
    });
    Ok(())
}

pub(crate) unsafe extern "C" fn host_ctx_provide_iface(
    handle: u64,
    name: Blob,
    version: u32,
    methods: Blob,
    dispatcher: u64,
    out: *mut Out,
) -> i32 {
    let service = match blob_to_string(name, "provide_iface 的服务名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let parsed: Value = match blob_to_json(methods, "方法清单") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let methods: Vec<String> = parsed
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    with_instance(handle, out, |instance| {
        let iface = PluginIface {
            service: service.clone(),
            version,
            methods,
            provider: instance.clone(),
            provider_handle: handle,
            dispatcher,
        };
        instance
            .ctx
            .provide_iface(&service, Arc::new(iface))
            .map(|_| String::new())
            .map_err(|err| err.to_string())
    })
}

#[allow(clippy::result_large_err)]
pub(crate) unsafe extern "C" fn host_ctx_call_iface(
    handle: u64,
    service: Blob,
    version: u32,
    method: Blob,
    args: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    let service = match blob_to_string(service, "call_iface 的服务名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let method = match blob_to_string(method, "call_iface 的方法名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let parsed: Value = match blob_to_json(args, "call_iface 的 args ") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    // M3：走 [`with_resolved_payload`] 而不是 [`with_instance`]——调用点上叠的
    // intercept 层挂在**派生 ctx** 上，用实例 ctx 调用等于把它丢掉（"use-site
    // 元数据"就无从谈起）。实例句柄照旧可用（派生表在实例表之后解析）；续延仍
    // 发回**根实例句柄**（派生句柄不是回调目标，与 `ctx_isolate` 的派生条目同一约定）。
    //
    // 用载荷版是为了让容量拒绝保留 `host_capacity` / `capacity` / `not_executed`。
    with_resolved_payload(handle, out, move |resolved| {
        let traced = resolved.instance.ctx.store.has_trace_sink();
        let trace_parent = traced.then(call_scope_top).flatten();
        let trace_correlation = traced
            .then(|| {
                resolved
                    .ctx
                    .correlation_id_arc()
                    .or_else(cordis_core::current_correlation_id)
            })
            .flatten();
        // 宿主方法白名单准入：在服务解析、派发槽位分配与提供者执行之前校验，
        // 身份直接来自宿主绑定的 `resolved.ctx`（不依赖 TraceSink / current_caller_uid）。
        if resolved.ctx.store.has_call_auth_hook() {
            let started = std::time::Instant::now();
            let caller = resolved.ctx.caller_identity();
            if let Err(err) = resolved
                .ctx
                .store
                .authorize_call(&caller, &service, &method)
            {
                if traced {
                    IfaceTrace {
                        store: resolved.instance.ctx.store.clone(),
                        name: format!("{service}.{method}"),
                        callee_plugin: service.clone(),
                        caller_uid: caller.fiber_uid,
                        parent_call_id: trace_parent.unwrap_or(0),
                        correlation_id: trace_correlation,
                        started,
                    }
                    .record("host_unauthorized");
                }
                return Err(ErrorPayload::new(err.to_string())
                    .with_code("host_unauthorized")
                    .with_category("authorization")
                    .with_certainty("not_executed"));
            }
        }
        let timeout_service = service.clone();
        let timeout_method = method.clone();
        let error_format = resolved.instance.error_format;
        let future = resolved.ctx.call_iface(&service, version, &method, parsed);
        let trace = traced.then(|| {
            Arc::new(IfaceTrace {
                store: resolved.instance.ctx.store.clone(),
                name: format!("{service}.{method}"),
                callee_plugin: service.clone(),
                caller_uid: resolved
                    .ctx
                    .fiber_opt()
                    .map(|fiber| fiber.uid())
                    .unwrap_or(cordis_core::UID_ROOT),
                parent_call_id: trace_parent.unwrap_or(0),
                correlation_id: trace_correlation.clone(),
                started: std::time::Instant::now(),
            })
        });
        let future = with_iface_timeout(
            future,
            timeout_service,
            timeout_method,
            resolved.instance.dispatch_policy,
            trace.clone(),
        );
        let consumer = resolved.owner;
        // 把这次 iface 调用登记成消费者实例的一条在飞记录：
        // 消费者卸载时 `apply_via` 的排水会把它撤掉，避免延迟 on_done 打进已释放的库。
        // 容量拒绝时直接返回：future 还没有被 poll，提供者代码不会执行。
        let iface_id = next_dispatch_id();
        let parent = scope_top().map(|(_, id)| id);
        let (cancel, done) = insert_dispatch_task(
            &resolved.instance,
            iface_id,
            DispatchRecord {
                sink: None,
                frame: None,
                parent,
                outbound: Some(Outbound {
                    owner: consumer,
                    continuation,
                    kind: ContinuationKind::Iface,
                    trace_parent,
                    trace_correlation,
                }),
                stream: None,
            },
        )
        .map_err(|(payload, _)| payload)?;
        let deliver: Arc<dyn Fn(Result<Config>) + Send + Sync> = Arc::new(move |result| {
            let payload = json!({
                "kind": KIND_IFACE,
                "id": continuation,
                "args": [match &result {
                    Ok(value) => json!({ "result": value }),
                    Err(err) => error_arg(error_format, &payload_of(err.as_ref())),
                }],
            })
            .to_string();
            deliver_outbound(consumer, iface_id, &payload);
        });
        let deferred = {
            let on_done = deliver.clone();
            let on_panic = deliver.clone();
            let panic_trace = trace.clone();
            complete_or_defer(
                future,
                move |result| on_done(result),
                move |payload| {
                    if let Some(trace) = panic_trace.as_ref() {
                        trace.record(&payload.code);
                    }
                    on_panic(Err(Box::new(CallFailure::from_payload(payload))))
                },
                (cancel, done),
                PanicScope {
                    scene: "iface",
                    handle: consumer,
                    dispatch_id: iface_id,
                },
            )
        };
        match deferred {
            // 内联完成：把结果直接带回，插件侧自己消费掉它的续延（零额外跨边界往返）。
            Ok(Some(result)) => {
                unregister_dispatch_node(consumer, iface_id);
                match result {
                    Ok(value) => Ok(json!({ "pending": false, "result": value }).to_string()),
                    // 并发收紧白名单时，内核 `call_iface` 复检仍可能返回 `MethodUnauthorized`：
                    // 统一收口为 `STATUS_ERR` + 结构化 `ErrorPayload`，避免协议形态漂移为 `STATUS_OK` 内联错。
                    Err(err)
                        if err
                            .downcast_ref::<cordis_core::MethodUnauthorized>()
                            .is_some() =>
                    {
                        Err(payload_of(err.as_ref()))
                    }
                    Err(err) => {
                        let mut response = json!({ "pending": false });
                        if let Value::Object(map) = &mut response {
                            if let Value::Object(error) =
                                error_arg(error_format, &payload_of(err.as_ref()))
                            {
                                map.extend(error);
                            }
                        }
                        Ok(response.to_string())
                    }
                }
            }
            // 未就绪：结果会通过 `OP_CALLBACK` 送到同一个续延 id 上。
            Ok(None) => Ok(json!({ "pending": true }).to_string()),
            Err(DeferFailure::NoRuntime(message)) => {
                if let Some(trace) = trace.as_ref() {
                    trace.record("host_call");
                }
                unregister_dispatch_node(consumer, iface_id);
                Err(ErrorPayload::new(message)
                    .with_code("host_call")
                    .with_category("protocol"))
            }
            // 首 poll panic：调用方同步拿到错误 ⇒ **先摘表再返回**，
            // 否则关闭路径会为同一次调用补发一条 abort（双交付）。
            Err(DeferFailure::Panic(payload)) => {
                if let Some(trace) = trace.as_ref() {
                    trace.record(&payload.code);
                }
                unregister_dispatch_node(consumer, iface_id);
                Err(payload)
            }
        }
    })
}

/// #17 `ctx_fulfill`（M2）：结算一个返回过 `STATUS_PENDING` 的入站续延。
///
/// 键是 `(实例, dispatch_id)`（D3）：同一 handler 的并发派发因此互不干扰；
/// 每 id **恰好结算一次**——第二次调用得到一条可读错误，绝不静默覆盖。
pub(crate) unsafe extern "C" fn host_ctx_fulfill(
    handle: u64,
    dispatch_id: u64,
    payload: Blob,
    out: *mut Out,
) -> i32 {
    let value: Value = if payload.len == 0 {
        Value::Null
    } else {
        match blob_to_json(payload, "fulfill 的载荷") {
            Ok(value) => value,
            Err(err) => return fail(out, &err),
        }
    };
    with_instance_as(handle, out, "host_fulfill", "lifecycle", move |instance| {
        // 取出即结算（remove-then-settle）：与"每 id 恰好一次"是同一件事。
        // 但**只允许摘有 sink 的入站记录**：outbound 记录（ctx_dispatch 的
        // dispatch_id）没有 sink，误摘会把它的正常交付静默取消。
        let record = {
            let mut table = instance.dispatches.lock().unwrap();
            match table.get(&dispatch_id) {
                Some(record) if record.sink.is_some() => {}
                Some(_) => {
                    return Err(format!(
                        "dispatch {dispatch_id} 不是可 fulfill 的入站续延（它是出站派发记录）"
                    ))
                }
                None => {
                    return Err(format!(
                        "dispatch {dispatch_id} 不存在或已结算（每次挂起只结算一次）"
                    ))
                }
            }
            table.remove(&dispatch_id).expect("checked above")
        };
        // 流（ADR 0001 §5）：`fulfill` 就等于"补上最后一件并正常收尾"——
        // 于是"值路径"与"流路径"在提供者看来是同一件事（长度 1 的流的推广）。
        if let Some(sink) = &record.stream {
            if !sink.is_finished() {
                sink.close_with(value.clone());
            }
        }
        if let Some(tx) = record.sink {
            let _ = tx.send(Ok(value));
        }
        Ok(String::new())
    })
}

/// #19 `ctx_abort`（M2c）：取消一次在飞派发——该派发**及其后代**全部以
/// 「已取消」结算（挂起续延收错误、出站续延收错误、waterfall 帧被丢）。
///
/// 父链来自派发记录的 `parent`（由宿主的**派发作用域**记录，插件不需要传）。
/// 级联在**同一实例**的记录表内递归（骨架范围；跨实例的级联见 §5 风险）。
pub(crate) unsafe extern "C" fn host_ctx_abort(
    handle: u64,
    dispatch_id: u64,
    out: *mut Out,
) -> i32 {
    with_instance_as(handle, out, "host_abort", "lifecycle", move |instance| {
        // 收集受害者（含自身）：先摘表，再结算——"remove-then-settle"。
        // 与取消/超时共用同一棵树的排水实现，保证级联语义只有一份。
        let payload = ErrorPayload::new("已取消：调用方撤销了这次派发")
            .with_code("host_cancel")
            .with_category("cancel")
            .with_certainty("unknown");
        drain_dispatch_tree(instance, dispatch_id, &payload)
            .map(|_| String::new())
            .ok_or_else(|| format!("dispatch {dispatch_id} 不存在或已结束（无可取消的在飞派发）"))
    })
}

/// 把一个失败原因送给**出站**续延（插件在等它的结果）。
///
/// ADR 0004 A4：原因通过 `args[0].error` 投递；能力位为 1 时带结构化兄弟字段。
/// `kind` 必须与发起时登记的类型一致——iface 调用方只认 `kind:"iface"`。
fn abort_payload(outbound: &Outbound, error_format: u32, error: &ErrorPayload) -> String {
    json!({
        "kind": outbound.kind.wire_name(),
        "id": outbound.continuation,
        "args": [error_arg(error_format, error)],
    })
    .to_string()
}

/// 向已认领的出站续延交付取消：即使 `closed` 已跨入，也必须先于 `OP_DISPOSE`。
/// 调用方必须持有 FFI 准入成功的 ActiveCall 或关闭 lease，并在锁外调用。
pub(crate) fn deliver_abort_direct(instance: &Instance, outbound: Outbound, error: &ErrorPayload) {
    let payload = abort_payload(&outbound, instance.error_format, error);
    if let Err(message) = unsafe {
        call_plugin(
            instance.call,
            outbound.owner,
            OP_CALLBACK,
            &payload,
            instance.error_format,
        )
    } {
        eprintln!("[cordis] 卸载期取消交付失败: {message}");
    }
}

/// #18 `ctx_next`（M2b）：续跑 waterfall 的**剩余链**，完成后以
/// `{kind:"next", id:on_done, args:[{"result": …}]}` 回调插件。
///
/// 剩余链先**同步试跑**（D6 的同步快速路径）：就地完成时就地投递 `on_done`
/// （于是"回调内 next、回调内结算"一次往返搞定）；未就绪则交给运行时。
/// 帧**取出即消费**：第二次 `ctx_next` 是可读错误。
#[allow(clippy::result_large_err)]
pub(crate) unsafe extern "C" fn host_ctx_next(
    handle: u64,
    dispatch_id: u64,
    args: Blob,
    on_done: u64,
    out: *mut Out,
) -> i32 {
    let parsed: Value = match blob_to_json(args, "next 的 args ") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let new_args = json_to_args(
        &parsed
            .get("args")
            .cloned()
            .unwrap_or_else(|| parsed.clone()),
    );
    with_instance_payload(handle, out, move |instance| {
        let next_error = |message| {
            ErrorPayload::new(message)
                .with_code("host_next")
                .with_category("lifecycle")
        };
        let error_format = instance.error_format;
        let delivered = Arc::new(AtomicBool::new(false));
        let cancel_outbound = Outbound {
            owner: handle,
            continuation: on_done,
            kind: ContinuationKind::Next,
            trace_parent: None,
            trace_correlation: None,
        };
        let frame = {
            let mut table = instance.dispatches.lock().unwrap();
            if instance.closed.load(Ordering::SeqCst) {
                return Err(closed_admission_payload());
            }
            match table.get_mut(&dispatch_id) {
                Some(record) => record.frame.take().map(|frame| {
                    (
                        frame,
                        dispatch_task_with_cancel(
                            instance,
                            dispatch_id,
                            Some((cancel_outbound, delivered.clone())),
                        ),
                    )
                }),
                None => None,
            }
        };
        let Some((next, (cancel, done))) = frame else {
            return Err(next_error(format!(
                "dispatch {dispatch_id} 没有可续跑的 waterfall 帧（不是 waterfall 派发，或已经 next 过）"
            )));
        };
        let future: BoxFuture<Ret> = Box::pin(next.call(new_args));
        // 剩余链的 future 打上作用域：它交付给插件的回调能记下父节点。
        let scoped: BoxFuture<Ret> = Box::pin(Scoped {
            inner: future,
            scope: (handle, dispatch_id),
        });
        let scoped = with_next_timeout(scoped, dispatch_id, instance.dispatch_policy);
        let deliver_flag = delivered.clone();
        let deliver: Arc<dyn Fn(std::result::Result<Ret, ErrorPayload>) + Send + Sync> =
            Arc::new(move |result| {
                deliver_next(
                    handle,
                    dispatch_id,
                    on_done,
                    error_format,
                    &deliver_flag,
                    result,
                );
            });
        let deferred = {
            let deliver = deliver.clone();
            let deliver_panic = deliver.clone();
            complete_or_defer(
                scoped,
                move |result| deliver(result),
                move |payload| deliver_panic(Err(payload)),
                (cancel, done),
                PanicScope {
                    scene: "next",
                    handle,
                    dispatch_id,
                },
            )
        };
        match deferred {
            Ok(Some(result)) => deliver(result),
            Ok(None) => {}
            Err(DeferFailure::NoRuntime(message)) => {
                // 返回 Err 时 SDK 会立刻 `forget(cb)`，置位避免并发取消再投递一次。
                delivered.store(true, Ordering::SeqCst);
                return Err(next_error(message));
            }
            // 首 poll panic：**不摘入站记录**（它由事件派发/取消路径收尾），只返回错误。
            Err(DeferFailure::Panic(payload)) => {
                delivered.store(true, Ordering::SeqCst);
                return Err(payload);
            }
        }
        Ok(String::new())
    })
}

/// 把下游结果投递给插件的 `on_done` 续延（`kind:"next"`）。
///
/// 与 [`deliver_outbound`] 同口径：`closed` 检查、`delivered` 终态认领与 `ActiveCall`
/// 准入在同一把 `dispatches` 锁内原子完成——若关闭已经先赢，不置位 `delivered`，
/// 留给卸载收尾经 `cancel_outbound` 投递 `host_lifecycle`（恰好一次，绝不丢失）。
fn deliver_next(
    handle: u64,
    dispatch_id: u64,
    on_done: u64,
    error_format: u32,
    delivered: &AtomicBool,
    result: std::result::Result<Ret, ErrorPayload>,
) {
    let Some(instance) = instances().read().unwrap().get(handle) else {
        return;
    };
    let _active = {
        let _table = instance.dispatches.lock().unwrap();
        if instance.closed.load(Ordering::SeqCst) {
            return;
        }
        let Ok(lease) = call_lease(&instance) else {
            return;
        };
        if delivered.swap(true, Ordering::SeqCst) {
            return;
        }
        *instance.lifecycle.lock().unwrap() += 1;
        ActiveCall {
            active: instance.lifecycle.clone(),
            changed: instance.calls_changed.clone(),
            lease: Ok(lease),
        }
    };
    let args = match result {
        Ok(ret) => json!({ "result": ret_to_json(ret) }),
        Err(err) => error_arg(error_format, &err),
    };
    let payload = json!({
        "kind": KIND_NEXT,
        "id": on_done,
        "args": [args],
    })
    .to_string();
    // 交付期间让"这次派发"留在作用域里：插件在 on_done 里再发派发时父节点仍是它。
    let _scope = scope_push(handle, dispatch_id);
    if let Err(message) = unsafe {
        call_plugin(
            instance.call,
            handle,
            OP_CALLBACK,
            &payload,
            instance.error_format,
        )
    } {
        eprintln!("[cordis] waterfall 的 on_done 交付失败: {message}");
    }
}

pub(crate) unsafe extern "C" fn host_ctx_on(
    handle: u64,
    event: Blob,
    cb: u64,
    out: *mut Out,
) -> i32 {
    let event = match blob_to_string(event, "on 的事件名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    with_instance(handle, out, |instance| {
        let ctx = instance.ctx.clone();
        let plugin_handle = handle;
        let event_name = event.clone();
        // 只让**监听器闭包**持有库强引用：钩子随 fiber 卸载被摘除，
        // 快照派发结束后强引用释放，动态库就能正常 FreeLibrary。
        let keepalive = instance.keepalive.as_ref().and_then(|weak| weak.upgrade());
        // M2/M2b：宿主监听器在**调用点**向插件发起入站回调（先登记 dispatch_id、再调用）；
        // waterfall 派发同时把**剩余链**（`call.next`）登记成帧——插件可以在回调内部
        // 调 `ctx_next` 用它；不调就永远不跑下游（惰性，M2b）。
        ctx.on(&event, move |call_args| {
            // 即使这次派发晚于实例注销到达，只要快照还在，库就保持加载；
            // 句柄校验（call_inbound）会拒绝真正进入插件代码。
            let _keepalive = keepalive.clone();
            let call_ctx = call_args.ctx.clone();
            let waterfall = call_args.next.is_linked();
            // 父节点：正在被交付的那次出站派发（作用域栈顶；不在派发里时为 None）。
            let parent = scope_top().map(|(_, id)| id);
            let frame = if waterfall {
                Some(call_args.next)
            } else {
                None
            };
            let cancel = call_args.cancel.clone();
            let next_args = call_args.args.clone();
            let mut payload = json!({
                "kind": KIND_EVENT,
                "id": cb,
                "event": event_name,
                "args": args_to_json(&call_args.args),
            });
            if waterfall {
                payload["mode"] = json!("waterfall");
            }
            // **不捕获快照里的 Instance**：派发可能晚于插件注销到达。
            // 每次调用重新取表项；取不到就把它当作"没有意见"，绝不碰裸 CallFn。
            // 注意先把实例取到局部变量再调用：`match instances().lock()...` 的
            // 临时 guard 会活到整个 match 结束，`call_inbound` 内部再取同一把锁就死锁。
            let live = instances().read().unwrap().get(plugin_handle);
            let result = match live {
                Some(instance) => unsafe {
                    call_inbound(
                        &instance,
                        plugin_handle,
                        OP_CALLBACK,
                        payload,
                        parent,
                        cancel,
                        frame,
                        None,
                        false,
                        None,
                    )
                },
                None => Err(CallFailure::new(
                    format!("ctx 句柄 {plugin_handle} 已失效（插件已卸载，或句柄来自上一代实例）"),
                    "host_lifecycle",
                    "lifecycle",
                    "unknown",
                )
                .with_frame(frame)),
            };
            Box::pin(async move {
                let settled = match result {
                    Ok(Inbound::Ready(value)) => Ok(value),
                    Ok(Inbound::Pending(pending)) => (*pending).settle("事件监听器").await,
                    Err(message) => Err(message),
                };
                match settled {
                    Ok(Value::Null) => None,
                    Ok(value) => Some(Dyn::new(ExternValue(value))),
                    Err(message) => {
                        // 非容量拒绝类失败留下一条错误日志；无论哪种失败（容量拒绝、句柄失效、
                        // 回调报错/panic/不支持/非合法 JSON、或挂起超时），只要 waterfall 剩余链
                        // 尚未被插件消费，都把剩余链继续跑完——这才是 waterfall 模式下的“没有意见”，
                        // 而不是返回 `None` 静默截断下游与 tail。
                        if message.payload.code != "host_capacity" {
                            call_ctx.log(Level::Error, &format!("事件监听器失败: {message}"));
                        }
                        let frame = message
                            .frame
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .take();
                        if let Some(frame) = frame {
                            return frame.call(next_args).await;
                        }
                        None
                    }
                }
            })
        });
        Ok(String::new())
    })
}

pub(crate) unsafe extern "C" fn host_ctx_effect(handle: u64, cb: u64, out: *mut Out) -> i32 {
    with_instance(handle, out, |instance| {
        let call = instance.call;
        let error_format = instance.error_format;
        let plugin_handle = handle;
        let fiber = instance.ctx.fiber();
        let fiber_weak = Arc::downgrade(&fiber);
        // M5-1：走 `try_effect`——**卸载期**（或已释放的 fiber）注册 effect 会回一条
        // 可读错误。走 `ctx.effect` 的话，这里会在 `extern "C"` 边界上 abort 整个进程，
        // 而"清理里想再挂一个收尾动作"是完全正常的插件写法。
        fiber
            .try_effect(move |_| {
                Disposable::new(move || {
                    let fiber_weak = fiber_weak.clone();
                    Box::pin(async move {
                        let instance = instances().read().unwrap().get(plugin_handle);
                        let Some(instance) = instance else {
                            return;
                        };
                        // 若 OP_APPLY 曾超时，统一按 LIFO 压入 deferred_cleanups，由卸载收尾阶段
                        // （严格在 wait_lifecycle_exclusive 之后、OP_DISPOSE 之前）按序补跑，
                        // 避免中途 OP_APPLY 恰好退出导致剩余回调抢跑破坏 LIFO 逆序。
                        if instance.apply_timed_out.load(Ordering::SeqCst) {
                            instance.deferred_cleanups.lock().unwrap().push(cb);
                            return;
                        }
                        let Some(active) = admit_call(&instance) else {
                            return;
                        };
                        let payload = json!({ "id": cb }).to_string();
                        let fiber = fiber_weak.upgrade();
                        let offload = fiber.as_ref().is_some_and(|f| f.should_offload_dispose());
                        let result = if offload {
                            match tokio::runtime::Handle::try_current() {
                                Ok(rt) => match rt
                                    .spawn_blocking(move || {
                                        let _active = active;
                                        unsafe {
                                            call_plugin(
                                                call,
                                                plugin_handle,
                                                cordis_abi::OP_CLEANUP,
                                                &payload,
                                                error_format,
                                            )
                                        }
                                    })
                                    .await
                                {
                                    Ok(res) => res,
                                    Err(err) => Err(if err.is_panic() {
                                        format!(
                                            "阻塞线程执行 OP_CLEANUP panic: {}",
                                            cordis_core::panic_message(&*err.into_panic())
                                        )
                                    } else {
                                        "OP_CLEANUP 阻塞任务被取消".to_string()
                                    }),
                                },
                                Err(_) => {
                                    let _active = active;
                                    unsafe {
                                        call_plugin(
                                            call,
                                            plugin_handle,
                                            cordis_abi::OP_CLEANUP,
                                            &payload,
                                            error_format,
                                        )
                                    }
                                }
                            }
                        } else {
                            let _active = active;
                            unsafe {
                                call_plugin(
                                    call,
                                    plugin_handle,
                                    cordis_abi::OP_CLEANUP,
                                    &payload,
                                    error_format,
                                )
                            }
                        };
                        if let Err(message) = result {
                            let detail = format!("插件清理回调失败: {message}");
                            eprintln!("[cordis] {detail}");
                            if let Some(fiber) = fiber {
                                fiber.record_cleanup_error(CleanupErrorKind::HostCleanup, detail);
                            }
                        }
                    })
                })
            })
            .map_err(|err| err.to_string())?;
        Ok(String::new())
    })
}

/// 按 mode 派发。`emit` 不取返回值，其余模式取"第一个 bail 值"。
fn dispatch_by_mode(ctx: &Ctx, mode: &str, event: &str, args: Vec<Dyn>) -> BoxFuture<Option<Dyn>> {
    match mode {
        "emit" => {
            let ctx = ctx.clone();
            let event = event.to_string();
            // 显式标注类型：`Box::pin(async {...})` 是具体类型，需要一次强制转换
            // 才能与其他分支的 `BoxFuture`（trait object）统一。
            let future: BoxFuture<Option<Dyn>> = Box::pin(async move {
                ctx.emit(&event, args).await;
                None
            });
            future
        }
        "serial" => ctx.serial(event, args),
        "parallel" => ctx.parallel(event, args),
        // 默认 bail：有返回值、遇第一个 bail 值即停。
        _ => ctx.bail(event, args),
    }
}

/// `ctx_emit` 与 `ctx_dispatch` 的共同实现：同步快速路径 + 异步兜底。
///
/// `continuation == 0` 表示调用方不要返回值（纯 `emit`）——这与 cordis 的
/// `emit` 语义一致：派发、丢弃返回值、不 await。
///
/// M2c：每次出站派发都在**发起实例**的记录表里登记一个节点（`parent` 来自
/// 派发作用域），响应带回它的 `dispatch_id`——`ctx_abort` 因此可以取消它，
/// 取消树也因此能把它的后代一网打尽。
#[allow(clippy::result_large_err)]
pub(crate) unsafe fn dispatch_impl(
    handle: u64,
    event: Blob,
    payload: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    let event = match blob_to_string(event, "dispatch 的事件名") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let parsed: Value = match blob_to_json(payload, "dispatch 的载荷") {
        Ok(value) => value,
        Err(err) => return fail(out, &err),
    };
    let args = json_to_args(&parsed.get("args").cloned().unwrap_or(Value::Null));
    let mode = parsed
        .get("mode")
        .and_then(|value| value.as_str())
        .unwrap_or("emit")
        .to_string();
    // 载荷版：容量拒绝必须保留 `host_capacity` / `capacity` / `not_executed`。
    with_instance_payload(handle, out, move |instance| {
        let ctx = instance.ctx.clone();
        let consumer = handle;
        let error_format = instance.error_format;
        let dispatch_id = next_dispatch_id();
        // 父节点 = 当前正在执行的那次派发（插件在自己的回调里发派发时，
        // 作用域栈顶就是它；不在派发里时为 None）。
        let parent = scope_top().map(|(_, id)| id);
        // 容量拒绝时直接返回：future 还没有被创建/驱动，订阅者不会收到派发。
        let (cancel, done) = insert_dispatch_task(
            instance,
            dispatch_id,
            DispatchRecord {
                sink: None,
                frame: None,
                parent,
                outbound: (continuation != 0).then_some(Outbound {
                    owner: handle,
                    continuation,
                    kind: ContinuationKind::Dispatch,
                    trace_parent: None,
                    trace_correlation: None,
                }),
                stream: None,
            },
        )
        .map_err(|(payload, _)| payload)?;
        let future = dispatch_by_mode(&ctx, &mode, &event, args);
        // 作用域：这次派发**交付给插件**的入站回调据此记下父节点（取消级联）。
        let future: BoxFuture<Ret> = Box::pin(Scoped {
            inner: future,
            scope: (handle, dispatch_id),
        });
        let future = with_dispatch_timeout(future, event, instance.dispatch_policy);
        if continuation == 0 {
            // 纯 emit：派发后立即返回。这不是妥协——cordis 的 `emit` 本来就不取返回值。
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                // 没有运行时就没有人驱动 future：不能假装 emit 成功。
                unregister_dispatch_node(handle, dispatch_id);
                return Err(ErrorPayload::new(
                    "emit 需要异步运行时（当前在同步上下文里，派发无法被驱动）",
                )
                .with_code("host_call")
                .with_category("protocol"));
            };
            let mut work = TaskWork {
                future,
                on_done: None::<fn(std::result::Result<Ret, ErrorPayload>)>,
                _exit: done,
            };
            runtime.spawn(async move {
                let panic = {
                    let mut caught = CatchUnwind::new(&mut work.future);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => None,
                        outcome = &mut caught => outcome.err(),
                    }
                };
                if let Some(payload) = panic {
                    // 纯 emit 没有续延可交付：只摘表 + 诊断，绝不为它伪造一条回调。
                    record_async_panic("emit", handle, dispatch_id, &*payload);
                }
                unregister_dispatch_node(handle, dispatch_id);
                drop(work);
            });
            return Ok(json!({ "dispatch_id": dispatch_id }).to_string());
        }
        // 交付续延前先摘节点：**节点还在**表示这次派发由正常路径收尾；
        // 节点不在 ⇒ 它已经被取消（取消路径已经交付过一次「已取消」，
        // 这里必须**抑制**自然交付——"恰好一次"）。
        let finished = {
            let handle = handle;
            Arc::new(move || unregister_dispatch_node(handle, dispatch_id))
        };
        let deliver = {
            Arc::new(move |result: std::result::Result<Ret, ErrorPayload>| {
                let args = match result {
                    Ok(ret) => json!({ "result": ret_to_json(ret) }),
                    Err(err) => error_arg(error_format, &err),
                };
                let payload = json!({
                    "kind": KIND_DISPATCH,
                    "id": continuation,
                    "args": [args],
                })
                .to_string();
                deliver_outbound(consumer, dispatch_id, &payload);
            })
        };
        let deferred = {
            let deliver = deliver.clone();
            let deliver_panic = deliver.clone();
            complete_or_defer(
                future,
                move |result| deliver(result),
                move |payload| deliver_panic(Err(payload)),
                (cancel, done),
                PanicScope {
                    scene: "dispatch",
                    handle,
                    dispatch_id,
                },
            )
        };
        Ok(match deferred {
            Ok(Some(Ok(ret))) => {
                // 内联完成：结果直接带回，插件自己消费续延（M2 起的既有语义）；
                // 节点顺手摘掉（不投递）。
                finished();
                json!({ "pending": false, "result": ret_to_json(ret), "dispatch_id": dispatch_id })
                    .to_string()
            }
            Ok(Some(Err(err))) => {
                finished();
                let mut payload = json!({
                    "pending": false,
                    "dispatch_id": dispatch_id,
                });
                if let Value::Object(map) = &mut payload {
                    if let Value::Object(error) = error_arg(error_format, &err) {
                        map.extend(error);
                    }
                }
                payload.to_string()
            }
            Ok(None) => json!({ "pending": true, "dispatch_id": dispatch_id }).to_string(),
            Err(DeferFailure::NoRuntime(message)) => {
                // 没有运行时，future 已被丢弃：必须把登记节点也摘掉，
                // 否则取消/排水会看到一条永远不会结算的记录。
                finished();
                return Err(ErrorPayload::new(message)
                    .with_code("host_call")
                    .with_category("protocol"));
            }
            // 首 poll panic：同样必须**先摘表再返回错误**（否则关闭时补发 abort）。
            Err(DeferFailure::Panic(payload)) => {
                finished();
                return Err(payload);
            }
        })
    })
}

/// 摘除一条**出站派发**的登记节点。返回 `true` 表示"它还在表里"——
/// 即这次派发仍由正常路径收尾；被取消（或已注销）时返回 `false`。
pub(crate) fn unregister_dispatch_node(handle: u64, dispatch_id: u64) -> bool {
    let instance = instances().read().unwrap().get(handle);
    let removed =
        instance.and_then(|instance| instance.dispatches.lock().unwrap().remove(&dispatch_id));
    removed.is_some()
}

// 普通出站的终态认领与调用准入必须在同一表锁内；关闭先赢则留给排水 abort。
// next 的入站记录可能早已终态，不能复用这个依赖记录存在的认领路径。
pub(crate) fn deliver_outbound(handle: u64, dispatch_id: u64, payload: &str) {
    let Some(instance) = instances().read().unwrap().get(handle) else {
        return;
    };
    let (removed, trace_parent, trace_correlation, _active) = {
        let mut table = instance.dispatches.lock().unwrap();
        if instance.closed.load(Ordering::SeqCst) || !table.contains_key(&dispatch_id) {
            return;
        }
        let Ok(lease) = call_lease(&instance) else {
            return;
        };
        let record = table.remove(&dispatch_id).unwrap();
        let trace_parent = record.outbound.as_ref().and_then(|out| out.trace_parent);
        let trace_correlation = record
            .outbound
            .as_ref()
            .and_then(|out| out.trace_correlation.clone());
        *instance.lifecycle.lock().unwrap() += 1;
        (
            record,
            trace_parent,
            trace_correlation,
            ActiveCall {
                active: instance.lifecycle.clone(),
                changed: instance.calls_changed.clone(),
                lease: Ok(lease),
            },
        )
    };
    drop(removed);
    #[cfg(test)]
    {
        let gate = instance.outbound_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.0.wait();
            gate.1.wait();
        }
    }
    // 续延在“调用方那一跳”的调用作用域与关联标识下执行（ADR 0010 / ADR 0011）：
    // PENDING 恢复后再发起的调用因此仍能挂回原来的父与所属会话。
    let _call_scope = trace_parent.map(call_scope_push);
    let _correlation = cordis_core::enter_correlation(trace_correlation);
    unsafe {
        if let Err(message) = call_plugin(
            instance.call,
            handle,
            OP_CALLBACK,
            payload,
            instance.error_format,
        ) {
            // 回调投递失败过去是静默的：插件等不到回调，宿主也没有任何线索。
            eprintln!(
                "[cordis] 出站续延交付失败（handle {handle}；dispatch {dispatch_id}）：{message}"
            );
        }
    }
}

fn ret_to_json(ret: Option<Dyn>) -> Value {
    ret.and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone())
        .unwrap_or(Value::Null)
}

pub(crate) unsafe extern "C" fn host_ctx_emit(
    handle: u64,
    event: Blob,
    payload: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    dispatch_impl(handle, event, payload, continuation, out)
}

pub(crate) unsafe extern "C" fn host_ctx_dispatch(
    handle: u64,
    event: Blob,
    payload: Blob,
    continuation: u64,
    out: *mut Out,
) -> i32 {
    dispatch_impl(handle, event, payload, continuation, out)
}

pub(crate) unsafe extern "C" fn host_log(handle: u64, level: u32, message: Blob) -> i32 {
    let Ok(bytes) = blob_as_bytes(message, "log 消息") else {
        return STATUS_ERR;
    };
    let message = String::from_utf8_lossy(bytes).into_owned();
    let ctx = instances()
        .read()
        .ok()
        .and_then(|table| table.get(handle))
        .map(|instance| instance.ctx);
    if let Some(ctx) = ctx {
        let level = match level {
            0 => Level::Debug,
            2 => Level::Warn,
            3 => Level::Error,
            _ => Level::Info,
        };
        // 与其它宿主函数一致：log sink 是宿主注入的第三方代码，panic 不能
        // 越过 `extern "C"` 边界 abort 整个进程。
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.log(level, &message)))
            .is_err()
        {
            eprintln!("[cordis] 宿主 log sink 在记录消息时 panic（已在 ABI 边界截获）");
            return STATUS_ERR;
        }
    }
    STATUS_OK
}
