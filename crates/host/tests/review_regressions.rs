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

// R3 P1-8: Self-disposing plugin with async disposer must NOT be resurrected by concurrent `sync()`
// before `persist_self_disposed` finishes writing `disabled: true` to `tree` and disk.
#[tokio::test]
async fn r3_p1_self_dispose_blocks_resurrection_during_async_teardown() {
    use cordis_core::{plugin_fn, Disposable, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct SelfDisposeSource {
        mount_count: Arc<AtomicUsize>,
        disposing: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl PluginSource for SelfDisposeSource {
        fn load(
            &self,
            _entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let mount_count = self.mount_count.clone();
            let disposing = self.disposing.clone();
            let release = self.release.clone();
            Ok(LoadedFile {
                plugin: plugin_fn("self-disp", Inject::default(), move |ctx, _cfg| {
                    let mount_count = mount_count.clone();
                    let disposing = disposing.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        mount_count.fetch_add(1, Ordering::SeqCst);
                        let _ = ctx.effect(move |_| {
                            let disposing = disposing.clone();
                            let release = release.clone();
                            Disposable::new(move || {
                                Box::pin(async move {
                                    disposing.notify_one();
                                    release.notified().await;
                                })
                            })
                        });
                        Ok(())
                    })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "cordis-r3-self-dispose-resurrect-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(&cfg_path, "- id: victim\n  name: self-disp\n").unwrap();

    let mount_count = Arc::new(AtomicUsize::new(0));
    let disposing = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(SelfDisposeSource {
            mount_count: mount_count.clone(),
            disposing: disposing.clone(),
            release: release.clone(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(mount_count.load(Ordering::SeqCst), 1);

    let fiber = state.fiber_of("victim").expect("victim mounted");
    let dispose_task = tokio::spawn(async move {
        fiber.dispose().await;
    });
    disposing.notified().await;

    // While `victim` is mid-teardown (after `enqueue_self_dispose`, before `persist_self_disposed`),
    // run a concurrent `state.sync().await` -> must NOT resurrect `victim`.
    let _ = state.sync().await;
    assert_eq!(
        mount_count.load(Ordering::SeqCst),
        1,
        "自我卸载异步收尾期间并发 sync 不得把该行重新复活成第二份实例"
    );
    assert!(
        state.fiber_of("victim").is_none(),
        "自我卸载期间 mounted 不得出现被复活的实例"
    );

    release.notify_one();
    dispose_task.await.unwrap();
    assert!(state.fiber_of("victim").is_none());
    let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        on_disk.contains("disabled: true"),
        "自我卸载落盘后配置必须为 disabled: true: {on_disk}"
    );
    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// R3 P2-12: `set_disabled` rollback when enabling a group fails partially must dispose
// any children newly mounted before the failure (no live orphan fibers).
#[tokio::test]
async fn r3_p2_set_disabled_rollback_disposes_newly_mounted_rows() {
    use cordis_core::{plugin_fn, Dyn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::Arc;

    struct PartialGroupSource;
    impl PluginSource for PartialGroupSource {
        fn load(
            &self,
            entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            if entry.name == "bad" {
                return Err("bad child fails".to_string());
            }
            Ok(LoadedFile {
                plugin: plugin_fn("ok-child", Inject::default(), |ctx, _cfg| {
                    Box::pin(async move {
                        ctx.provide("ok-child-svc", Dyn::new(1u32))?;
                        Ok(())
                    })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "cordis-r3-set-disabled-orphan-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(
        &cfg_path,
        concat!(
            "- id: grp\n",
            "  group: true\n",
            "  disabled: true\n",
            "  config:\n",
            "    - id: ok_child\n",
            "      name: ok\n",
            "    - id: bad_child\n",
            "      name: bad\n",
        ),
    )
    .unwrap();

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(PartialGroupSource),
    )
    .await
    .unwrap();
    assert!(state.mounted_labels().is_empty());

    let res = state.set_disabled("grp", Some(false)).await;
    assert!(res.is_err(), "组内子行失败应导致 set_disabled 返回 Err");
    assert!(
        state.mounted_labels().is_empty(),
        "set_disabled 回滚必须同时卸载刚挂载成功的子行，不得遗留活实例: {:?}",
        state.mounted_labels()
    );
    assert!(
        app.root.get("ok-child-svc").is_none(),
        "被回滚子行提供的服务必须已回收"
    );

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// R3 P3-11: `LoaderState::capabilities()` must keep first-row JS capabilities when duplicate
// labels exist under `strict: false`, and not advance anonymous ordinals into duplicate groups.
#[tokio::test]
async fn r3_p3_capabilities_keeps_first_row_on_duplicate_id() {
    use cordis_core::{plugin_fn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::Arc;

    struct SimpleSource;
    impl PluginSource for SimpleSource {
        fn load(
            &self,
            _entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            Ok(LoadedFile {
                plugin: plugin_fn("dup-p", Inject::default(), |_ctx, _cfg| {
                    Box::pin(async { Ok(()) })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir = std::env::temp_dir().join(format!("cordis-r3-dup-caps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(
        &cfg_path,
        concat!(
            "- id: dup\n",
            "  name: dup-p\n",
            "  config:\n",
            "    secret: !!js process.env.R3_FIRST_ONLY\n",
            "- id: dup\n",
            "  name: dup-p\n",
            "  config: {}\n",
        ),
    )
    .unwrap();

    let app = App::new();
    let mut cfg = LoaderConfig::new(cfg_path.to_string_lossy().into_owned());
    cfg.strict = false;
    let state = load_with_source(app.root.clone(), cfg, Arc::new(SimpleSource))
        .await
        .unwrap();
    let caps = state.capabilities();
    let dup = caps
        .iter()
        .find(|row| row["label"] == "dup")
        .expect("dup mounted");
    assert_eq!(
        dup["js"]["env"],
        serde_json::json!(["R3_FIRST_ONLY"]),
        "同名重行下 capabilities 必须遵循首行生效原则，不得被跳过的第二行空配置覆盖：{caps:?}"
    );
    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// ============================================================================
// Round-4 P1 structural closure tests (P1-11, P1-12, P1-13, P1-14)
// ============================================================================

// R4 P1-11: Cancelling `mount_loaded` mid-`settle().await` must dispose the orphan fiber
// and track `Draining` until its cleanup finishes (never leaving a live untracked orphan).
#[tokio::test]
async fn r4_p1_mount_loaded_cancel_disposes_orphan_and_tracks_draining() {
    use cordis_core::{plugin_fn, Disposable, Dyn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct SlowMountSource {
        mounts: Arc<AtomicUsize>,
        entered_apply: Arc<tokio::sync::Notify>,
        release_apply: Arc<tokio::sync::Notify>,
        entered_dispose: Arc<tokio::sync::Notify>,
        release_dispose: Arc<tokio::sync::Notify>,
        gate_first: Arc<AtomicBool>,
    }
    impl PluginSource for SlowMountSource {
        fn load(
            &self,
            _entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let mounts = self.mounts.clone();
            let entered_apply = self.entered_apply.clone();
            let release_apply = self.release_apply.clone();
            let entered_dispose = self.entered_dispose.clone();
            let release_dispose = self.release_dispose.clone();
            let gated = self.gate_first.swap(false, Ordering::SeqCst);
            Ok(LoadedFile {
                plugin: plugin_fn("slow-mount", Inject::default(), move |ctx, _cfg| {
                    let mounts = mounts.clone();
                    let entered_apply = entered_apply.clone();
                    let release_apply = release_apply.clone();
                    let entered_dispose = entered_dispose.clone();
                    let release_dispose = release_dispose.clone();
                    Box::pin(async move {
                        let id = mounts.fetch_add(1, Ordering::SeqCst) + 1;
                        if gated {
                            let _ = ctx.effect(move |_| {
                                let entered_dispose = entered_dispose.clone();
                                let release_dispose = release_dispose.clone();
                                Disposable::new(move || {
                                    Box::pin(async move {
                                        entered_dispose.notify_one();
                                        release_dispose.notified().await;
                                    })
                                })
                            });
                            entered_apply.notify_one();
                            release_apply.notified().await;
                        }
                        ctx.provide("r4-slow-mount-svc", Dyn::new(id))?;
                        Ok(())
                    })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir =
        std::env::temp_dir().join(format!("cordis-r4-mount-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(&cfg_path, "[]\n").unwrap();

    let mounts = Arc::new(AtomicUsize::new(0));
    let entered_apply = Arc::new(tokio::sync::Notify::new());
    let release_apply = Arc::new(tokio::sync::Notify::new());
    let entered_dispose = Arc::new(tokio::sync::Notify::new());
    let release_dispose = Arc::new(tokio::sync::Notify::new());
    let gate_first = Arc::new(AtomicBool::new(true));

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(SlowMountSource {
            mounts: mounts.clone(),
            entered_apply: entered_apply.clone(),
            release_apply: release_apply.clone(),
            entered_dispose: entered_dispose.clone(),
            release_dispose: release_dispose.clone(),
            gate_first: gate_first.clone(),
        }),
    )
    .await
    .unwrap();

    let entries = cordis_host::patch::parse_entries("- id: slow\n  name: slow-mount\n").unwrap();
    let reconcile_state = state.clone();
    let reconcile_entries = entries.clone();
    let handle = tokio::spawn(async move {
        let _ = reconcile_state.reconcile_once(reconcile_entries).await;
    });
    tokio::time::timeout(Duration::from_secs(5), entered_apply.notified())
        .await
        .expect("entered slow apply");

    // Abort reconcile mid-`settle().await`!
    handle.abort();
    let _ = handle.await;

    // Release apply so the fiber can finish or enter dispose.
    release_apply.notify_waiters();

    // The cancelled fiber MUST be disposed (entering its disposer).
    tokio::time::timeout(Duration::from_secs(5), entered_dispose.notified())
        .await
        .expect("被取消的 mount_loaded 必须自动 dispose 孤儿 fiber，绝不得让其继续存活");

    // While the cancelled fiber's disposer is still running, a concurrent `sync()` MUST see `Draining`
    // and refuse to double-mount!
    let blocked = state.sync().await;
    assert!(
        blocked
            .failures
            .iter()
            .any(|msg| msg.contains("后台清理") || msg.contains("清理")),
        "取消清理尚未完成前 sync 必须被 Draining 阻断，实际: {blocked:?}"
    );
    assert_eq!(
        mounts.load(Ordering::SeqCst),
        1,
        "Draining 期间不得提前挂入第二份实例"
    );

    // Finish background dispose -> next `sync()` must converge cleanly to a single healthy instance!
    release_dispose.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let outcome = state.sync().await;
        if outcome.failures.is_empty() && state.fiber_of("slow").is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "后台清理完成后 sync 必须恢复挂载，最后结果: {outcome:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(mounts.load(Ordering::SeqCst), 2);
    assert!(app.root.get("r4-slow-mount-svc").is_some());

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// R4 P1-12 (replace path): If a mounted plugin self-disposes while `exec_replace_action` is
// preloading the replacement plugin, `exec_replace_action` must NOT mount the replacement
// concurrently with the self-disposing fiber or resurrect the self-disposed row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_p1_dispose_self_race_with_replace_never_resurrects() {
    use cordis_core::{plugin_fn, Disposable, Dyn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct RaceReplaceSource {
        v2_mounts: Arc<AtomicUsize>,
        v1_fiber: Arc<Mutex<Option<Arc<cordis_core::Fiber>>>>,
        entered_self_dispose: Arc<std::sync::Condvar>,
        self_dispose_flag: Arc<Mutex<bool>>,
        release_self_dispose: Arc<tokio::sync::Notify>,
    }
    impl PluginSource for RaceReplaceSource {
        fn load(
            &self,
            entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            if entry.name == "v1" {
                let v1_fiber = self.v1_fiber.clone();
                let entered_cv = self.entered_self_dispose.clone();
                let flag = self.self_dispose_flag.clone();
                let release = self.release_self_dispose.clone();
                Ok(LoadedFile {
                    plugin: plugin_fn("v1", Inject::default(), move |ctx, _cfg| {
                        let v1_fiber = v1_fiber.clone();
                        let entered_cv = entered_cv.clone();
                        let flag = flag.clone();
                        let release = release.clone();
                        Box::pin(async move {
                            *v1_fiber.lock().unwrap() = Some(ctx.fiber());
                            ctx.provide("r4-exclusive-svc", Dyn::new(1u32))?;
                            let _ = ctx.effect(move |_| {
                                let entered_cv = entered_cv.clone();
                                let flag = flag.clone();
                                let release = release.clone();
                                Disposable::new(move || {
                                    Box::pin(async move {
                                        *flag.lock().unwrap() = true;
                                        entered_cv.notify_all();
                                        release.notified().await;
                                    })
                                })
                            });
                            Ok(())
                        })
                    }),
                    source: std::path::PathBuf::new(),
                    staged: std::path::PathBuf::new(),
                    fingerprint: None,
                })
            } else {
                // While `exec_replace_action` is preloading `v2`, trigger `v1.dispose()` (self-dispose!)
                // and wait until `v1` has entered its async disposer (so `on_plugin` has ALREADY claimed SelfDisposing!).
                if let Some(fiber) = self.v1_fiber.lock().unwrap().clone() {
                    tokio::runtime::Handle::current().spawn(async move {
                        fiber.dispose().await;
                    });
                    let mut guard = self.self_dispose_flag.lock().unwrap();
                    while !*guard {
                        let (g, res) = self
                            .entered_self_dispose
                            .wait_timeout(guard, Duration::from_secs(5))
                            .unwrap();
                        guard = g;
                        assert!(!res.timed_out(), "v1 self-dispose should enter disposer");
                    }
                }
                let v2_mounts = self.v2_mounts.clone();
                Ok(LoadedFile {
                    plugin: plugin_fn("v2", Inject::default(), move |ctx, _cfg| {
                        let v2_mounts = v2_mounts.clone();
                        Box::pin(async move {
                            v2_mounts.fetch_add(1, Ordering::SeqCst);
                            ctx.provide("r4-exclusive-svc", Dyn::new(2u32))?;
                            Ok(())
                        })
                    }),
                    source: std::path::PathBuf::new(),
                    staged: std::path::PathBuf::new(),
                    fingerprint: None,
                })
            }
        }
    }

    let tmp_dir =
        std::env::temp_dir().join(format!("cordis-r4-replace-selfdisp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(&cfg_path, "- id: victim\n  name: v1\n").unwrap();

    let v2_mounts = Arc::new(AtomicUsize::new(0));
    let v1_fiber = Arc::new(Mutex::new(None));
    let entered_self_dispose = Arc::new(std::sync::Condvar::new());
    let self_dispose_flag = Arc::new(Mutex::new(false));
    let release_self_dispose = Arc::new(tokio::sync::Notify::new());

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(RaceReplaceSource {
            v2_mounts: v2_mounts.clone(),
            v1_fiber: v1_fiber.clone(),
            entered_self_dispose: entered_self_dispose.clone(),
            self_dispose_flag: self_dispose_flag.clone(),
            release_self_dispose: release_self_dispose.clone(),
        }),
    )
    .await
    .unwrap();
    assert!(state.fiber_of("victim").is_some());

    // Trigger Action::Replace ("v1" -> "v2"): `v1` self-disposes mid-preload!
    let replace_entries = cordis_host::patch::parse_entries("- id: victim\n  name: v2\n").unwrap();
    let _outcome = state.reconcile_once(replace_entries).await;

    assert_eq!(
        v2_mounts.load(Ordering::SeqCst),
        0,
        "v1 正在自我卸载期间，exec_replace_action 绝不得并发挂入 v2 或复活该行"
    );
    assert!(
        state.fiber_of("victim").is_none(),
        "自我卸载中的行不得被 replace 写回已挂载状态"
    );

    release_self_dispose.notify_waiters();
    tokio::time::sleep(Duration::from_millis(80)).await;
    let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        on_disk.contains("disabled: true"),
        "自我卸载完成后磁盘必须落盘 disabled: true:\n{on_disk}"
    );

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// R4 P1-13: Cancelling `request_reconcile` mid-action-loop AFTER `adopt()` already committed
// the new tree must trigger compensation convergence so remaining actions (e.g. mounting `row_b`)
// still converge automatically instead of leaving tree/runtime permanently diverged.
#[tokio::test]
async fn r4_p1_cancel_drive_pending_after_adopt_converges_via_compensation_sync() {
    use cordis_core::{plugin_fn, Disposable, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::Arc;
    use std::time::Duration;

    struct TwoRowSource {
        entered_a_dispose: Arc<tokio::sync::Notify>,
        release_a_dispose: Arc<tokio::sync::Notify>,
    }
    impl PluginSource for TwoRowSource {
        fn load(
            &self,
            entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            if entry.name == "row-a" {
                let entered = self.entered_a_dispose.clone();
                let release = self.release_a_dispose.clone();
                Ok(LoadedFile {
                    plugin: plugin_fn("row-a", Inject::default(), move |ctx, _cfg| {
                        let entered = entered.clone();
                        let release = release.clone();
                        Box::pin(async move {
                            let _ = ctx.effect(move |_| {
                                let entered = entered.clone();
                                let release = release.clone();
                                Disposable::new(move || {
                                    Box::pin(async move {
                                        entered.notify_one();
                                        release.notified().await;
                                    })
                                })
                            });
                            Ok(())
                        })
                    }),
                    source: std::path::PathBuf::new(),
                    staged: std::path::PathBuf::new(),
                    fingerprint: None,
                })
            } else {
                Ok(LoadedFile {
                    plugin: plugin_fn("row-b", Inject::default(), |_ctx, _cfg| {
                        Box::pin(async { Ok(()) })
                    }),
                    source: std::path::PathBuf::new(),
                    staged: std::path::PathBuf::new(),
                    fingerprint: None,
                })
            }
        }
    }

    let tmp_dir =
        std::env::temp_dir().join(format!("cordis-r4-adopt-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(&cfg_path, "- id: a\n  name: row-a\n").unwrap();

    let entered_a_dispose = Arc::new(tokio::sync::Notify::new());
    let release_a_dispose = Arc::new(tokio::sync::Notify::new());
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(TwoRowSource {
            entered_a_dispose: entered_a_dispose.clone(),
            release_a_dispose: release_a_dispose.clone(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(state.mounted_labels(), vec!["a"]);

    // New tree disposes `a` and mounts `b`.
    let next_entries = cordis_host::patch::parse_entries("- id: b\n  name: row-b\n").unwrap();
    let req_state = state.clone();
    let req_task = tokio::spawn(async move {
        let _ = req_state.request_reconcile(next_entries).await;
    });

    // Wait until `a`'s dispose starts — at this point `adopt()` has ALREADY committed `next_entries`!
    tokio::time::timeout(Duration::from_secs(5), entered_a_dispose.notified())
        .await
        .expect("a dispose started after adopt");

    // Cancel `request_reconcile` right before `b` gets mounted!
    req_task.abort();
    let _ = req_task.await;
    release_a_dispose.notify_waiters();

    // Compensation sync MUST automatically converge `b` without another external request!
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if state.mounted_labels() == vec!["b".to_string()] {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "adopt 后对账循环被取消必须自动触发补偿收敛，当前 mounted={:?}, tree={:?}",
            state.mounted_labels(),
            state.tree_snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// R4 P1-14: Cancelling `dispose_row` mid-`dispose_with_report().await` must keep the row in
// `Draining` until background disposal finishes, blocking concurrent remounts.
#[tokio::test]
async fn r4_p1_cancel_dispose_row_tracks_draining_until_cleanup_finishes() {
    use cordis_core::{plugin_fn, Disposable, Dyn, Inject};
    use cordis_host::loader::{load_with_source, LoadedFile, LoaderConfig, PluginSource};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct SlowDisposeSource {
        mounts: Arc<AtomicUsize>,
        entered_dispose: Arc<tokio::sync::Notify>,
        release_dispose: Arc<tokio::sync::Notify>,
    }
    impl PluginSource for SlowDisposeSource {
        fn load(
            &self,
            _entry: &cordis_host::patch::Entry,
            _search: &[std::path::PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let mounts = self.mounts.clone();
            let entered = self.entered_dispose.clone();
            let release = self.release_dispose.clone();
            Ok(LoadedFile {
                plugin: plugin_fn("slow-disp", Inject::default(), move |ctx, _cfg| {
                    let mounts = mounts.clone();
                    let entered = entered.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        let id = mounts.fetch_add(1, Ordering::SeqCst) + 1;
                        ctx.provide("r4-slow-disp-svc", Dyn::new(id))?;
                        if id == 1 {
                            let _ = ctx.effect(move |_| {
                                let entered = entered.clone();
                                let release = release.clone();
                                Disposable::new(move || {
                                    Box::pin(async move {
                                        entered.notify_one();
                                        release.notified().await;
                                    })
                                })
                            });
                        }
                        Ok(())
                    })
                }),
                source: std::path::PathBuf::new(),
                staged: std::path::PathBuf::new(),
                fingerprint: None,
            })
        }
    }

    let tmp_dir =
        std::env::temp_dir().join(format!("cordis-r4-dispose-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("cordis.yml");
    std::fs::write(&cfg_path, "- id: svc\n  name: slow-disp\n").unwrap();

    let mounts = Arc::new(AtomicUsize::new(0));
    let entered_dispose = Arc::new(tokio::sync::Notify::new());
    let release_dispose = Arc::new(tokio::sync::Notify::new());
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(SlowDisposeSource {
            mounts: mounts.clone(),
            entered_dispose: entered_dispose.clone(),
            release_dispose: release_dispose.clone(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(mounts.load(Ordering::SeqCst), 1);

    // Start disabling `svc`, then cancel mid-`dispose_with_report().await`.
    let disable_state = state.clone();
    let disable_task = tokio::spawn(async move {
        let _ = disable_state.set_disabled("svc", Some(true)).await;
    });
    tokio::time::timeout(Duration::from_secs(5), entered_dispose.notified())
        .await
        .expect("entered slow disposer");
    disable_task.abort();
    let _ = disable_task.await;

    // Immediately trying to re-enable / remount while disposer is STILL running MUST be blocked by `Draining`!
    let remount_entries =
        cordis_host::patch::parse_entries("- id: svc\n  name: slow-disp\n").unwrap();
    let blocked = state.reconcile_once(remount_entries.clone()).await;
    assert!(
        blocked
            .failures
            .iter()
            .any(|msg| msg.contains("后台清理") || msg.contains("清理")),
        "dispose_row 中途被取消时必须落入 Draining 阻断并发重挂，实际: {blocked:?}"
    );
    assert_eq!(
        mounts.load(Ordering::SeqCst),
        1,
        "旧实例清理未结束前绝不得挂入第二份实例"
    );

    // Release slow disposer -> background cleanup completes -> Draining clears -> next sync mounts cleanly!
    release_dispose.notify_waiters();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let outcome = state.reconcile_once(remount_entries.clone()).await;
        if outcome.failures.is_empty() && state.fiber_of("svc").is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "后台清理完成后应可正常重新挂载，最后结果: {outcome:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(mounts.load(Ordering::SeqCst), 2);

    app.close().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
