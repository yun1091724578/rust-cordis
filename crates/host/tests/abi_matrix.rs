//! A3 的宿主侧半边（M0）：**v1 形态**的插件在 v2 宿主上可加载、可工作。
//!
//! 全部无 DLL（D9）：门禁是纯函数（[`cordis_host::desc_gate`]，D8 的 seam），
//! 装载走 [`cordis_host::apply_via`] + 手写 `CallFn` —— 这正是 `host::CallFn`
//! 文档里"不经动态库的 ABI 形状插件"夹具。真 DLL 的方向留给 demo/spike。

use cordis_abi::{Blob, PluginDesc, ABI_VERSION, OP_APPLY, OP_DISPOSE, STATUS_UNSUPPORTED};
use cordis_core::{App, Config, Ctx, ExternValue, FiberState, Inject, Plugin};
use cordis_host::{apply_via, desc_gate};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// M0 红测试 1（G14）：门禁改为**固定前缀**。
///
/// 旧行为（`size >= size_of::<PluginDesc>()`）会把"尾追加字段"变成破坏性变更——
/// 一旦宿主侧 `PluginDesc` 追加 `provision`，所有现存 v1 插件（`size` 只到 v1 边界）
/// 全部被拒载。新门禁只要求 `size >= PLUGIN_DESC_PREFIX_SIZE`（冻结前缀）。
#[test]
fn desc_gate_rejects_a_prefix_only_desc_under_the_old_rule_and_accepts_it_under_the_new() {
    let prefix = cordis_abi::PLUGIN_DESC_PREFIX_SIZE;

    // 输入正是旧规则会拒掉的那一种：desc 尺寸恰好等于前缀（v1 边界），
    // 而宿主结构体已经比它大（v2 追加了 provision 尾字段）。
    let old_rule_accepts = prefix >= std::mem::size_of::<PluginDesc>();
    assert!(
        !old_rule_accepts,
        "fixture 前提：这个输入在旧规则（size >= sizeof(PluginDesc)）下必须是被拒的"
    );

    // 新门禁：接受。
    desc_gate(ABI_VERSION, prefix, prefix)
        .expect("前缀恰好达标的 desc 必须被接受（这正是 G14 要修的）");
    // 更大的（v2）desc 也接受：门禁只设下限。
    desc_gate(ABI_VERSION, std::mem::size_of::<PluginDesc>(), prefix)
        .expect("完整 v2 desc 也必须被接受");
    // 比前缀还小：拒绝（宿主连冻结前缀内的字段都读不全）。
    assert!(
        desc_gate(ABI_VERSION, prefix - 1, prefix).is_err(),
        "连冻结前缀都盖不满的 desc 必须被拒绝"
    );

    // 主版本不匹配：拒绝，且错误信息可读（不是一句裸数字）。
    let error = desc_gate(ABI_VERSION + 1, std::mem::size_of::<PluginDesc>(), prefix)
        .expect_err("ABI 主版本不匹配必须被拒绝");
    let text = error.to_string();
    assert!(
        text.contains("2") && text.contains("1"),
        "错误信息应包含双方版本号：{text}"
    );
}

// ------------------------------------------------------------------ 手写 v1 插件夹具

/// v1 插件收到的宿主表（v1 插件不认识尾追加函数，但 `attach` 总会被调用）。
static V1_HOST: OnceLock<&'static cordis_abi::HostApi> = OnceLock::new();

static V1_DISPOSED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn v1_attach(host: *const cordis_abi::HostApi) {
    if !host.is_null() {
        let _ = V1_HOST.set(&*host);
    }
}

/// 手写 `call`（v1 插件没有 SDK 帮忙）：OP_APPLY 里经**真宿主函数表**提供一个服务，
/// 未知 op 原样 `STATUS_UNSUPPORTED`——v1 插件对新宿主的新 op 只能这样答。
unsafe extern "C" fn v1_call(
    ctx: u64,
    op: u32,
    _a: cordis_abi::Blob,
    _b: cordis_abi::Blob,
    out: *mut cordis_abi::Out,
) -> i32 {
    match op {
        OP_APPLY => {
            let Some(host) = V1_HOST.get().copied() else {
                (*out).write(b"v1 plugin: attach was never called");
                return cordis_abi::STATUS_ERR;
            };
            let Some(provide) = host.ctx_provide else {
                (*out).write(b"v1 plugin: host has no ctx_provide");
                return cordis_abi::STATUS_ERR;
            };
            let mut buffer = [0u8; 64];
            let mut provide_out = cordis_abi::Out {
                ptr: buffer.as_mut_ptr(),
                cap: buffer.len(),
                len: 0,
            };
            provide(
                ctx,
                cordis_abi::Blob::new(b"v1-svc"),
                cordis_abi::Blob::new(b"{\"from\":\"v1\"}"),
                &mut provide_out,
            )
        }
        OP_DISPOSE => {
            V1_DISPOSED.store(true, Ordering::SeqCst);
            (*out).write(b"")
        }
        _ => STATUS_UNSUPPORTED,
    }
}

