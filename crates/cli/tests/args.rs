//! CLI 参数边界：缺值、重复位置参数和不允许的 flag 必须是用法错误。

fn run(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_cordis"))
        .args(args)
        .output()
        .expect("启动 cordis")
}

#[test]
fn missing_plugin_dir_value_is_a_usage_error() {
    let output = run(&["inspect", "config.yml", "--plugin-dir"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--plugin-dir"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn duplicate_config_positionals_are_rejected() {
    for command in ["inspect", "status", "run", "validate"] {
        let output = run(&[command, "a.yml", "b.yml"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{command} 不得静默采用最后一个位置参数"
        );
    }
}

#[test]
fn service_rejects_duplicate_config_positionals_before_scm_startup() {
    let output = run(&["service", "a.yml", "b.yml", "--name", "x"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn json_is_not_accepted_by_run() {
    let output = run(&["run", "config.yml", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--json"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}
