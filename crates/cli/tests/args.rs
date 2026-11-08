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
fn source_policy_requires_a_value() {
    for command in ["inspect", "status", "run", "validate"] {
        let output = run(&[command, "config.yml", "--source-policy"]);
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--source-policy"),
            "{command} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = run(&["service", "config.yml", "--name", "x", "--source-policy"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn missing_source_policy_file_is_a_config_error() {
    let missing = std::env::temp_dir().join(format!(
        "cordis-missing-source-policy-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&missing);
    let output = run(&[
        "inspect",
        "config.yml",
        "--source-policy",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("allow-list"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_source_policy_json_is_a_config_error() {
    let dir = std::env::temp_dir().join(format!(
        "cordis-invalid-source-policy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let policy = dir.join("allow-list.json");
    std::fs::write(&policy, "not json").unwrap();
    let output = run(&[
        "inspect",
        "config.yml",
        "--source-policy",
        policy.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("解析来源 allow-list"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
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
