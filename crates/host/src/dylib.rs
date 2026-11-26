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

/// 裸文件名统一补 `.` 前缀：POSIX `dlopen` 与 Windows `LoadLibraryW` 对裸文件名都会搜索系统/PATH
/// 目录而非显式限定当前目录路径。
fn normalize_open_path(path: &Path) -> std::borrow::Cow<'_, Path> {
    if path.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        std::borrow::Cow::Owned(Path::new(".").join(path))
    } else {
        std::borrow::Cow::Borrowed(path)
    }
}

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
            let effective = normalize_open_path(path);
            let mut wide: Vec<u16> = OsStr::new(effective.as_os_str()).encode_wide().collect();
            if wide.contains(&0) {
                return Err(format!(
                    "动态库路径包含非法内部 NUL 字符 ({})",
                    path.display()
                ));
            }
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
    use std::ffi::CStr;
    use std::os::raw::c_char;
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn dlopen(filename: *const c_char, flags: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
        fn dlerror() -> *const c_char;
    }

    const RTLD_NOW: i32 = 2;

    pub struct DynLib(*mut c_void);

    unsafe impl Send for DynLib {}
    unsafe impl Sync for DynLib {}

    fn last_dlerror() -> String {
        let ptr = unsafe { dlerror() };
        if ptr.is_null() {
            "unknown dlerror".to_string()
        } else {
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }

    impl DynLib {
        pub fn open(path: &Path) -> Result<Self, String> {
            let effective = normalize_open_path(path);
            let c_path = std::ffi::CString::new(effective.as_os_str().as_bytes())
                .map_err(|err| err.to_string())?;
            let handle = unsafe { dlopen(c_path.as_ptr(), RTLD_NOW) };
            if handle.is_null() {
                return Err(format!(
                    "dlopen 失败 ({}): {}",
                    path.display(),
                    last_dlerror()
                ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r2_p3_dylib_open_rejects_interior_nul() {
        #[cfg(windows)]
        let bad = {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;
            let wide: Vec<u16> = "foo\0bar.dll".encode_utf16().collect();
            std::path::PathBuf::from(OsString::from_wide(&wide))
        };
        #[cfg(unix)]
        let bad = {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            std::path::PathBuf::from(OsStr::from_bytes(b"foo\0bar.so"))
        };
        let err = DynLib::open(&bad)
            .err()
            .expect("含内部 NUL 的动态库路径必须被显式拒绝");
        assert!(
            err.to_ascii_lowercase().contains("nul"),
            "错误必须明确指出 NUL 字节，而非交给系统截断加载：{err}"
        );
    }

    #[test]
    fn r2_p3_normalize_open_path_prefixes_bare_filename() {
        let bare = normalize_open_path(Path::new("plugin.dll"));
        assert_eq!(bare.as_ref(), Path::new(".").join("plugin.dll"));
        let nested = normalize_open_path(Path::new("dir/plugin.dll"));
        assert_eq!(nested.as_ref(), Path::new("dir/plugin.dll"));
    }
}
