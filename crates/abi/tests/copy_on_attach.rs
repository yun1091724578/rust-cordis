//! M0 红测试 2（G15）：**copy-on-attach** —— `attach` 时把宿主函数表按 `host.size`
//! 拷进插件自有的零初始化缓冲，此后一切读都走自有副本（结构上不可能越界读宿主内存）。
//!
//! 独立测试二进制（`tests/` 目录）：`HOST` 是进程级 `OnceLock`，本文件必须独占进程，
//! 才能与 `src/sdk.rs` 里的既有测试（也调 `attach`）互不干扰——D10 的隔离纪律。
//!
//! oracle 是**真截断**分配：只分配 `size` 字节的 `HostApi`，其后紧跟金丝雀字节。
//! 越界读在普通测试里"恰好没崩"，在这里会变成可断言的失败：
//! 毒化原宿主表之后，插件调到的必须仍是 attach 那一刻的快照。

use cordis_abi::sdk::{attach, attached_host};
use cordis_abi::{Blob, CtxGetFn, HostApi, Out, STATUS_OK};

/// 截断边界：只覆盖前 3 个字段（abi_version / size / ctx_get）。
const TRUNC: usize = std::mem::offset_of!(HostApi, ctx_provide);

unsafe extern "C" fn get_a(_: u64, _: Blob, out: *mut Out) -> i32 {
    (*out).write(br#"{"value":"A"}"#)
}

unsafe extern "C" fn get_b(_: u64, _: Blob, out: *mut Out) -> i32 {
    (*out).write(br#"{"value":"B"}"#)
}

#[test]
fn truncated_host_api_reads_only_inside_its_own_copy() {
    // 1) 真截断分配：先造一张全量表，再只把前 TRUNC 字节拷进 arena；
    //    arena 里 TRUNC 之后是金丝雀字节（0xA5）——任何越界写都会被看见。
    //    截断边界之外的字段值无关紧要（真 v1 宿主的分配里它们根本不存在）。
    let full = HostApi {
        abi_version: cordis_abi::ABI_VERSION,
        size: TRUNC as u32,
        ctx_get: Some(get_a),
        ..HostApi::default()
    };
    let mut arena = [0xA5u8; TRUNC + 16];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &full as *const HostApi as *const u8,
            arena.as_mut_ptr(),
            TRUNC,
        );
    }
    let canary_before: Vec<u8> = arena[TRUNC..].to_vec();
    let host = arena.as_ptr() as *const HostApi;

    // 2) attach：版本匹配 ⇒ 登记的必须是自己那份拷贝。
    unsafe { attach(host) };
    let api = attached_host().expect("abi_version 匹配时 attach 必须登记宿主表");

    // 3) 毒化原宿主表：把 size 之内唯一放得进的函数指针换成另一个合法函数
    //    （安全形态的毒化——不引入未定义行为，只让"读宿主内存"与"读快照"可区分）。
    unsafe {
        std::ptr::write_volatile(
            arena
                .as_mut_ptr()
                .add(std::mem::offset_of!(HostApi, ctx_get)) as *mut Option<CtxGetFn>,
            Some(get_b),
        );
    }

    // 4) 结构性断言：一切读都在插件自有副本之内。
    //    （fn 指针没有 Debug，比较转成 usize：同一 crate 内地址区分 A/B 足够。）
    assert!(
        api.ctx_get.map(|f| f as usize) == Some(get_a as CtxGetFn as usize),
        "插件读到的必须是 attach 那一刻的快照（A），而不是宿主表现在的值（B）：got {:#x}",
        api.ctx_get.map_or(0, |f| f as usize)
    );
    assert_eq!(api.size, TRUNC as u32, "副本带的是宿主声明过的 size");
    assert!(
        api.ctx_provide.is_none()
            && api.ctx_on.is_none()
            && api.ctx_effect.is_none()
            && api.ctx_emit.is_none()
            && api.log.is_none()
            && api.ctx_provide_iface.is_none()
            && api.ctx_call_iface.is_none()
            && api.ctx_dispatch.is_none()
            && api.ctx_require.is_none()
            && api.ctx_set.is_none()
            && api.ctx_isolate.is_none()
            && api.ctx_intercept.is_none()
            && api.ctx_use.is_none()
            && api.ctx_dispose.is_none()
            && api.ctx_dispose_self.is_none()
            && api.ctx_fulfill.is_none()
            && api.ctx_next.is_none()
            && api.ctx_abort.is_none()
            && api.ctx_stream_push.is_none()
            && api.ctx_stream_close.is_none()
            && api.ctx_stream_fail.is_none()
            && api.error_format == 0
            && api.reserved == 0,
        "size 之外的字段必须是零初始化的 None / 0（宿主不支持），绝不是越界读来的垃圾指针"
    );
    // size 比较保留作语义区分（`api!` 的探测谓词）：截断之外一律 None。
    assert!(unsafe {
        api.field_at::<Option<cordis_abi::CtxProvideFn>>(std::mem::offset_of!(HostApi, ctx_provide))
    }
    .is_none());

    // 5) 行为断言：拿副本里的指针真调一次 —— 调到的必须是 A。
    let get = match api.ctx_get {
        Some(get) => get,
        None => panic!("快照里的 ctx_get 必须可调用"),
    };
    let mut buffer = [0u8; 64];
    let mut out = Out {
        ptr: buffer.as_mut_ptr(),
        cap: buffer.len(),
        len: 0,
    };
    let status = unsafe { get(1, Blob::new(b"svc"), &mut out) };
    assert_eq!(status, STATUS_OK);
    let body = String::from_utf8_lossy(&buffer[..out.len]).into_owned();
    assert_eq!(
        body, r#"{"value":"A"}"#,
        "调用必须落在 attach 快照的函数上：{body}"
    );

    // 6) 金丝雀未变：没有任何越过 `size` 的写。
    assert_eq!(
        &arena[TRUNC..],
        canary_before.as_slice(),
        "截断边界之后的字节必须原样"
    );
}
