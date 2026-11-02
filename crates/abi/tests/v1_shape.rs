//! v1 形态固化（M0 第 6 项）：把 `HostApi` 的 11 个函数字段与 `PluginDesc`
//! 的字段序钉死成断言——任何漂移（改序、改名、往中间插字段）都在测试期爆掉，
//! 而不是在某个第三方插件莫名其妙拒载时才被发现。

use cordis_abi::{Blob, HostApi, PluginDesc, PLUGIN_DESC_PREFIX_SIZE};
use std::mem::{offset_of, size_of};

/// M0 红测试 3：前缀常量冻结。
///
/// `PLUGIN_DESC_PREFIX_SIZE` 是 v1/v2 兼容的**契约**（宿主门禁用它），一旦发布就
/// 永远不能变。这里用两种独立算法手算它：纯字段尺寸累加、`call` 偏移 + 函数指针宽。
#[test]
fn plugin_desc_prefix_size_is_frozen() {
    type CallFnShape = unsafe extern "C" fn(u64, u32, Blob, Blob, *mut cordis_abi::Out) -> i32;

    // 手算一：逐字段累加（u32 + u32 + 3×Blob + attach 指针 + call 指针）。
    let hand = 4 + 4 + 3 * size_of::<Blob>() + size_of::<CallFnShape>() + size_of::<CallFnShape>();
    assert_eq!(
        PLUGIN_DESC_PREFIX_SIZE, hand,
        "前缀常量必须等于手算的 v1 布局尺寸（x64：4+4+3×16+8+8）"
    );

    // 手算二：定义原文 —— `offset_of!(PluginDesc, call) + size_of::<CallFnShape>()`。
    assert_eq!(
        PLUGIN_DESC_PREFIX_SIZE,
        offset_of!(PluginDesc, call) + size_of::<CallFnShape>(),
        "前缀常量必须等于定义原文"
    );

    // 前缀正好是 v1 边界：`provision` 是前缀之后的第一个（也是唯一的）尾字段。
    assert_eq!(
        PLUGIN_DESC_PREFIX_SIZE,
        offset_of!(PluginDesc, provision),
        "前缀之后紧跟着 provision（往 call 之后追加永不移动它——这条性质现在被钉死）"
    );
}

/// `PluginDesc` 字段序冻结（M0 第 6 项）：`#[repr(C)]` 布局是公开契约。
#[test]
fn plugin_desc_field_order_is_frozen() {
    let layout: [(&str, usize); 10] = [
        ("abi_version", offset_of!(PluginDesc, abi_version)),
        ("size", offset_of!(PluginDesc, size)),
        ("id", offset_of!(PluginDesc, id)),
        ("inject", offset_of!(PluginDesc, inject)),
        ("schema", offset_of!(PluginDesc, schema)),
        ("attach", offset_of!(PluginDesc, attach)),
        ("call", offset_of!(PluginDesc, call)),
        ("provision", offset_of!(PluginDesc, provision)),
        ("error_format", offset_of!(PluginDesc, error_format)),
        ("tail_reserved", offset_of!(PluginDesc, tail_reserved)),
    ];
    // x64 手算值：4 字节对齐、指针/usize 各 8 字节、Blob = 16 字节。
    let expect = [0, 4, 8, 24, 40, 56, 64, 72, 88, 92];
    assert_eq!(layout.iter().map(|(_, at)| *at).collect::<Vec<_>>(), expect);

    // 严格递增（没有字段被重排/塞进缝隙）。
    for pair in layout.windows(2) {
        assert!(pair[0].1 < pair[1].1, "字段序必须严格递增：{layout:?}");
    }
    assert_eq!(
        offset_of!(PluginDesc, provision) + size_of::<Blob>(),
        offset_of!(PluginDesc, error_format),
        "provision 后只能尾追加 error_format"
    );
    assert_eq!(
        offset_of!(PluginDesc, error_format) + size_of::<u32>(),
        offset_of!(PluginDesc, tail_reserved),
        "tail_reserved 必须紧跟 error_format"
    );
    assert_eq!(
        offset_of!(PluginDesc, tail_reserved) + size_of::<u32>(),
        size_of::<PluginDesc>(),
        "tail_reserved 必须是尾字段（后续里程碑只许再尾追加）"
    );
}