/// 手工构造 desc：`size` 可调——v1 形态只填到前缀为止，v2 形态填全尺寸。
fn desc_with(size: u32) -> &'static PluginDesc {
    Box::leak(Box::new(PluginDesc {
        abi_version: ABI_VERSION,
        size,
        id: cordis_abi::Blob::new(b"v1-handwritten"),
        inject: cordis_abi::Blob::new(b"[]"),
        schema: cordis_abi::Blob::EMPTY,
        attach: v1_attach,
        call: v1_call,
        provision: cordis_abi::Blob::new(br#"["tools"]"#),
        error_format: 0,
        tail_reserved: 0,
    }))
}

/// v1 夹具（M0 第 6 项）：手写一个 **v1 形态**的插件——desc 的 `size` 只到 v1 边界
/// （不含 `provision` 尾字段），`call` 完全手写，经宿主函数表的真函数工作。
/// 它必须被 v2 宿主加载（门禁放行 + `field_at` 不越界）且真的能干活（A3 前半）。
#[tokio::test]
async fn a_handwritten_v1_shaped_plugin_loads_and_works_on_a_v2_host() {
    let prefix = cordis_abi::PLUGIN_DESC_PREFIX_SIZE;

    // ① 可加载：v2 宿主的门禁放行 v1 尺寸的 desc（新规则）。
    let desc = desc_with(prefix as u32);
    desc_gate(desc.abi_version, desc.size as usize, prefix)
        .expect("v1 形态的 desc 必须能过 v2 宿主的门禁");

    // ② 读 provision 的安全形态：`size` 不足 ⇒ `None`（未声明），而不是越界读。
    assert!(
        unsafe {
            PluginDesc::field_at::<cordis_abi::Blob>(
                desc,
                std::mem::offset_of!(PluginDesc, provision),
            )
        }
        .is_none(),
        "size 只到 v1 边界时，provision 必须读作「未声明」"
    );
    // 对照：size 足够时同一个入口读得到**非空**尾字段（这样前缀案例的 None
    // 才不是“尾字段本来就是空”的空断言）。
    let full = desc_with(std::mem::size_of::<PluginDesc>() as u32);
    let provision = unsafe {
        PluginDesc::field_at::<cordis_abi::Blob>(full, std::mem::offset_of!(PluginDesc, provision))
    }
    .expect("size 足够时 provision 必须存在");
    // v1 形态（size 只到前缀）没有能力位；超过 `size` 的尾字段不能被读出来。
    assert_eq!(
        unsafe { PluginDesc::error_format_at(desc) },
        0,
        "v1 形态必须读作没有结构化错误能力"
    );
    // SAFETY：Blob 指向 `desc_with` 里的静态字节串，生命周期是整个测试。
    assert_eq!(
        unsafe { provision.as_str() },
        r#"["tools"]"#,
        "size 足够时必须读出非空 provision"
    );

    // ③ 可工作：先把**真宿主函数表**交给插件——这是 open() 里
    // `desc.attach(&HOST_API)` 的等价物（apply_via 不经过 open()，手工补上）；
    // 然后 apply_via 真装载，插件经宿主函数表提供服务，卸载通知也送达。
    unsafe { (desc.attach)(cordis_host::host_api()) };
    let app = App::new();
    let plugin: Arc<dyn Plugin> = Arc::new(cordis_core::FnPlugin {
        name: "v1-handwritten",
        key: cordis_core::unique_key("v1-handwritten"),
        inject: Inject::default(),
        validator: None,
        f: Arc::new(|ctx: Ctx, config: Config| apply_via(v1_call, ctx, config, "v1-handwritten")),
    });
    let fiber = app.root.plugin(plugin, Config::Null);
    fiber.settle().await;
    assert_eq!(
        fiber.state(),
        FiberState::Active,
        "v1 插件在 v2 宿主上必须装载成功"
    );

    let read = app
        .root
        .get("v1-svc")
        .and_then(|value| value.downcast::<ExternValue>())
        .map(|value| value.0.clone());
    assert_eq!(
        read.and_then(|value| value.get("from").cloned()),
        Some(serde_json::json!("v1")),
        "v1 插件经宿主函数表提供的服务必须真的可读"
    );

    fiber.dispose().await;
    assert!(
        V1_DISPOSED.load(Ordering::SeqCst),
        "卸载通知（OP_DISPOSE）必须送达 v1 插件"
    );
}

