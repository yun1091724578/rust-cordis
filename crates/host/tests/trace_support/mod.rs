//! 归因测试共用的脚手架：ABI 形状插件、trace 收集器、宿主函数表的小包装。
#![allow(dead_code)]

use cordis_abi::{Blob, Out, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Ctx, Inject, Plugin, TraceRecord, TraceSink};
use cordis_host::{apply_via_with_error_format, host_api, CallFn};
use std::sync::{Arc, Mutex};

pub struct Collector(pub Arc<Mutex<Vec<TraceRecord>>>);

impl TraceSink for Collector {
    fn record(&self, record: TraceRecord) {
        self.0.lock().unwrap().push(record);
    }
}

pub fn collect(app: &App) -> Arc<Mutex<Vec<TraceRecord>>> {
    let records = Arc::new(Mutex::new(Vec::new()));
    assert!(
        app.set_trace_sink(Arc::new(Collector(records.clone()))),
        "trace sink 只允许安装一次"
    );
    records
}

pub fn abi_plugin(name: &'static str, inject: Inject, call: CallFn) -> Arc<dyn Plugin> {
    let key = cordis_core::unique_key(name);
    Arc::new(cordis_core::FnPlugin {
        name,
        key,
        inject,
        validator: None,
        f: Arc::new(move |ctx: Ctx, config: Config| {
            apply_via_with_error_format(call, ctx, config, name, cordis_abi::ERROR_FORMAT_V1)
        }),
    })
}

pub fn scratch_out(buffer: &mut [u8]) -> Out {
    Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    }
}

/// 注册一个带方法的服务。`methods` 是 JSON 数组文本，`dispatcher` 是回调 id。
///
/// # Safety
/// 只能在 ABI 插件的回调里、用宿主发给它的实例句柄调用。
pub unsafe fn provide(
    handle: u64,
    service: &str,
    methods: &str,
    dispatcher: u64,
    out: *mut Out,
) -> i32 {
    let Some(provide) = host_api().ctx_provide_iface else {
        return STATUS_UNSUPPORTED;
    };
    let mut buffer = [0u8; 256];
    let mut slot = scratch_out(&mut buffer);
    let status = provide(
        handle,
        Blob::new(service.as_bytes()),
        1,
        Blob::new(methods.as_bytes()),
        dispatcher,
        &mut slot,
    );
    if status == cordis_abi::STATUS_OK {
        (*out).write(b"");
    } else {
        (*out).write(&buffer[..slot.len.min(buffer.len())]);
    }
    status
}

/// 调一次宿主 `ctx_call_iface`，返回 `(status, 回复文本)`。
///
/// # Safety
/// 只能在 ABI 插件的回调里、用宿主发给它的实例句柄调用。
pub unsafe fn call_iface(
    handle: u64,
    service: &str,
    method: &str,
    continuation: u64,
) -> (i32, String) {
    let Some(call) = host_api().ctx_call_iface else {
        return (STATUS_UNSUPPORTED, "宿主不支持 ctx_call_iface".into());
    };
    let mut buffer = vec![0u8; 4096];
    let mut out = scratch_out(&mut buffer);
    let status = call(
        handle,
        Blob::new(service.as_bytes()),
        1,
        Blob::new(method.as_bytes()),
        Blob::new(b"[]"),
        continuation,
        &mut out,
    );
    (
        status,
        String::from_utf8_lossy(&buffer[..out.len.min(buffer.len())]).into_owned(),
    )
}
