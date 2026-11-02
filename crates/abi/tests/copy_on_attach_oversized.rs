//! 宿主声明的 `size` 大于当前 `HostApi` 时，copy-on-attach 只真正复制
//! `size_of::<HostApi>()` 字节，因此副本的 `size` 必须收回到实际拷贝量。
//!
//! 独立测试二进制：`HOST` 是进程级 `OnceLock`。

use cordis_abi::sdk::{attach, attached_host};
use cordis_abi::{HostApi, ABI_VERSION};

#[test]
fn oversized_declared_size_is_clamped_to_the_actual_copy() {
    let full = HostApi {
        abi_version: ABI_VERSION,
        size: (std::mem::size_of::<HostApi>() + 64) as u32,
        ..HostApi::default()
    };
    unsafe { attach(&full) };

    let api = attached_host().expect("版本匹配时 attach 必须登记宿主表");
    assert_eq!(
        api.size as usize,
        std::mem::size_of::<HostApi>(),
        "副本不能声明比实际拷贝更多的字节"
    );
    assert!(
        unsafe { api.field_at::<u32>(std::mem::offset_of!(HostApi, error_format)) }.is_some(),
        "当前字段仍应可读"
    );
}
