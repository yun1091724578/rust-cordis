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
