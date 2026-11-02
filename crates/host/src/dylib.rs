//! 极简动态库加载层。
//!
//! # 为什么不直接用 `libloading`
//! 1. 本机只有 `x86_64-pc-windows-gnu` 工具链且缺 `dlltool.exe`，`libloading` 因此编不过；
//! 2. 更重要的是**依赖预算**：动态加载本身就是 4 个系统调用的薄封装
//!    （`LoadLibraryW` / `GetProcAddress` / `FreeLibrary`，或 `dlopen`/`dlsym`/`dlclose`），
//!    为此引入一个 crate 不划算。内核的"任意插件安装"能力因此不依赖任何第三方代码。
//!
//! # 安全性
//! 这里集中了整个工程**唯一**的裸 FFI。约束收得很紧：
//! - 句柄类型不实现 `Clone`，生命周期就是 struct 的生命周期；
//! - `symbol` 是 `unsafe`：调用方必须自己保证符号签名与 ABI 版本匹配
//!   （这一步由 [`crate::LoadedPlugin::open`] 的版本闸门负责）；
//! - 句柄在 `Drop` 中释放，`LoadedPlugin` 的字段顺序保证它活到最后一刻。

use std::ffi::c_void;
use std::path::Path;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(path: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }

    pub struct DynLib(*mut c_void);

    // 系统句柄本身可以跨线程使用；`LoadedPlugin` 的 `Send + Sync` 由此成立。
    unsafe impl Send for DynLib {}
    unsafe impl Sync for DynLib {}

    impl DynLib {
        pub fn open(path: &Path) -> Result<Self, String> {
            let mut wide: Vec<u16> = OsStr::new(path).encode_wide().collect();
            wide.push(0);
            let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
            if handle.is_null() {
                return Err(format!(
                    "LoadLibraryW 失败 ({}): {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self(handle))
        }

        /// # Safety
        /// `name` 必须是 NUL 结尾的字节序列；符号的真实签名必须与调用方假设一致。
        pub unsafe fn symbol(&self, name: &[u8]) -> Result<*const c_void, String> {
            let pointer = GetProcAddress(self.0, name.as_ptr());
            if pointer.is_null() {
                let readable = String::from_utf8_lossy(&name[..name.len().saturating_sub(1)]);
                return Err(format!("未导出符号 {readable}"));
            }
            Ok(pointer as *const c_void)
        }
    }

    impl Drop for DynLib {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;

    extern "C" {
        fn dlopen(filename: *const i8, flags: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
    }

    const RTLD_NOW: i32 = 2;

    pub struct DynLib(*mut c_void);

    unsafe impl Send for DynLib {}
    unsafe impl Sync for DynLib {}

    impl DynLib {
        pub fn open(path: &Path) -> Result<Self, String> {
            let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes())
                .map_err(|err| err.to_string())?;
            let handle = unsafe { dlopen(c_path.as_ptr(), RTLD_NOW) };
            if handle.is_null() {
                return Err(format!("dlopen 失败: {}", path.display()));
            }
            Ok(Self(handle))
        }

        /// # Safety
        /// `name` 必须是 NUL 结尾的字节序列；符号的真实签名必须与调用方假设一致。
        pub unsafe fn symbol(&self, name: &[u8]) -> Result<*const c_void, String> {
            let c_name = std::ffi::CString::new(&name[..name.len().saturating_sub(1)])
                .map_err(|err| err.to_string())?;
            let pointer = dlsym(self.0, c_name.as_ptr());
            if pointer.is_null() {
                return Err(format!("未导出符号 {}", c_name.to_string_lossy()));
            }
            Ok(pointer as *const c_void)
        }
    }

    impl Drop for DynLib {
        fn drop(&mut self) {
            unsafe {
                dlclose(self.0);
            }
        }
    }
}

pub use imp::DynLib;
