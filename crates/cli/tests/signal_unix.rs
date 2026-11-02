//! Unix 信号关闭的进程级验证：SIGTERM 必须走 `App::close` 并以 0 退出。
//!
//! 只在一个独立子进程里发信号，绝不触碰测试进程自身。

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[test]
fn sigterm_triggers_graceful_shutdown() {
    let config = std::env::temp_dir().join(format!("cordis-signal-{}.yml", std::process::id()));
    std::fs::write(&config, "[]\n").expect("写空配置");

    let mut child = Command::new(env!("CARGO_BIN_EXE_cordis"))
        .arg("run")
        .arg(&config)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 cordis run");
    let pid = child.id();
    // 保持 stdin 打开：否则 stdin watcher 收到 EOF，会在信号之前自行关闭。
    let _stdin = child.stdin.take().expect("stdin");

    let finished = Arc::new(AtomicBool::new(false));
    let watchdog_flag = finished.clone();
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        if !watchdog_flag.load(Ordering::SeqCst) {
            unsafe {
                kill(pid as i32, SIGKILL);
            }
        }
    });

    // “按 Enter 或 Ctrl-C 关闭…” 在 install() 之后打印；读到它再发信号，
    // 避免在处理器安装前把进程直接 SIGTERM 掉。
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    loop {
        line.clear();
        let read = stderr.read_line(&mut line).expect("读 stderr");
        assert_ne!(read, 0, "cordis run 在打印关闭提示前退出");
        if line.contains("按 Enter") {
            break;
        }
    }

    let sent = unsafe { kill(pid as i32, SIGTERM) };
    assert_eq!(sent, 0, "发送 SIGTERM 失败");

    let status = child.wait().expect("等待子进程");
    finished.store(true, Ordering::SeqCst);
    let _ = watchdog.join();

    assert!(
        status.success(),
        "SIGTERM 必须触发优雅关闭并以 0 退出，实际: {status:?}"
    );
    let _ = std::fs::remove_file(&config);
}

#[test]
fn service_mode_is_windows_only() {
    let config =
        std::env::temp_dir().join(format!("cordis-service-unix-{}.yml", std::process::id()));
    std::fs::write(&config, "[]\n").expect("写空配置");

    let output = Command::new(env!("CARGO_BIN_EXE_cordis"))
        .arg("service")
        .arg(&config)
        .args(["--name", "CordisTestLifecycle"])
        .output()
        .expect("运行 cordis service");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(4), "stderr={stderr}");
    assert!(
        stderr.contains("Windows"),
        "非 Windows 平台应明确说明 service 不可用：{stderr}"
    );
    let _ = std::fs::remove_file(&config);
}