/// `HostApi` 的 v1 清单冻结（M0 第 6 项）：11 个函数字段 + 2 个头部字段，
/// 顺序就是 v1 的兼容契约——v2 只能往尾部追加（附录 A）。
///
/// M1 更新：HostApi 已按附录 A 尾追加 #12–#16，所以这里钉的是
/// **前 11 字段序 + v1 前缀尺寸**（`ctx_set` 之前含它的总宽）——
/// v1 前缀是"旧宿主 `size` 可能报告的最大值"，v2 字段永远排在其后。
#[test]
fn host_api_v1_function_fields_are_frozen() {
    // 头部：abi_version 在 0、size 在 4（所有代际都读得到）。
    assert_eq!(offset_of!(HostApi, abi_version), 0);
    assert_eq!(offset_of!(HostApi, size), 4);

    // 11 个函数字段，v1 冻结序。
    let functions: [(&str, usize); 11] = [
        ("ctx_get", offset_of!(HostApi, ctx_get)),
        ("ctx_provide", offset_of!(HostApi, ctx_provide)),
        ("ctx_on", offset_of!(HostApi, ctx_on)),
        ("ctx_effect", offset_of!(HostApi, ctx_effect)),
        ("ctx_emit", offset_of!(HostApi, ctx_emit)),
        ("log", offset_of!(HostApi, log)),
        ("ctx_provide_iface", offset_of!(HostApi, ctx_provide_iface)),
        ("ctx_call_iface", offset_of!(HostApi, ctx_call_iface)),
        ("ctx_dispatch", offset_of!(HostApi, ctx_dispatch)),
        ("ctx_require", offset_of!(HostApi, ctx_require)),
        ("ctx_set", offset_of!(HostApi, ctx_set)),
    ];
    assert_eq!(functions.len(), 11, "v1 的函数面就是 11 个（§0.1）");
    for pair in functions.windows(2) {
        assert!(
            pair[0].1 < pair[1].1,
            "11 个函数字段必须保持 v1 冻结序：{functions:?}"
        );
    }

    // v1 前缀尺寸：`ctx_set`（含）收尾。v1 宿主的 `size` 最多到这里，
    // v2 的尾追加字段必须严格排在前缀之后（copy-on-attach 的截断语义依赖它）。
    let v1_prefix = offset_of!(HostApi, ctx_set) + size_of::<Option<cordis_abi::CtxSetFn>>();
    assert_eq!(
        offset_of!(HostApi, ctx_isolate),
        v1_prefix,
        "ctx_isolate 必须紧跟 v1 前缀（v2 只许尾追加）"
    );

    // v2 尾追加序（附录 A #12–#22）：isolate → intercept → use → dispose → dispose_self →
    // fulfill → next → abort → stream_push → stream_close → stream_fail → error_format → reserved。
    // （M2b/M2c 追加 `ctx_next`/`ctx_abort`，流（ADR 0001 §5）追加三个 `ctx_stream_*`，
    //   ADR 0004 追加结构化错误能力位与尾部保留字——v1/v2 前缀部分逐字未动。）
    let v2_tail: [(&str, usize); 13] = [
        ("ctx_isolate", offset_of!(HostApi, ctx_isolate)),
        ("ctx_intercept", offset_of!(HostApi, ctx_intercept)),
        ("ctx_use", offset_of!(HostApi, ctx_use)),
        ("ctx_dispose", offset_of!(HostApi, ctx_dispose)),
        ("ctx_dispose_self", offset_of!(HostApi, ctx_dispose_self)),
        ("ctx_fulfill", offset_of!(HostApi, ctx_fulfill)),
        ("ctx_next", offset_of!(HostApi, ctx_next)),
        ("ctx_abort", offset_of!(HostApi, ctx_abort)),
        ("ctx_stream_push", offset_of!(HostApi, ctx_stream_push)),
        ("ctx_stream_close", offset_of!(HostApi, ctx_stream_close)),
        ("ctx_stream_fail", offset_of!(HostApi, ctx_stream_fail)),
        ("error_format", offset_of!(HostApi, error_format)),
        ("reserved", offset_of!(HostApi, reserved)),
    ];
    for pair in v2_tail.windows(2) {
        assert!(
            pair[0].1 < pair[1].1,
            "v2 尾字段必须保持附录 A 序：{v2_tail:?}"
        );
    }
    assert_eq!(
        v2_tail[10].1 + size_of::<Option<cordis_abi::CtxStreamFailFn>>(),
        v2_tail[11].1,
        "error_format 必须紧跟流字段"
    );
    assert_eq!(
        v2_tail[11].1 + size_of::<u32>(),
        v2_tail[12].1,
        "reserved 必须紧跟 error_format"
    );
    assert_eq!(
        v2_tail[12].1 + size_of::<u32>(),
        size_of::<HostApi>(),
        "reserved 必须是尾字段（后续里程碑只许再尾追加）"
    );
}
