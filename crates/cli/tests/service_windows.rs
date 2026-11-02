//! Windows SCM 入口的边界：不是由 SCM 启动时，`cordis service` 必须快速失败。
//!
//! 真正的服务生命周期需要 `sc create` / `net start` 才能覆盖；这里只锁定
//! “普通控制台进程误用 service 子命令”这条可自动化的失败路径。

#![cfg(windows)]

use std::process::Command;

#[test]
fn service_mode_outside_scm_fails_with_runtime_code() {
    let config = std::env::temp_dir().join(format!("cordis-service-{}.yml", std::process::id()));
    std::fs::write(&config, "[]\n").expect("写空配置");

    let output = Command::new(env!("CARGO_BIN_EXE_cordis"))
        .arg("service")
        .arg(&config)
        .args(["--name", "CordisTestLifecycle"])
        .output()
        .expect("运行 cordis service");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(4),
        "未由 SCM 启动时应返回运行时错误 4，stderr={stderr}"
    );
    assert!(
        stderr.contains("1063") || stderr.contains("SCM"),
        "错误信息应说明必须由 SCM 启动：{stderr}"
    );

    let _ = std::fs::remove_file(&config);
}
