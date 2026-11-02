//! 可移植的关闭信号入口。
//!
//! - Windows：`SetConsoleCtrlHandler` 捕获 Ctrl-C / Ctrl-Break / 控制台关闭；
//! - Unix：捕获 `SIGINT` / `SIGTERM`；
//! - 同时保留 Enter（stdin 一行）作为显式关闭方式。
//!
//! 信号处理器只做一件事：把一个 `AtomicBool` 置位。真正的清理仍由
//! [`crate::RunSession::shutdown`] / `App::close` 在正常异步路径里完成。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);
static COMPLETE: AtomicBool = AtomicBool::new(false);
static STDIN_STARTED: AtomicBool = AtomicBool::new(false);

/// 安装平台关闭处理器；重复调用是幂等的。
pub fn install() -> Result<(), String> {
    if INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }

    #[cfg(windows)]
    if let Err(err) = install_windows() {
        INSTALLED.store(false, Ordering::SeqCst);
        return Err(err);
    }

    #[cfg(unix)]
    if let Err(err) = install_unix() {
        INSTALLED.store(false, Ordering::SeqCst);
        return Err(err);
    }

    Ok(())
}

/// 请求关闭。信号处理器与测试都走这里；仅做原子写，满足信号安全。
pub fn request_shutdown() {
    REQUESTED.store(true, Ordering::SeqCst);
}

pub fn is_requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// 标记优雅清理已经完成。Windows 的 `CTRL_CLOSE_EVENT` 处理器会等这个信号，
/// 避免系统在 `App::close` 排水中途直接结束进程。
pub fn complete() {
    COMPLETE.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
fn wait_for_completion(timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !COMPLETE.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 等到关闭请求：Enter、Ctrl-C、SIGTERM 或控制台关闭事件。
pub async fn wait() {
    if is_requested() {
        return;
    }
    start_stdin_watcher();
    // EOF 不是关闭请求：无终端运行（stdin 被关闭 / 重定向到 /dev/null）时
    // 仍要等 Ctrl-C / SIGTERM，否则 `cordis run` 会启动即退出。
    while !REQUESTED.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 服务模式专用：不启动 stdin watcher，只等 SCM/信号置位。
pub async fn wait_service() {
    while !REQUESTED.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn start_stdin_watcher() {
    if STDIN_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            // 读到一行（含空行 Enter）⇒ 用户显式要求关闭。
            Ok(n) if n > 0 => request_shutdown(),
            // Ok(0) = EOF，Err = stdin 不可用：都只说明"没有交互输入"，
            // 不能把它当成用户按了关闭。继续等系统信号。
            _ => {}
        }
    });
}

#[cfg(windows)]
fn install_windows() -> Result<(), String> {
    unsafe {
        if SetConsoleCtrlHandler(Some(console_handler), 1) == 0 {
            return Err(format!(
                "SetConsoleCtrlHandler 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn console_handler(ctrl_type: u32) -> i32 {
    // CTRL_C_EVENT / CTRL_BREAK_EVENT / CTRL_CLOSE_EVENT /
    // CTRL_LOGOFF_EVENT / CTRL_SHUTDOWN_EVENT
    match ctrl_type {
        0 | 1 => {
            request_shutdown();
            1
        }
        // 关闭/注销/关机事件返回后系统可能直接结束进程；在这里等异步清理收尾。
        2 | 5 | 6 => {
            request_shutdown();
            wait_for_completion(Duration::from_secs(4));
            1
        }
        _ => 0,
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

#[cfg(unix)]
fn install_unix() -> Result<(), String> {
    use std::os::raw::c_int;

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    const SIG_ERR: usize = usize::MAX;

    for signal_number in [SIGINT, SIGTERM] {
        let previous = unsafe { signal(signal_number, unix_handler as *const () as usize) };
        if previous == SIG_ERR {
            return Err(format!(
                "安装信号 {signal_number} 处理器失败: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
extern "C" fn unix_handler(_signal: std::os::raw::c_int) {
    request_shutdown();
}

#[cfg(unix)]
#[link(name = "c")]
unsafe extern "C" {
    fn signal(signal: std::os::raw::c_int, handler: usize) -> usize;
}
