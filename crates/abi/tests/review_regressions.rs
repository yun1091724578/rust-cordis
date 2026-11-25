//! Regression tests for P0/P1/P2 findings in `cordis-abi`.

use cordis_abi::{HostApi, Out, PluginDesc, STATUS_ERR, STATUS_OK};

#[test]
fn out_write_rejects_null_ptr_with_nonzero_cap() {
    let mut out = Out {
        ptr: std::ptr::null_mut(),
        cap: 16,
        len: 0,
    };
    let status = unsafe { out.write(b"hello") };
    assert_eq!(status, STATUS_ERR);
}

#[test]
fn out_write_rejects_oversized_cap() {
    let mut buf = [0u8; 8];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: usize::MAX,
        len: 0,
    };
    let status = unsafe { out.write(b"hi") };
    assert_eq!(status, STATUS_ERR);
}

#[test]
fn field_at_does_not_overflow_on_huge_offset() {
    let host = HostApi::default();
    let v = unsafe { host.field_at::<u32>(usize::MAX - 1) };
    assert!(v.is_none());

    let desc = PluginDesc {
        abi_version: cordis_abi::ABI_VERSION,
        size: std::mem::size_of::<PluginDesc>() as u32,
        id: cordis_abi::Blob::new(b""),
        inject: cordis_abi::Blob::new(b""),
        schema: cordis_abi::Blob::new(b""),
        attach: cordis_abi::sdk::attach,
        call: dummy_call,
        provision: cordis_abi::Blob::new(b""),
        error_format: 0,
        tail_reserved: 0,
    };
    let v2 = unsafe { PluginDesc::field_at::<u32>(&desc as *const PluginDesc, usize::MAX - 1) };
    assert!(v2.is_none());
}

unsafe extern "C" fn dummy_call(
    _ctx: u64,
    _op: u32,
    _a: cordis_abi::Blob,
    _b: cordis_abi::Blob,
    _out: *mut Out,
) -> i32 {
    STATUS_OK
}
