//! Regression tests for P0/P1/P2 findings in `cordis-host`.

use cordis_core::App;
use cordis_host::js::{BuiltinJs, JsEvaluator};
use cordis_host::plan::{resolve, PlanEntry};
use cordis_host::resolve_plugin_path;
use semver::{Version, VersionReq};
use std::collections::BTreeMap;

// P1: plan::resolve must find a provider that satisfies ALL requirements for the same key.
#[test]
fn plan_resolve_finds_common_provider_satisfying_all_requirements() {
    let entries = vec![
        PlanEntry {
            id: "p1".into(),
            version: Version::parse("1.0.0").unwrap(),
            provides: BTreeMap::from([("svc".into(), Version::parse("1.0.0").unwrap())]),
            requires: BTreeMap::new(),
        },
        PlanEntry {
            id: "p2".into(),
            version: Version::parse("2.0.0").unwrap(),
            provides: BTreeMap::from([("svc".into(), Version::parse("2.0.0").unwrap())]),
            requires: BTreeMap::new(),
        },
        PlanEntry {
            id: "consumer-a".into(),
            version: Version::parse("1.0.0").unwrap(),
            provides: BTreeMap::new(),
            requires: BTreeMap::from([("svc".into(), VersionReq::parse(">=1.0.0").unwrap())]),
        },
        PlanEntry {
            id: "consumer-b".into(),
            version: Version::parse("1.0.0").unwrap(),
            provides: BTreeMap::new(),
            requires: BTreeMap::from([("svc".into(), VersionReq::parse("^1.0.0").unwrap())]),
        },
    ];

    let plan = resolve(&entries).expect("p1 1.0.0 satisfies both >=1.0.0 and ^1.0.0");
    let chosen = plan.providers.get("svc").expect("svc resolved");
    assert_eq!(chosen.plugin, "p1");
    assert_eq!(chosen.version, Version::parse("1.0.0").unwrap());
}

// P1: resolve_plugin_path must search explicit search dirs BEFORE CWD and reject `..` traversal.
#[test]
fn resolve_plugin_path_rejects_parent_dir_traversal() {
    let dir = std::env::temp_dir();
    let err = resolve_plugin_path("../some_plugin", &[dir]).unwrap_err();
    assert!(
        err.to_string().contains(".."),
        "expected path traversal rejection, got: {err}"
    );
}

// P2: js eval_step must support `process.env['KEY']` / `env['KEY']` subscript access.
#[test]
fn js_eval_supports_env_bracket_index() {
    let key = "CORDIS_TEST_JS_ENV_BRACKET_KEY";
    // SAFETY: test-only unique env var name.
    unsafe { std::env::set_var(key, "hello-bracket") };
    let app = App::new();
    let val = BuiltinJs
        .evaluate(&format!("process.env['{key}']"), &app.root)
        .expect("process.env['KEY'] must be supported by eval_step");
    assert_eq!(val, serde_json::json!("hello-bracket"));
    let val2 = BuiltinJs
        .evaluate(&format!("env['{key}']"), &app.root)
        .expect("env['KEY'] must be supported by eval_step");
    assert_eq!(val2, serde_json::json!("hello-bracket"));
    unsafe { std::env::remove_var(key) };
}