// ------------------------------------------------------------------ A3 的自动矩阵（M10）

/// 一个可探测的字段：标签（**偏移是算出来的**，不是手抄的）、探测方式。
struct Probe {
    label: String,
    offset: usize,
    width: usize,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    U32,
    Blob,
    CallFn,
}

impl Probe {
    /// 真读一次：`field_at` 的判界是 `size >= offset + width`，所以这里的结果
    /// 就是宿主侧的答案（不是我们对规则的复述）。
    fn readable(&self, desc: &PluginDesc) -> bool {
        let desc = desc as *const PluginDesc;
        match self.kind {
            Kind::U32 => unsafe { PluginDesc::field_at::<u32>(desc, self.offset) }.is_some(),
            Kind::Blob => unsafe { PluginDesc::field_at::<Blob>(desc, self.offset) }.is_some(),
            Kind::CallFn => {
                unsafe { PluginDesc::field_at::<CallFnShape>(desc, self.offset) }.is_some()
            }
        }
    }
}

type CallFnShape = unsafe extern "C" fn(u64, u32, Blob, Blob, *mut cordis_abi::Out) -> i32;

fn probes() -> Vec<Probe> {
    use std::mem::{offset_of, size_of};
    let field = |label: &str, offset: usize, width: usize, kind: Kind| Probe {
        label: format!("{label}@{offset}"),
        offset,
        width,
        kind,
    };
    vec![
        field(
            "abi_version",
            offset_of!(PluginDesc, abi_version),
            size_of::<u32>(),
            Kind::U32,
        ),
        field(
            "size",
            offset_of!(PluginDesc, size),
            size_of::<u32>(),
            Kind::U32,
        ),
        field(
            "id",
            offset_of!(PluginDesc, id),
            size_of::<Blob>(),
            Kind::Blob,
        ),
        field(
            "inject",
            offset_of!(PluginDesc, inject),
            size_of::<Blob>(),
            Kind::Blob,
        ),
        field(
            "schema",
            offset_of!(PluginDesc, schema),
            size_of::<Blob>(),
            Kind::Blob,
        ),
        field(
            "attach",
            offset_of!(PluginDesc, attach),
            size_of::<CallFnShape>(),
            Kind::CallFn,
        ),
        field(
            "call",
            offset_of!(PluginDesc, call),
            size_of::<CallFnShape>(),
            Kind::CallFn,
        ),
        field(
            "provision",
            offset_of!(PluginDesc, provision),
            size_of::<Blob>(),
            Kind::Blob,
        ),
        field(
            "error_format",
            offset_of!(PluginDesc, error_format),
            size_of::<u32>(),
            Kind::U32,
        ),
        field(
            "tail_reserved",
            offset_of!(PluginDesc, tail_reserved),
            size_of::<u32>(),
            Kind::U32,
        ),
    ]
}

