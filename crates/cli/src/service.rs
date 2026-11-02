//! Windows Service Control Manager（SCM）入口。
//!
//! 控制台模式用 `SetConsoleCtrlHandler`；服务模式走的是另一套协议：
//! `StartServiceCtrlDispatcherW` + `RegisterServiceCtrlHandlerExW`。
//! 两者最终都调用同一个 `shutdown::request_shutdown()`，清理仍由
//! `RunSession::shutdown` / `App::close` 完成。

use crate::RunOptions;
use std::path::Path;

#[cfg(windows)]
pub fn run(config: &Path, name: &str, options: RunOptions) -> Result<(), String> {
    windows::run(config, name, options)
}

#[cfg(not(windows))]
pub fn run(_config: &Path, _name: &str, _options: RunOptions) -> Result<(), String> {
    Err("service 子命令仅在 Windows 上可用".to_string())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::{disk_source, RunSession};
    use std::ffi::OsStr;
    use std::os::raw::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::sync::OnceLock;

    const SERVICE_WIN32_OWN_PROCESS: u32 = 0x0000_0010;
    const SERVICE_STOPPED: u32 = 0x0000_0001;
    const SERVICE_START_PENDING: u32 = 0x0000_0002;
    const SERVICE_STOP_PENDING: u32 = 0x0000_0003;
    const SERVICE_RUNNING: u32 = 0x0000_0004;
    const SERVICE_ACCEPT_STOP: u32 = 0x0000_0001;
    const SERVICE_ACCEPT_SHUTDOWN: u32 = 0x0000_0004;
    const SERVICE_CONTROL_STOP: u32 = 0x0000_0001;
    const SERVICE_CONTROL_INTERROGATE: u32 = 0x0000_0004;
    const SERVICE_CONTROL_SHUTDOWN: u32 = 0x0000_0005;
    const NO_ERROR: u32 = 0;
    const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
    const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;

    #[repr(C)]
    #[derive(Debug)]
    struct ServiceStatus {
        service_type: u32,
        current_state: u32,
        controls_accepted: u32,
        win32_exit_code: u32,
        service_specific_exit_code: u32,
        checkpoint: u32,
        wait_hint: u32,
    }

    impl ServiceStatus {
        fn new(state: u32, exit_code: u32, checkpoint: u32, wait_hint: u32) -> Self {
            let controls_accepted = if state == SERVICE_RUNNING {
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
            } else {
                0
            };
            let (win32_exit_code, service_specific_exit_code) = if exit_code == 0 {
                (NO_ERROR, 0)
            } else {
                (ERROR_SERVICE_SPECIFIC_ERROR, exit_code)
            };
            Self {
                service_type: SERVICE_WIN32_OWN_PROCESS,
                current_state: state,
                controls_accepted,
                win32_exit_code,
                service_specific_exit_code,
                checkpoint,
                wait_hint,
            }
        }
    }

    struct ServiceArgs {
        name_wide: Vec<u16>,
        config: PathBuf,
        options: RunOptions,
    }

    static ARGS: OnceLock<ServiceArgs> = OnceLock::new();
    static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static LAST_STATE: AtomicU32 = AtomicU32::new(SERVICE_STOPPED);
    static LAST_EXIT: AtomicU32 = AtomicU32::new(0);
    static LAST_CHECKPOINT: AtomicU32 = AtomicU32::new(0);
    static LAST_WAIT_HINT: AtomicU32 = AtomicU32::new(0);

    type ServiceMainFn = unsafe extern "system" fn(u32, *mut *mut u16);

    #[repr(C)]
    struct ServiceTableEntryW {
        service_name: *const u16,
        service_proc: Option<ServiceMainFn>,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn StartServiceCtrlDispatcherW(table: *const ServiceTableEntryW) -> i32;
        fn RegisterServiceCtrlHandlerExW(
            service_name: *const u16,
            handler: Option<unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32>,
            context: *mut c_void,
        ) -> isize;
        fn SetServiceStatus(handle: isize, status: *const ServiceStatus) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLastError() -> u32;
    }

    pub fn run(config: &Path, name: &str, options: RunOptions) -> Result<(), String> {
        let name_wide: Vec<u16> = OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if name_wide.len() <= 1 {
            return Err("service 名称不能为空".to_string());
        }
        let service_name = name_wide.clone();
        ARGS.set(ServiceArgs {
            name_wide,
            config: config.to_path_buf(),
            options,
        })
        .map_err(|_| "service 已经启动过".to_string())?;

        let table = [
            ServiceTableEntryW {
                service_name: service_name.as_ptr(),
                service_proc: Some(service_main),
            },
            ServiceTableEntryW {
                service_name: std::ptr::null(),
                service_proc: None,
            },
        ];
        let started = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
        if started == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "StartServiceCtrlDispatcherW 失败（错误 {code}）。\
                 `cordis service` 必须由 Windows SCM 启动；请先用 `sc create` 注册服务。"
            ));
        }
        Ok(())
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
        let Some(args) = ARGS.get() else {
            return;
        };
        let handle = unsafe {
            RegisterServiceCtrlHandlerExW(
                args.name_wide.as_ptr(),
                Some(control_handler),
                std::ptr::null_mut(),
            )
        };
        if handle == 0 {
            return;
        }
        STATUS_HANDLE.store(handle, Ordering::SeqCst);
        set_status(SERVICE_START_PENDING, 0, 1, 30_000);

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("cordis service: 无法创建 tokio runtime: {err}");
                set_status(SERVICE_STOPPED, 4, 0, 0);
                STATUS_HANDLE.store(0, Ordering::SeqCst);
                return;
            }
        };

        let exit_code = runtime.block_on(async {
            match RunSession::start(&args.config, args.options.clone(), disk_source()).await {
                Ok(session) => {
                    if !crate::shutdown::is_requested() {
                        set_status(SERVICE_RUNNING, 0, 0, 0);
                    }
                    crate::shutdown::wait_service().await;
                    let _ = session.shutdown().await;
                    crate::shutdown::complete();
                    0
                }
                Err(err) => {
                    eprintln!("cordis service: {err}");
                    err.exit_code() as u32
                }
            }
        });

        set_status(SERVICE_STOPPED, exit_code, 0, 0);
        STATUS_HANDLE.store(0, Ordering::SeqCst);
    }

    unsafe extern "system" fn control_handler(
        control: u32,
        _event_type: u32,
        _event_data: *mut c_void,
        _context: *mut c_void,
    ) -> u32 {
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                set_status(SERVICE_STOP_PENDING, 0, 1, 30_000);
                crate::shutdown::request_shutdown();
                NO_ERROR
            }
            SERVICE_CONTROL_INTERROGATE => {
                report_current();
                NO_ERROR
            }
            _ => ERROR_CALL_NOT_IMPLEMENTED,
        }
    }

    fn set_status(state: u32, exit_code: u32, checkpoint: u32, wait_hint: u32) {
        LAST_STATE.store(state, Ordering::SeqCst);
        LAST_EXIT.store(exit_code, Ordering::SeqCst);
        LAST_CHECKPOINT.store(checkpoint, Ordering::SeqCst);
        LAST_WAIT_HINT.store(wait_hint, Ordering::SeqCst);
        report(ServiceStatus::new(state, exit_code, checkpoint, wait_hint));
    }

    fn report_current() {
        report(ServiceStatus::new(
            LAST_STATE.load(Ordering::SeqCst),
            LAST_EXIT.load(Ordering::SeqCst),
            LAST_CHECKPOINT.load(Ordering::SeqCst),
            LAST_WAIT_HINT.load(Ordering::SeqCst),
        ));
    }

    fn report(status: ServiceStatus) {
        let handle = STATUS_HANDLE.load(Ordering::SeqCst);
        if handle != 0 {
            unsafe {
                SetServiceStatus(handle, &status);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stopped_status_carries_service_specific_exit() {
            let status = ServiceStatus::new(SERVICE_STOPPED, 4, 0, 0);
            assert_eq!(status.current_state, SERVICE_STOPPED);
            assert_eq!(status.win32_exit_code, ERROR_SERVICE_SPECIFIC_ERROR);
            assert_eq!(status.service_specific_exit_code, 4);
            assert_eq!(status.controls_accepted, 0);
        }

        #[test]
        fn running_status_accepts_stop_and_shutdown() {
            let status = ServiceStatus::new(SERVICE_RUNNING, 0, 0, 0);
            assert_eq!(
                status.controls_accepted,
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
            );
            assert_eq!(status.win32_exit_code, NO_ERROR);
        }
    }
}