// Problem 2: on_commit disk persistence must NOT block the Tokio worker thread.
#[tokio::test]
async fn on_commit_persist_does_not_block_tokio_worker_when_lock_is_held() {
    use cordis_core::{plugin_fn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct DummySource;
    impl PluginSource for DummySource {
        fn load(
            &self,
            _entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            Ok(LoadedFile {
                plugin: plugin_fn("demo", Inject::default(), |_ctx, _cfg| {
                    Box::pin(async { Ok(()) })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "cordis-on-commit-nonblocking-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(&cfg_path, "- id: demo\n  name: demo\n  config:\n    v: 1\n").unwrap();

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(DummySource),
    )
    .await
    .expect("load_with_source");
    let fiber = state.fiber_of("demo").expect("demo fiber");

    // Pre-create `.cordis.yml.lock` so ConfigLock::acquire must wait until released by a
    // concurrent async task on THIS same current_thread Tokio worker.
    let lock_path = tmp_dir.join(".cordis.yml.lock");
    std::fs::write(&lock_path, "held-by-test").unwrap();

    let lock_released_on_worker = Arc::new(AtomicBool::new(false));
    let flag = lock_released_on_worker.clone();
    let lock_path_c = lock_path.clone();
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        flag.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(Path::new(&lock_path_c));
    });

    fiber
        .update(serde_json::json!({ "v": 2 }))
        .await
        .expect("fiber update");
    releaser.await.unwrap();

    assert!(
        lock_released_on_worker.load(Ordering::SeqCst),
        "async releaser task never ran during persist()"
    );
    assert!(
        state.persisted_state_errors().is_empty(),
        "persist() timed out because it blocked the Tokio worker thread: {:?}",
        state.persisted_state_errors()
    );
    let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        on_disk.contains("v: 2"),
        "expected updated config on disk, got:\n{on_disk}"
    );

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// Problem 3: Per-App policies must live on Store (no raw-pointer `Arc::as_ptr(&ctx.store) as usize` global maps).
#[test]
fn per_app_policies_do_not_use_raw_pointer_global_tables() {
    let policy_rs = include_str!("../src/policy.rs");
    assert!(
        !policy_rs.contains("Arc::as_ptr(&ctx.store)"),
        "policy.rs must store per-App policies on Store, not in raw-pointer HashMap<usize, _>"
    );
}

#[tokio::test]
async fn per_app_policies_are_isolated_across_concurrent_apps() {
    use cordis_host::{
        method_allow_list_for, set_dispatch_policy_for, set_host_limits_for,
        set_method_allow_list_for, set_startup_policy_for, startup_policy_for, DispatchPolicy,
        HostLimits, MethodAllowList, StartupPolicy,
    };

    let app_a = App::new();
    let app_b = App::new();

    set_dispatch_policy_for(&app_a.root, DispatchPolicy::new(Some(111), Some(222)));
    set_dispatch_policy_for(&app_b.root, DispatchPolicy::new(Some(333), Some(444)));
    set_host_limits_for(&app_a.root, HostLimits::new(10, 20, 30, 40).unwrap()).unwrap();
    set_host_limits_for(&app_b.root, HostLimits::new(50, 60, 70, 80).unwrap()).unwrap();

    let sp_a = StartupPolicy::default();
    set_startup_policy_for(&app_a.root, Some(sp_a.clone()));
    assert_eq!(startup_policy_for(&app_a.root), Some(sp_a));
    assert_eq!(startup_policy_for(&app_b.root), None);

    let ml_a = MethodAllowList::new().allow_row_method("r1", "svc@1", "ping");
    set_method_allow_list_for(&app_a.root, Some(ml_a.clone()));
    assert_eq!(method_allow_list_for(&app_a.root), Some(ml_a));
    assert_eq!(method_allow_list_for(&app_b.root), None);

    set_startup_policy_for(&app_a.root, None);
    set_method_allow_list_for(&app_a.root, None);
    assert_eq!(startup_policy_for(&app_a.root), None);
    assert_eq!(method_allow_list_for(&app_a.root), None);

    app_a.close().await;
    app_b.close().await;
}

// ============================================================================
// Round-2 Review TDD Regression Tests (Host Loader)
// ============================================================================

// R2 P1: Plugin self-dispose (`mark_self_disposed`) must NOT block the Tokio worker thread
// while acquiring `config_gate` / `ConfigLock` and writing `disabled: true` to disk.
#[tokio::test]
async fn r2_p1_self_dispose_persist_does_not_block_tokio_worker_when_lock_is_held() {
    use cordis_core::{plugin_fn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct DummySource;
    impl PluginSource for DummySource {
        fn load(
            &self,
            _entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            Ok(LoadedFile {
                plugin: plugin_fn("suicidal", Inject::default(), |_ctx, _cfg| {
                    Box::pin(async { Ok(()) })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "cordis-self-dispose-nonblocking-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(
        &cfg_path,
        "- id: suicidal\n  name: suicidal\n  config:\n    v: 1\n",
    )
    .unwrap();

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(DummySource),
    )
    .await
    .expect("load_with_source");
    let fiber = state.fiber_of("suicidal").expect("suicidal fiber");

    // Pre-create `.cordis.yml.lock` so `ConfigLock::acquire` must wait until released by a
    // concurrent async task on THIS same `current_thread` Tokio worker.
    let lock_path = tmp_dir.join(".cordis.yml.lock");
    std::fs::write(&lock_path, "held-by-test").unwrap();

    let lock_released_on_worker = Arc::new(AtomicBool::new(false));
    let flag = lock_released_on_worker.clone();
    let lock_path_c = lock_path.clone();
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        flag.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(Path::new(&lock_path_c));
    });

    fiber.dispose().await;
    releaser.await.unwrap();

    assert!(
        lock_released_on_worker.load(Ordering::SeqCst),
        "async releaser task never ran during self-dispose persist()"
    );
    assert!(
        state.persisted_state_errors().is_empty(),
        "self-dispose persist() timed out because it blocked the Tokio worker thread: {:?}",
        state.persisted_state_errors()
    );
    let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        on_disk.contains("disabled: true"),
        "fiber.dispose().await 返回时 disabled: true 必须已落盘，实际：\n{on_disk}"
    );

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// R2 P2: `LoaderState::capabilities()` must key `!!js` capabilities by `stable_label`
// (`fake#0` vs `fake#1`) rather than bare `name`, so same-name anonymous rows don't overwrite each other.
#[tokio::test]
async fn r2_p2_capabilities_distinguishes_anonymous_same_name_rows_by_stable_label() {
    use cordis_core::{plugin_fn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::Arc;

    struct DummySource;
    impl PluginSource for DummySource {
        fn load(
            &self,
            _entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            Ok(LoadedFile {
                plugin: plugin_fn("fake", Inject::default(), |_ctx, _cfg| {
                    Box::pin(async { Ok(()) })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "cordis-r2-caps-stable-label-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(
        &cfg_path,
        concat!(
            "- name: fake\n",
            "  config:\n",
            "    a: !!js process.env.R2_CAP_ONLY_A\n",
            "- name: fake\n",
            "  config:\n",
            "    b: !!js process.env.R2_CAP_ONLY_B\n",
        ),
    )
    .unwrap();

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(DummySource),
    )
    .await
    .expect("load_with_source");

    let caps = state.capabilities();
    let row0 = caps
        .iter()
        .find(|row| row["label"] == "fake#0")
        .expect("fake#0 present");
    let row1 = caps
        .iter()
        .find(|row| row["label"] == "fake#1")
        .expect("fake#1 present");

    assert_eq!(
        row0["js"]["env"],
        serde_json::json!(["R2_CAP_ONLY_A"]),
        "fake#0 必须报告自己的 !!js env 能力，而不是被 fake#1 覆盖：{caps:?}"
    );
    assert_eq!(
        row1["js"]["env"],
        serde_json::json!(["R2_CAP_ONLY_B"]),
        "fake#1 必须报告自己的 !!js env 能力：{caps:?}"
    );

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// R2 P2: `reload_one_report` recovery and `set_disabled` rollback must clear stale
// `state.last.failures` entries so `loader.report["failed"]` doesn't report resolved/rolled-back errors.
#[tokio::test]
async fn r2_p2_reload_recovery_and_set_disabled_rollback_clear_stale_report_failed() {
    use cordis_core::{plugin_fn, Inject};
    use cordis_host::loader::{
        load_with_source, reload_one_with_plugin, LoadedFile, LoaderConfig, PluginSource,
        ReloadStatus,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct ToggleSource {
        fail_bad: Arc<AtomicBool>,
        fail_flakey: Arc<AtomicBool>,
    }
    impl PluginSource for ToggleSource {
        fn load(
            &self,
            entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            if entry.name == "bad" && self.fail_bad.load(Ordering::SeqCst) {
                return Err("bad load failed".to_string());
            }
            if entry.name == "flakey-v2" && self.fail_flakey.load(Ordering::SeqCst) {
                return Err("flakey-v2 load failed".to_string());
            }
            let pname: &'static str = if entry.name == "bad" { "bad" } else { "flakey" };
            Ok(LoadedFile {
                plugin: plugin_fn(pname, Inject::default(), |_ctx, _cfg| {
                    Box::pin(async { Ok(()) })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir =
        std::env::temp_dir().join(format!("cordis-r2-stale-failures-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(
        &cfg_path,
        concat!(
            "- id: bad\n  name: bad\n  disabled: true\n",
            "- id: flakey\n  name: flakey\n",
        ),
    )
    .unwrap();

    let fail_bad = Arc::new(AtomicBool::new(true));
    let fail_flakey = Arc::new(AtomicBool::new(true));
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(ToggleSource {
            fail_bad: fail_bad.clone(),
            fail_flakey: fail_flakey.clone(),
        }),
    )
    .await
    .expect("initial load");
    assert_eq!(state.report()["failed"], serde_json::json!([]));

    // Subcase A: `set_disabled("bad", Some(false))` fails and rolls `bad` back to disabled.
    // `report()["failed"]` must NOT retain stale `"bad: bad load failed"`.
    let err = state
        .set_disabled("bad", Some(false))
        .await
        .expect_err("enabling failing row must fail and roll back");
    assert!(err.contains("bad load failed"), "{err}");
    assert_eq!(
        state.report()["failed"],
        serde_json::json!([]),
        "set_disabled 回滚后 report.failed 不得残留已回滚行的失败记录"
    );

    // Subcase B: A failed reconcile replaces `flakey` with `flakey-v2` (populates `last.failures`),
    // then `reload_one_report` succeeds on `flakey` -> must clear `"flakey: ..."` from `report.failed`.
    let bad_entries = cordis_host::patch::parse_entries(concat!(
        "- id: bad\n  name: bad\n  disabled: true\n",
        "- id: flakey\n  name: flakey-v2\n",
    ))
    .unwrap();
    let outcome = state.reconcile_once(bad_entries).await;
    assert!(
        outcome.failures.iter().any(|f| f.starts_with("flakey: ")),
        "reconcile failure expected: {:?}",
        outcome.failures
    );
    let fresh_plugin = plugin_fn("flakey", Inject::default(), |_ctx, _cfg| {
        Box::pin(async { Ok(()) })
    });
    let reload = reload_one_with_plugin(&app.root, &state, "flakey", fresh_plugin).await;
    assert_eq!(reload.status, ReloadStatus::Active);
    let failed_after = state.report()["failed"].clone();
    assert!(
        !failed_after
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.starts_with("flakey: "))),
        "reload 恢复成功后 report.failed 不得残留陈旧的 flakey 失败：{failed_after}"
    );

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
