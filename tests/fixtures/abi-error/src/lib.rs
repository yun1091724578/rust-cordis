//! Test fixture: a current-layout plugin that returns a structured `STATUS_ERR`
//! from `OP_APPLY`. It intentionally does not link `cordis-abi`, so the host test
//! exercises the `LoadedPlugin::apply` capability seam rather than the SDK helper.

use std::sync::OnceLock;

const ABI_VERSION: u32 = 1;
const OP_APPLY: u32 = 1;
const OP_DISPOSE: u32 = 2;
const OP_PING: u32 = 5;
const STATUS_OK: i32 = 0;
const STATUS_ERR: i32 = 1;
const STATUS_UNSUPPORTED: i32 = 3;
const ERROR_FORMAT_V1: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Blob {
    ptr: *const u8,
    len: usize,
}

impl Blob {
    const EMPTY: Blob = Blob {
        ptr: std::ptr::null(),
        len: 0,
    };

    fn new(bytes: &'static [u8]) -> Self {
        Self {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    }
}

#[repr(C)]
pub struct Out {
    ptr: *mut u8,
    cap: usize,
    len: usize,
}

impl Out {
    unsafe fn write(&mut self, bytes: &[u8]) -> i32 {
        if bytes.len() > self.cap {
            self.len = bytes.len();
            return 2; // STATUS_TOO_SMALL
        }
        if !bytes.is_empty() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr, bytes.len());
        }
        self.len = bytes.len();
        STATUS_OK
    }
}

/// The fixture only needs the pointer type; it never calls host functions.
#[repr(C)]
pub struct HostApi {
    _private: [u8; 0],
}

#[repr(C)]
pub struct PluginDesc {
    abi_version: u32,
    size: u32,
    id: Blob,
    inject: Blob,
    schema: Blob,
    attach: unsafe extern "C" fn(*const HostApi),
    call: unsafe extern "C" fn(u64, u32, Blob, Blob, *mut Out) -> i32,
    provision: Blob,
    error_format: u32,
    tail_reserved: u32,
}

// The descriptor is immutable after initialization; the raw blobs point at
// static byte literals, so sharing it across threads is sound.
unsafe impl Send for PluginDesc {}
unsafe impl Sync for PluginDesc {}

static DESC: OnceLock<PluginDesc> = OnceLock::new();

unsafe extern "C" fn attach(_: *const HostApi) {}

unsafe extern "C" fn call(_: u64, op: u32, _: Blob, _: Blob, out: *mut Out) -> i32 {
    match op {
        OP_APPLY => {
            let payload = br#"{"error":"apply boom","code":"E_APPLY","category":"business","certainty":"reported","retry":"never","detail":null}"#;
            let status = (*out).write(payload);
            if status == STATUS_OK {
                STATUS_ERR
            } else {
                status
            }
        }
        OP_PING | OP_DISPOSE => (*out).write(b""),
        _ => STATUS_UNSUPPORTED,
    }
}

#[no_mangle]
pub extern "C" fn cordis_plugin_v1() -> *const PluginDesc {
    DESC.get_or_init(|| PluginDesc {
        abi_version: ABI_VERSION,
        size: std::mem::size_of::<PluginDesc>() as u32,
        id: Blob::new(b"abi-error-fixture"),
        inject: Blob::new(b"[]"),
        schema: Blob::new(br#"{"type":"object"}"#),
        attach,
        call,
        provision: Blob::EMPTY,
        error_format: ERROR_FORMAT_V1,
        tail_reserved: 0,
    })
}