/// 矩阵里的 `size` 取样：每个字段的**边界前后**都取到（够 / 差一个字节两种）。
fn sampled_sizes(prefix: usize) -> Vec<usize> {
    let mut sizes: Vec<usize> = Vec::new();
    for probe in probes() {
        sizes.push(probe.offset + probe.width); // 恰好够
        sizes.push(probe.offset + probe.width - 1); // 差一个字节 ⇒ 读不到
    }
    sizes.push(0);
    sizes.push(prefix);
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

/// **M10c（A3 的后半截）**：兼容矩阵——宿主只读 `PluginDesc.size` **覆盖得到**的字段。
///
/// 这条测试同时是三件事：
/// 1. **判界即证据**：每一格都是**真读一次** `field_at` 得来的，并与规则
///    `size >= offset + width` 对照——"越界读"在结构上等价于"读一个不存在的字段 ⇒ `None`"；
/// 2. **门禁矩阵**：`desc_gate` 在每个取样尺寸上的收/拒与"`size >= 冻结前缀`"一致；
/// 3. **文档防漂移**：把矩阵渲染成 `兼容矩阵.md`（仓库根）。默认**只断言**（与已提交的
///    文档逐字一致），`CORDIS_WRITE_MATRIX=1` 时**生成**它——
///    这样"自动生成的文档"不会悄悄过期：
///    `CORDIS_WRITE_MATRIX=1 cargo test -p rust-cordis-host --test abi_matrix`。
#[test]
fn abi_compatibility_matrix() {
    let prefix = cordis_abi::PLUGIN_DESC_PREFIX_SIZE;
    let probes = probes();
    let sizes = sampled_sizes(prefix);

    let mut rows: Vec<(usize, bool, Vec<bool>)> = Vec::new();
    for size in &sizes {
        let desc = desc_with(*size as u32);
        let readable: Vec<bool> = probes.iter().map(|probe| probe.readable(desc)).collect();
        for (probe, readable) in probes.iter().zip(&readable) {
            // ① 判界即证据：真读的结果必须与规则逐格一致。
            assert_eq!(
                *readable,
                *size >= probe.offset + probe.width,
                "size={size} 的 {} 读取结果与判界规则不符",
                probe.label
            );
        }
        let accepted = desc_gate(ABI_VERSION, *size, prefix).is_ok();
        // ② 门禁矩阵：只有盖满冻结前缀的 desc 才收（G14 的新规则）。
        assert_eq!(
            accepted,
            *size >= prefix,
            "size={size} 的门禁结论与规则不符"
        );
        rows.push((*size, accepted, readable));
    }

    // ③ 渲染 + 防漂移。
    let rendered = render_matrix(&probes, &rows, prefix);
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../兼容矩阵.md");
    if std::env::var_os("CORDIS_WRITE_MATRIX").is_some() {
        std::fs::write(&doc, rendered.as_bytes()).expect("写兼容矩阵.md");
        eprintln!("已生成 {}", doc.display());
        return;
    }
    let text = std::fs::read_to_string(&doc).unwrap_or_else(|err| {
        panic!(
            "读 {} 失败（用 `CORDIS_WRITE_MATRIX=1 cargo test -p rust-cordis-host --test abi_matrix` 生成）: {err}",
            doc.display()
        )
    });
    assert_eq!(
        text.replace("\r\n", "\n"),
        rendered,
        "兼容矩阵.md 与自动生成的不一致：用 CORDIS_WRITE_MATRIX=1 重新生成"
    );
}

fn render_matrix(probes: &[Probe], rows: &[(usize, bool, Vec<bool>)], prefix: usize) -> String {
    let mut out = String::new();
    out.push_str("# ABI 兼容矩阵（自动生成，勿手改）\n\n");
    out.push_str("> 生成者：`crates/host/tests/abi_matrix.rs::abi_compatibility_matrix`\n");
    out.push_str(
        "> （`CORDIS_WRITE_MATRIX=1 cargo test -p rust-cordis-host --test abi_matrix`）。\n",
    );
    out.push_str(
        "> 它同时是 A3 后半截的可执行证据：**宿主只读 `PluginDesc.size` 覆盖得到的字段**；\n",
    );
    out.push_str(
        "> `size` 之外一律读作 `None`＝「插件没声明」——前提是插件**如实声明** `size`；\n\
         > 插件谎报更大的 `size` 时仍可能越界读，宿主不把它当不可信输入。\n\n",
    );
    out.push_str(&format!(
        "冻结前缀 `PLUGIN_DESC_PREFIX_SIZE` = **{prefix}**（`size >= prefix` 才收）。\n\n"
    ));
    out.push_str("| `size` | 门禁 |");
    for probe in probes {
        out.push_str(&format!(" `{}` |", probe.label));
    }
    out.push('\n');
    out.push_str("|---|---|");
    for _ in probes {
        out.push_str("---|");
    }
    out.push('\n');
    for (size, accepted, readable) in rows {
        out.push_str(&format!(
            "| {size} | {} |",
            if *accepted { "收" } else { "拒" }
        ));
        for cell in readable {
            out.push_str(&format!(" {} |", if *cell { "读得到" } else { "—" }));
        }
        out.push('\n');
    }
    out.push_str("\n## 读法\n\n");
    out.push_str("- 一份 desc 只声明 `size` 以内的字段：v1 插件（`size = prefix`）读不到尾追加的 `provision`，\n");
    out.push_str("  宿主读到的 `None` 就是「未声明」；`Blob::EMPTY` 才是「有字段但没提供」。\n");
    out.push_str("- 表里的每一格都是**真读一次** `PluginDesc::field_at` 的结果，并与规则\n");
    out.push_str("  `size >= offset + width` 逐格对照；规则一旦被改坏，这条测试先红。\n");
    out
}
