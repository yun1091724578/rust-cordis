//! 关闭信号入口的可测试部分：安装不自爆，收到请求后 `wait()` 能返回。
//!
//! 真正的 Ctrl-C / SIGTERM 由平台处理器设置同一面 flag；这里不向测试进程发信号。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

#[tokio::test]
async fn install_is_idempotent_and_request_wakes_wait() {
    cordis_cli::shutdown::install().expect("安装控制台/信号处理器");
    cordis_cli::shutdown::install().expect("重复安装应是幂等空操作");

    cordis_cli::shutdown::request_shutdown();
    assert!(cordis_cli::shutdown::is_requested());

    tokio::time::timeout(Duration::from_secs(1), cordis_cli::shutdown::wait())
        .await
        .expect("关闭请求后 wait 必须及时返回");
}

/// stdin EOF 不是"用户按了关闭"：没有终端（或 stdin 被关闭）时，
/// `cordis run` 必须继续等 Ctrl-C / SIGTERM，而不是启动即退出。
///
/// 进程级验证：`wait()` 的静态标志是进程级的，只有独立子进程才能
/// 断言"关闭子进程 stdin 之后它仍然活着"。
#[test]
fn stdin_eof_does_not_shutdown_the_run_command() {
    let config =
        std::env::temp_dir().join(format!("cordis-cli-stdin-eof-{}.yml", std::process::id()));
    std::fs::write(&config, "[]\n").expect("写空配置");

    let mut child = Command::new(env!("CARGO_BIN_EXE_cordis"))
        .arg("run")
        .arg(&config)
        .arg("--allow-unverified")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 cordis run");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    stderr.read_line(&mut line).expect("读取启动提示");
    assert!(line.contains("按 Enter"), "启动提示：{line}");

    drop(child.stdin.take());
    std::thread::sleep(Duration::from_millis(1000));
    let status = child.try_wait().expect("查询子进程状态");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&config);

    assert!(
        status.is_none(),
        "stdin EOF 不应触发关闭，但进程已经退出：{status:?}"
    );
}
