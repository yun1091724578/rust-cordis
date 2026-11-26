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
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::Duration;

    const PENDING_WAIT_HINT_MS: u32 = 30_000;
    const PENDING_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

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

    #[derive(Clone, Copy)]
    struct ServiceState {
        state: u32,
        exit_code: u32,
        checkpoint: u32,
        wait_hint: u32,
    }

    static ARGS: OnceLock<ServiceArgs> = OnceLock::new();
    static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static SERVICE_STATE: OnceLock<Mutex<ServiceState>> = OnceLock::new();

    fn state_lock() -> &'static Mutex<ServiceState> {
        SERVICE_STATE.get_or_init(|| {
            Mutex::new(ServiceState {
                state: SERVICE_STOPPED,
                exit_code: 0,
                checkpoint: 0,
                wait_hint: 0,
            })
        })
    }

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
        crate::shutdown::reset();
        STATUS_HANDLE.store(handle, Ordering::SeqCst);
        set_status(SERVICE_START_PENDING, 0, 1, PENDING_WAIT_HINT_MS);

        let exit_code = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let start_heartbeat =
                PendingHeartbeat::start(SERVICE_START_PENDING, PENDING_HEARTBEAT_INTERVAL);
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    drop(start_heartbeat);
                    eprintln!("cordis service: 无法创建 tokio runtime: {err}");
                    return 4;
                }
            };

            runtime.block_on(async {
                match RunSession::start(&args.config, args.options.clone(), disk_source()).await {
                    Ok(session) => {
                        drop(start_heartbeat);
                        set_running_if_not_stopping();
                        crate::shutdown::wait_service().await;
                        ensure_stop_pending();
                        let stop_heartbeat = PendingHeartbeat::start(
                            SERVICE_STOP_PENDING,
                            PENDING_HEARTBEAT_INTERVAL,
                        );
                        let report = session.shutdown().await;
                        drop(stop_heartbeat);
                        crate::shutdown::complete();
                        if report.close_report.status != cordis_core::CloseStatus::Completed {
                            eprintln!(
                                "cordis service: 关闭状态 {} (errors={}, still_running={})",
                                report.close_report.status.as_str(),
                                report.close_report.errors.len(),
                                report.close_report.still_running
                            );
                            for err in &report.close_report.errors {
                                eprintln!(
                                    "  - {}#{} [{}]: {}",
                                    err.fiber,
                                    err.uid,
                                    err.kind.as_str(),
                                    err.message
                                );
                            }
                            4
                        } else {
                            0
                        }
                    }
                    Err(err) => {
                        drop(start_heartbeat);
                        eprintln!("cordis service: {err}");
                        err.exit_code() as u32
                    }
                }
            })
        })) {
            Ok(code) => code,
            Err(_) => {
                eprintln!("cordis service: 运行时发生未捕获 panic，已拦截防止跨 FFI 展开");
                crate::shutdown::complete();
                4
            }
        };

        set_stopped_and_clear_handle(exit_code);
    }

    /// 原子完成终态上报并清零 `STATUS_HANDLE`，避免 `control_handler` 在 `SERVICE_STOPPED`
    /// 与清句柄之间插队覆盖终态。
    fn set_stopped_and_clear_handle(exit_code: u32) {
        let mut guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
        *guard = ServiceState {
            state: SERVICE_STOPPED,
            exit_code,
            checkpoint: 0,
            wait_hint: 0,
        };
        report(ServiceStatus::new(SERVICE_STOPPED, exit_code, 0, 0));
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
                ensure_stop_pending();
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

    /// 保证在进入关闭清理阶段时 SCM 至少已收到 `SERVICE_STOP_PENDING`（不覆盖已推进的 checkpoint）。
    fn ensure_stop_pending() {
        let mut guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
        if guard.state == SERVICE_STOP_PENDING || guard.state == SERVICE_STOPPED {
            return;
        }
        *guard = ServiceState {
            state: SERVICE_STOP_PENDING,
            exit_code: 0,
            checkpoint: 1,
            wait_hint: PENDING_WAIT_HINT_MS,
        };
        report(ServiceStatus::new(
            SERVICE_STOP_PENDING,
            0,
            1,
            PENDING_WAIT_HINT_MS,
        ));
    }

    /// 在 `START_PENDING` / `STOP_PENDING` 期间按周期递增 `dwCheckPoint` 上报 SCM，
    /// 避免启动或排水超过 `dwWaitHint`（30s）时被 SCM 以 `ERROR_SERVICE_NOT_RESPONDING (1053)` 强杀。
    fn advance_pending_checkpoint(expected_state: u32) -> bool {
        let mut guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
        if guard.state != expected_state {
            return false;
        }
        guard.checkpoint = guard.checkpoint.saturating_add(1).max(1);
        report(ServiceStatus::new(
            guard.state,
            guard.exit_code,
            guard.checkpoint,
            guard.wait_hint,
        ));
        true
    }

    /// 独立 OS 线程心跳守卫：不依赖 Tokio worker 是否阻塞；Drop 时同步停止并 `join`，
    /// 确保不会在切换到 `RUNNING` / `STOPPED` 之后再发出陈旧的 `PENDING` 状态。
    struct PendingHeartbeat {
        stop: Arc<(Mutex<bool>, Condvar)>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl PendingHeartbeat {
        fn start(expected_state: u32, interval: Duration) -> Self {
            let stop = Arc::new((Mutex::new(false), Condvar::new()));
            let signal = Arc::clone(&stop);
            let thread = std::thread::Builder::new()
                .name("cordis-scm-heartbeat".to_string())
                .spawn(move || {
                    let (lock, cvar) = &*signal;
                    let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while !*stopped {
                        let (guard, _) = cvar
                            .wait_timeout(stopped, interval)
                            .unwrap_or_else(|e| e.into_inner());
                        stopped = guard;
                        if *stopped {
                            break;
                        }
                        if !advance_pending_checkpoint(expected_state) {
                            break;
                        }
                    }
                })
                .ok();
            Self { stop, thread }
        }
    }

    impl Drop for PendingHeartbeat {
        fn drop(&mut self) {
            let (lock, cvar) = &*self.stop;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cvar.notify_all();
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
        }
    }

    fn set_status(state: u32, exit_code: u32, checkpoint: u32, wait_hint: u32) {
        let mut guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
        *guard = ServiceState {
            state,
            exit_code,
            checkpoint,
            wait_hint,
        };
        report(ServiceStatus::new(state, exit_code, checkpoint, wait_hint));
    }

    /// 仅在尚未收到停止请求时切到 `SERVICE_RUNNING`，避免与 `control_handler` 竞态覆盖 `STOP_PENDING`。
    fn set_running_if_not_stopping() {
        let mut guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
        if crate::shutdown::is_requested() || guard.state == SERVICE_STOP_PENDING {
            return;
        }
        *guard = ServiceState {
            state: SERVICE_RUNNING,
            exit_code: 0,
            checkpoint: 0,
            wait_hint: 0,
        };
        report(ServiceStatus::new(SERVICE_RUNNING, 0, 0, 0));
    }

    fn report_current() {
        let guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
        report(ServiceStatus::new(
            guard.state,
            guard.exit_code,
            guard.checkpoint,
            guard.wait_hint,
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

        /// 串行化访问进程级全局 `SERVICE_STATE` 的单元测试，避免并行测试互相踩状态。
        static TEST_SERIAL: Mutex<()> = Mutex::new(());

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

        #[test]
        fn pending_heartbeat_advances_checkpoint_and_stops_on_state_transition() {
            let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            // STATUS_HANDLE 为 0，SetServiceStatus 不会被实际调用，可安全单测状态机与心跳线程。
            set_status(SERVICE_START_PENDING, 0, 1, PENDING_WAIT_HINT_MS);
            let hb = PendingHeartbeat::start(SERVICE_START_PENDING, Duration::from_millis(10));
            std::thread::sleep(Duration::from_millis(60));
            drop(hb);

            let snap = *state_lock().lock().unwrap();
            assert_eq!(snap.state, SERVICE_START_PENDING);
            assert!(
                snap.checkpoint > 1,
                "START_PENDING 期间心跳必须递增 checkpoint，实际 {}",
                snap.checkpoint
            );

            // 切到 RUNNING 后，旧 START_PENDING 推进必须自动停止且不覆盖 RUNNING。
            set_status(SERVICE_RUNNING, 0, 0, 0);
            assert!(!advance_pending_checkpoint(SERVICE_START_PENDING));
            assert_eq!(state_lock().lock().unwrap().state, SERVICE_RUNNING);

            // ensure_stop_pending 切入 STOP_PENDING，且不重置已推进的 checkpoint。
            ensure_stop_pending();
            assert_eq!(state_lock().lock().unwrap().state, SERVICE_STOP_PENDING);
            assert_eq!(state_lock().lock().unwrap().checkpoint, 1);
            assert!(advance_pending_checkpoint(SERVICE_STOP_PENDING));
            assert_eq!(state_lock().lock().unwrap().checkpoint, 2);
            ensure_stop_pending();
            assert_eq!(
                state_lock().lock().unwrap().checkpoint,
                2,
                "重复 ensure_stop_pending 不得把已推进的 checkpoint 回退到 1"
            );
            set_status(SERVICE_STOPPED, 0, 0, 0);
        }

        #[test]
        fn r2_p1_control_handler_preserves_stopped_state_and_stop_pending_checkpoint() {
            let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            // Advance STOP_PENDING checkpoint to 3, then simulate duplicate SHUTDOWN/STOP controls.
            set_status(SERVICE_STOP_PENDING, 0, 1, PENDING_WAIT_HINT_MS);
            assert!(advance_pending_checkpoint(SERVICE_STOP_PENDING));
            assert!(advance_pending_checkpoint(SERVICE_STOP_PENDING));
            assert_eq!(state_lock().lock().unwrap().checkpoint, 3);

            let rc = unsafe {
                control_handler(
                    SERVICE_CONTROL_SHUTDOWN,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, NO_ERROR);
            {
                let snap = *state_lock().lock().unwrap();
                assert_eq!(snap.state, SERVICE_STOP_PENDING);
                assert_eq!(
                    snap.checkpoint, 3,
                    "control_handler 不得将已推进的 STOP_PENDING checkpoint 回退到 1"
                );
            }

            // Once SERVICE_STOPPED is reported, late STOP control must NEVER overwrite it.
            set_status(SERVICE_STOPPED, 4, 0, 0);
            let rc2 = unsafe {
                control_handler(
                    SERVICE_CONTROL_STOP,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc2, NO_ERROR);
            let snap = *state_lock().lock().unwrap();
            assert_eq!(
                snap.state, SERVICE_STOPPED,
                "control_handler 不得将 SERVICE_STOPPED 覆盖回 SERVICE_STOP_PENDING"
            );
            assert_eq!(snap.exit_code, 4);
            crate::shutdown::reset();
        }
    }
}
